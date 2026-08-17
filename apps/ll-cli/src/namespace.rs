use std::env;
use std::ffi::{CString, OsStr, OsString, c_void};
use std::fs;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

const CAP_DAC_OVERRIDE: u32 = 1;
const CAP_SYS_ADMIN: u32 = 21;
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
const STACK_SIZE: usize = 1024 * 1024;
const CHILD_ENVIRONMENT: &str = "LINYAPS_INTERNAL_NAMESPACE_CHILD";

#[repr(C)]
struct CapabilityHeader {
    version: u32,
    pid: i32,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct CapabilityData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

struct ChildArguments {
    executable: CString,
    _arguments: Vec<CString>,
    argument_pointers: Vec<*const libc::c_char>,
    _environment: Vec<CString>,
    environment_pointers: Vec<*const libc::c_char>,
    socket: RawFd,
    uid: libc::uid_t,
    gid: libc::gid_t,
}

pub(super) fn has_effective_sys_admin() -> Result<bool, String> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("failed to read /proc/self/status: {error}"))?;
    let capabilities = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:\t"))
        .ok_or_else(|| "CapEff is missing from /proc/self/status".to_string())?;
    let capabilities = u64::from_str_radix(capabilities.trim(), 16)
        .map_err(|error| format!("failed to parse CapEff: {error}"))?;
    Ok(capabilities & (1_u64 << CAP_SYS_ADMIN) != 0)
}

pub(super) fn is_child() -> bool {
    env::var_os(CHILD_ENVIRONMENT).as_deref() == Some(OsStr::new("1"))
}

pub(super) fn run(run_context: &str) -> Result<ExitStatus, String> {
    let executable = fs::read_link("/proc/self/exe")
        .map_err(|error| format!("failed to resolve /proc/self/exe: {error}"))?;
    let mut arguments = env::args_os().collect::<Vec<_>>();
    let run_position = arguments
        .iter()
        .position(|argument| argument == OsStr::new("run"))
        .ok_or_else(|| "failed to locate run subcommand".to_string())?;
    arguments[0] = executable.clone().into_os_string();
    arguments.splice(
        run_position + 1..run_position + 1,
        [OsString::from("--run-context"), OsString::from(run_context)],
    );

    let executable = c_string(executable.as_os_str())?;
    let arguments = arguments
        .iter()
        .map(|argument| c_string(argument))
        .collect::<Result<Vec<_>, _>>()?;
    let mut argument_pointers = arguments
        .iter()
        .map(|argument| argument.as_ptr())
        .collect::<Vec<_>>();
    argument_pointers.push(std::ptr::null());
    let mut environment = env::vars_os()
        .filter(|(name, _)| name != OsStr::new(CHILD_ENVIRONMENT))
        .map(|(name, value)| {
            let mut entry = name;
            entry.push("=");
            entry.push(value);
            c_string(&entry)
        })
        .collect::<Result<Vec<_>, _>>()?;
    environment.push(
        CString::new(format!("{CHILD_ENVIRONMENT}=1"))
            .expect("internal environment variable has no NUL byte"),
    );
    let mut environment_pointers = environment
        .iter()
        .map(|entry| entry.as_ptr())
        .collect::<Vec<_>>();
    environment_pointers.push(std::ptr::null());

    let mut sockets = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            sockets.as_mut_ptr(),
        )
    } != 0
    {
        return Err(format!(
            "failed to create namespace socket: {}",
            io::Error::last_os_error()
        ));
    }

    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let mut child_arguments = Box::new(ChildArguments {
        executable,
        _arguments: arguments,
        argument_pointers,
        _environment: environment,
        environment_pointers,
        socket: sockets[1],
        uid,
        gid,
    });
    let stack_words = STACK_SIZE.div_ceil(std::mem::size_of::<usize>());
    let mut stack = vec![0_usize; stack_words];
    let stack_top = unsafe { stack.as_mut_ptr().add(stack.len()).cast::<c_void>() };
    let pid = unsafe {
        libc::clone(
            namespace_child,
            stack_top,
            libc::CLONE_NEWNS | libc::CLONE_NEWUSER | libc::SIGCHLD,
            (&mut *child_arguments as *mut ChildArguments).cast::<c_void>(),
        )
    };
    close_fd(sockets[1]);
    if pid < 0 {
        close_fd(sockets[0]);
        return Err(format!(
            "failed to create user namespace: {}",
            io::Error::last_os_error()
        ));
    }

    let setup = (|| {
        read_byte(sockets[0])
            .map_err(|error| format!("namespace child failed to start: {error}"))?;
        fs::write(format!("/proc/{pid}/uid_map"), format!("{uid} {uid} 1\n"))
            .map_err(|error| format!("failed to map namespace uid: {error}"))?;
        fs::write(format!("/proc/{pid}/setgroups"), "deny\n")
            .map_err(|error| format!("failed to disable namespace setgroups: {error}"))?;
        fs::write(format!("/proc/{pid}/gid_map"), format!("{gid} {gid} 1\n"))
            .map_err(|error| format!("failed to map namespace gid: {error}"))?;
        write_byte(sockets[0])
            .map_err(|error| format!("failed to release namespace child: {error}"))?;
        Ok::<(), String>(())
    })();
    close_fd(sockets[0]);
    let status = wait_for_child(pid)?;
    setup?;
    Ok(status)
}

extern "C" fn namespace_child(argument: *mut c_void) -> libc::c_int {
    let arguments = unsafe { &*(argument.cast::<ChildArguments>()) };
    if child_write_byte(arguments.socket).is_err() || child_read_byte(arguments.socket).is_err() {
        close_fd(arguments.socket);
        return 125;
    }
    close_fd(arguments.socket);
    if configure_capabilities(arguments.uid, arguments.gid).is_err() {
        return 126;
    }
    unsafe {
        libc::execve(
            arguments.executable.as_ptr(),
            arguments.argument_pointers.as_ptr(),
            arguments.environment_pointers.as_ptr(),
        );
    }
    127
}

fn configure_capabilities(uid: libc::uid_t, gid: libc::gid_t) -> Result<(), ()> {
    if unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) } != 0 {
        return Err(());
    }
    if unsafe { libc::setuid(uid) } != 0 || unsafe { libc::setgid(gid) } != 0 {
        return Err(());
    }
    let mask = (1_u64 << CAP_DAC_OVERRIDE) | (1_u64 << CAP_SYS_ADMIN);
    let header = CapabilityHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [
        CapabilityData {
            effective: mask as u32,
            permitted: mask as u32,
            inheritable: mask as u32,
        },
        CapabilityData {
            effective: (mask >> 32) as u32,
            permitted: (mask >> 32) as u32,
            inheritable: (mask >> 32) as u32,
        },
    ];
    if unsafe {
        libc::syscall(
            libc::SYS_capset,
            &header as *const CapabilityHeader,
            data.as_mut_ptr(),
        )
    } != 0
    {
        return Err(());
    }
    for capability in [CAP_DAC_OVERRIDE, CAP_SYS_ADMIN] {
        if unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_RAISE,
                capability,
                0,
                0,
            )
        } != 0
        {
            return Err(());
        }
    }
    Ok(())
}

fn c_string(value: &OsStr) -> Result<CString, String> {
    CString::new(value.as_bytes()).map_err(|_| {
        format!(
            "argument contains an embedded NUL byte: {}",
            value.display()
        )
    })
}

fn read_byte(fd: RawFd) -> io::Result<()> {
    let mut byte = 0_u8;
    loop {
        let count = unsafe { libc::read(fd, (&mut byte as *mut u8).cast::<c_void>(), 1) };
        if count == 1 {
            return Ok(());
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "socket closed",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn write_byte(fd: RawFd) -> io::Result<()> {
    let byte = 1_u8;
    loop {
        let count = unsafe { libc::write(fd, (&byte as *const u8).cast::<c_void>(), 1) };
        if count == 1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn child_read_byte(fd: RawFd) -> Result<(), ()> {
    let mut byte = 0_u8;
    loop {
        let count = unsafe { libc::read(fd, (&mut byte as *mut u8).cast::<c_void>(), 1) };
        if count == 1 {
            return Ok(());
        }
        if count == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(());
        }
    }
}

fn child_write_byte(fd: RawFd) -> Result<(), ()> {
    let byte = 1_u8;
    loop {
        let count = unsafe { libc::write(fd, (&byte as *const u8).cast::<c_void>(), 1) };
        if count == 1 {
            return Ok(());
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(());
        }
    }
}

fn wait_for_child(pid: libc::pid_t) -> Result<ExitStatus, String> {
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited == pid {
            return Ok(ExitStatus::from_raw(status));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(format!("failed to wait for namespace child: {error}"));
        }
    }
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_effective_capability_bit() {
        let capabilities = 1_u64 << CAP_SYS_ADMIN;
        assert_ne!(capabilities & (1_u64 << CAP_SYS_ADMIN), 0);
        assert_eq!(capabilities & (1_u64 << CAP_DAC_OVERRIDE), 0);
    }
}
