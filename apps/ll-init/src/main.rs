use std::env;
use std::ffi::{CStr, CString, OsString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process;
use std::ptr;
use std::time::Duration;

const CONTAINER_LOCK_PATH: &str = "/run/linglong/.lock";
const UNBLOCK_SIGNALS: [libc::c_int; 9] = [
    libc::SIGABRT,
    libc::SIGBUS,
    libc::SIGFPE,
    libc::SIGILL,
    libc::SIGSEGV,
    libc::SIGSYS,
    libc::SIGTRAP,
    libc::SIGXCPU,
    libc::SIGXFSZ,
];

fn verbose() -> bool {
    env::var_os("LINYAPS_INIT_VERBOSE_OUTPUT").is_some()
}

fn info(message: impl AsRef<str>) {
    if verbose() {
        eprintln!("{}", message.as_ref());
    }
}

fn system_error(message: impl AsRef<str>) {
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    system_error_code(message, errno);
}

fn system_error_code(message: impl AsRef<str>, errno: libc::c_int) {
    let description = unsafe { CStr::from_ptr(libc::strerror(errno)) }.to_string_lossy();
    eprintln!("{}: {description}", message.as_ref());
}

fn signal_name(signal: libc::c_int) -> String {
    let name = unsafe { libc::strsignal(signal) };
    if name.is_null() {
        return signal.to_string();
    }
    unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned()
}

fn child_status(status: libc::c_int, pid: libc::pid_t) -> libc::c_int {
    if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        info(format!("child {pid} exited with status {code}"));
        return code;
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        info(format!("child {pid} exited with signal {signal}"));
        return signal + 128;
    }
    info(format!("child {pid} exited with unknown status {status}"));
    -1
}

#[derive(Debug)]
struct OwnedFd(RawFd);

impl OwnedFd {
    fn new(fd: RawFd) -> io::Result<Self> {
        if fd == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(fd))
        }
    }

    fn raw(&self) -> RawFd {
        self.0
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

struct SignalMask {
    blocked: libc::sigset_t,
    original: libc::sigset_t,
}

impl SignalMask {
    fn install() -> io::Result<Self> {
        unsafe {
            let mut blocked = MaybeUninit::<libc::sigset_t>::zeroed().assume_init();
            let mut original = MaybeUninit::<libc::sigset_t>::zeroed().assume_init();
            if libc::sigfillset(&mut blocked) == -1 {
                return Err(io::Error::last_os_error());
            }
            for signal in UNBLOCK_SIGNALS {
                if libc::sigdelset(&mut blocked, signal) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::sigprocmask(libc::SIG_SETMASK, &blocked, &mut original) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { blocked, original })
        }
    }

    fn restore(&self) -> io::Result<()> {
        let result =
            unsafe { libc::sigprocmask(libc::SIG_SETMASK, &self.original, ptr::null_mut()) };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for SignalMask {
    fn drop(&mut self) {
        unsafe {
            libc::sigprocmask(libc::SIG_SETMASK, &self.original, ptr::null_mut());
        }
    }
}

fn c_arguments(arguments: &[OsString]) -> io::Result<(Vec<CString>, Vec<*const libc::c_char>)> {
    let strings = arguments
        .iter()
        .map(|argument| {
            CString::new(argument.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argument contains NUL"))
        })
        .collect::<io::Result<Vec<_>>>()?;
    let mut pointers = strings
        .iter()
        .map(|value| value.as_ptr())
        .collect::<Vec<_>>();
    pointers.push(ptr::null());
    Ok((strings, pointers))
}

fn exec(arguments: &[OsString], error_message: &str) -> ! {
    let Ok((_strings, pointers)) = c_arguments(arguments) else {
        eprintln!("failed to run process: argument contains NUL");
        unsafe { libc::_exit(libc::EXIT_FAILURE) }
    };
    unsafe {
        libc::execvp(pointers[0], pointers.as_ptr());
    }
    system_error(error_message);
    unsafe { libc::_exit(libc::EXIT_FAILURE) }
}

fn wait_for(pid: libc::pid_t) -> libc::c_int {
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited == pid {
            info("delegate done");
            return child_status(status, pid);
        }
        if waited == -1 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        system_error("failed to wait for delegated child");
        return -1;
    }
}

fn delegate_run(arguments: &[OsString]) -> libc::c_int {
    info("delegate run");
    let tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
    let tty_fd = if tty {
        let fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
        match OwnedFd::new(fd) {
            Ok(fd) => Some(fd),
            Err(_) => {
                system_error("failed dup tty");
                return -1;
            }
        }
    } else {
        None
    };
    let child = unsafe { libc::fork() };
    if child == -1 {
        system_error("failed to fork for delegate run");
        return -1;
    }
    if child == 0 {
        if let Some(fd) = tty_fd.as_ref() {
            unsafe {
                libc::dup2(fd.raw(), libc::STDIN_FILENO);
                libc::dup2(fd.raw(), libc::STDOUT_FILENO);
                libc::dup2(fd.raw(), libc::STDERR_FILENO);
            }
        }
        exec(arguments, "failed to exec for delegate run");
    }
    drop(tty_fd);
    wait_for(child)
}

struct ChildProcess {
    pid: libc::pid_t,
    exit_code: libc::c_int,
}

impl ChildProcess {
    fn spawn(arguments: &[OsString], mask: &SignalMask) -> io::Result<Self> {
        let child = unsafe { libc::fork() };
        if child == -1 {
            return Err(io::Error::last_os_error());
        }
        if child == 0 {
            if unsafe { libc::setpgid(0, 0) } == -1 {
                system_error("failed to set process group");
                unsafe { libc::_exit(libc::EXIT_FAILURE) }
            }
            if unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpid()) } == -1
                && io::Error::last_os_error().raw_os_error() != Some(libc::ENOTTY)
            {
                system_error("failed to set terminal process group");
                unsafe { libc::_exit(libc::EXIT_FAILURE) }
            }
            if mask.restore().is_err() {
                system_error("failed to restore signal mask");
                unsafe { libc::_exit(libc::EXIT_FAILURE) }
            }
            exec(arguments, "failed to run process");
        }
        info(format!("run child {child}"));
        Ok(Self {
            pid: child,
            exit_code: 0,
        })
    }

    fn forward_signal(&self, signal: libc::c_int) {
        if self.pid >= 0 {
            if unsafe { libc::kill(self.pid, signal) } == -1 {
                system_error(format!("failed to forward signal {}", signal_name(signal)));
            }
            return;
        }
        if unsafe { libc::kill(-1, signal) } == -1
            && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        {
            system_error(format!(
                "failed to forward signal {} to process group",
                signal_name(signal)
            ));
        }
    }

    fn reap_pending(&mut self) {
        loop {
            let mut status = 0;
            let waited = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if waited == 0 {
                return;
            }
            if waited == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ECHILD) {
                    return;
                }
                system_error_code(
                    "failed to wait for child",
                    error.raw_os_error().unwrap_or(0),
                );
                return;
            }
            let code = child_status(status, waited);
            if waited == self.pid {
                self.pid = -1;
                self.exit_code = code;
            }
        }
    }

    fn has_exited(&self) -> bool {
        self.pid == -1
    }
}

fn has_children_or_zombies() -> bool {
    loop {
        let mut status = 0;
        let waited = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if waited == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                return false;
            }
            system_error_code("waitpid failed", error.raw_os_error().unwrap_or(0));
            return true;
        }
        if waited > 0 {
            child_status(status, waited);
        }
        return true;
    }
}

enum LockState {
    Skipped,
    Active(OwnedFd),
}

struct ContainerLock {
    state: LockState,
}

impl ContainerLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let exists = match path.try_exists() {
            Ok(exists) => exists,
            Err(error) => {
                system_error_code(
                    "failed to detect lock exists or not, maybe container state has already broken",
                    error.raw_os_error().unwrap_or(0),
                );
                false
            }
        };
        let skip = env::var_os("LINYAPS_INIT_SKIP_LOCK")
            .is_some_and(|value| value.as_os_str().as_bytes() == b"YES")
            || !exists;
        if skip {
            info("skipping container lock");
            return Ok(Self {
                state: LockState::Skipped,
            });
        }
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "lock path contains NUL"))?;
        let fd = match OwnedFd::new(unsafe {
            libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
        }) {
            Ok(fd) => fd,
            Err(error) => {
                system_error_code(
                    format!("failed to open lock file {}", path.to_string_lossy()),
                    error.raw_os_error().unwrap_or(0),
                );
                return Err(error);
            }
        };
        if let Err(error) = set_lock(fd.raw(), true, libc::F_WRLCK as libc::c_short) {
            system_error_code(
                "failed to lock container lock during initializing",
                error.raw_os_error().unwrap_or(0),
            );
            return Err(error);
        }
        let content = match read_fd(fd.raw()) {
            Ok(content) => content,
            Err(error) => {
                system_error_code(
                    format!("failed to read from fd {}", fd.raw()),
                    error.raw_os_error().unwrap_or(0),
                );
                return Err(error);
            }
        };
        if content.is_empty() {
            return Err(io::Error::other("empty container lock state"));
        }
        if content != b"initializing" {
            info(format!(
                "container is not in expected initializing state, current state: {}",
                String::from_utf8_lossy(&content)
            ));
            return Err(io::Error::other("unexpected container lock state"));
        }
        Ok(Self {
            state: LockState::Active(fd),
        })
    }

    fn transition_to_running(&self) -> bool {
        let LockState::Active(fd) = &self.state else {
            return true;
        };
        if overwrite_fd(fd.raw(), b"running").is_err() {
            info("failed to update lock state");
            return false;
        }
        if let Err(error) = unlock(fd.raw()) {
            system_error_code(
                "failed to unlock lock file",
                error.raw_os_error().unwrap_or(0),
            );
            return false;
        }
        true
    }

    fn transition_to_quitting(&self) -> bool {
        let LockState::Active(fd) = &self.state else {
            return true;
        };
        match set_lock(fd.raw(), false, libc::F_WRLCK as libc::c_short) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EAGAIN) | Some(libc::EACCES)
                ) =>
            {
                return false;
            }
            Err(error) => {
                system_error_code(
                    "failed to lock container lock during exiting",
                    error.raw_os_error().unwrap_or(0),
                );
                return false;
            }
        }
        if overwrite_fd(fd.raw(), b"quitting").is_err() {
            info("failed to update lock state");
        }
        if let Err(error) = unlock(fd.raw()) {
            system_error_code(
                "failed to unlock container lock",
                error.raw_os_error().unwrap_or(0),
            );
        }
        true
    }
}

fn set_lock(fd: RawFd, blocked: bool, lock_type: libc::c_short) -> io::Result<()> {
    let mut lock = unsafe { MaybeUninit::<libc::flock>::zeroed().assume_init() };
    lock.l_type = lock_type;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    let command = if blocked {
        libc::F_SETLKW
    } else {
        libc::F_SETLK
    };
    if unsafe { libc::fcntl(fd, command, &lock) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn unlock(fd: RawFd) -> io::Result<()> {
    set_lock(fd, false, libc::F_UNLCK as libc::c_short)
}

fn read_fd(fd: RawFd) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 32];
    loop {
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read == 0 {
            return Ok(output);
        }
        if read == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        output.extend_from_slice(&buffer[..read as usize]);
    }
}

fn overwrite_fd(fd: RawFd, content: &[u8]) -> io::Result<()> {
    if unsafe { libc::ftruncate(fd, 0) } == -1 {
        let error = io::Error::last_os_error();
        system_error_code("failed to truncate file", error.raw_os_error().unwrap_or(0));
        return Err(error);
    }
    if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } == -1 {
        let error = io::Error::last_os_error();
        system_error_code(
            "failed to seek to beginning of file",
            error.raw_os_error().unwrap_or(0),
        );
        return Err(error);
    }
    loop {
        let result = unsafe { libc::write(fd, content.as_ptr().cast(), content.len()) };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            system_error_code("failed to write to file", error.raw_os_error().unwrap_or(0));
            return Err(error);
        }
        return Ok(());
    }
}

fn run_init(arguments: &[OsString]) -> libc::c_int {
    let mask = match SignalMask::install() {
        Ok(mask) => mask,
        Err(_) => {
            system_error("failed to set signal mask");
            return -1;
        }
    };
    let lock = match ContainerLock::acquire(Path::new(CONTAINER_LOCK_PATH)) {
        Ok(lock) => lock,
        Err(_) => return -1,
    };
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) } == -1 {
        system_error("failed to set child subreaper");
        return -1;
    }
    let mut child = match ChildProcess::spawn(arguments, &mask) {
        Ok(child) => child,
        Err(error) => {
            eprintln!("Failed to fork: {error}");
            return -1;
        }
    };
    let signal_fd = match OwnedFd::new(unsafe {
        libc::signalfd(-1, &mask.blocked, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC)
    }) {
        Ok(fd) => fd,
        Err(_) => {
            system_error("failed to create signalfd");
            return -1;
        }
    };
    if !lock.transition_to_running() {
        return -1;
    }
    let mut waiting_for_lock = false;
    loop {
        let mut descriptor = libc::pollfd {
            fd: signal_fd.raw(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = if waiting_for_lock { 1000 } else { -1 };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if ready == -1 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            system_error("failed to wait for events");
            return -1;
        }
        if ready > 0
            && descriptor.revents & libc::POLLIN != 0
            && !dispatch_signals(signal_fd.raw(), &mut child)
        {
            return -1;
        }
        if child.has_exited() && !has_children_or_zombies() {
            if lock.transition_to_quitting() {
                return child.exit_code;
            }
            waiting_for_lock = true;
        }
        if waiting_for_lock {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn dispatch_signals(fd: RawFd, child: &mut ChildProcess) -> bool {
    loop {
        let mut signal = MaybeUninit::<libc::signalfd_siginfo>::zeroed();
        let read = unsafe {
            libc::read(
                fd,
                signal.as_mut_ptr().cast(),
                std::mem::size_of::<libc::signalfd_siginfo>(),
            )
        };
        if read == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EAGAIN)
                || error.raw_os_error() == Some(libc::EWOULDBLOCK)
            {
                return true;
            }
            system_error_code(
                "failed to read from signalfd",
                error.raw_os_error().unwrap_or(0),
            );
            return false;
        }
        if read == 0 {
            return true;
        }
        if read as usize != std::mem::size_of::<libc::signalfd_siginfo>() {
            system_error_code("failed to read from signalfd", libc::EIO);
            return false;
        }
        let signal = unsafe { signal.assume_init() }.ssi_signo as libc::c_int;
        if signal == libc::SIGCHLD {
            child.reap_pending();
        } else {
            child.forward_signal(signal);
        }
    }
}

fn main() {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    let code = if unsafe { libc::getpid() } == 1 {
        run_init(&arguments)
    } else {
        delegate_run(&arguments)
    };
    process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn decodes_exit_and_signal_status() {
        assert_eq!(child_status(37 << 8, 100), 37);
        assert_eq!(child_status(libc::SIGTERM, 100), 128 + libc::SIGTERM);
    }

    #[test]
    fn lock_transitions_update_state() {
        let directory = env::temp_dir().join(format!("ll-init-lock-{}", process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("lock");
        fs::write(&path, "initializing").unwrap();
        let lock = ContainerLock::acquire(&path).unwrap();
        assert!(lock.transition_to_running());
        assert_eq!(fs::read_to_string(&path).unwrap(), "running");
        assert!(lock.transition_to_quitting());
        assert_eq!(fs::read_to_string(&path).unwrap(), "quitting");
        fs::remove_dir_all(directory).unwrap();
    }
}
