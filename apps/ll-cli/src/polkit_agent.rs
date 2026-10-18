use std::ffi::OsString;
use std::io::{self, IsTerminal, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(3);
const PKTTYAGENT: &str = "/usr/bin/pkttyagent";
const NOTIFY_FD: RawFd = 3;

pub(crate) struct TtyPolkitAgent {
    child: Child,
}

impl TtyPolkitAgent {
    pub(crate) fn start(system_bus_name: Option<&str>) -> Option<Self> {
        if !io::stdin().is_terminal() {
            return None;
        }
        let (mut readiness, child_readiness) = UnixStream::pair().ok()?;
        let child_readiness_fd = child_readiness.as_raw_fd();
        let mut command = Command::new(PKTTYAGENT);
        command
            .args(arguments(std::process::id(), system_bus_name))
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(move || duplicate_notify_fd(child_readiness_fd));
        }
        let mut child = command.spawn().ok()?;
        drop(child_readiness);
        let (sender, receiver) = mpsc::channel();
        let readiness_thread = thread::spawn(move || {
            let mut byte = [0_u8; 1];
            let _ = sender.send(readiness.read(&mut byte));
        });
        if !matches!(receiver.recv_timeout(REGISTRATION_TIMEOUT), Ok(Ok(0))) {
            stop(&mut child);
            let _ = readiness_thread.join();
            return None;
        }
        let _ = readiness_thread.join();
        match child.try_wait() {
            Ok(None) => Some(Self { child }),
            Ok(Some(_)) | Err(_) => {
                stop(&mut child);
                None
            }
        }
    }
}

impl Drop for TtyPolkitAgent {
    fn drop(&mut self) {
        stop(&mut self.child);
    }
}

fn arguments(process_id: u32, system_bus_name: Option<&str>) -> Vec<OsString> {
    let mut arguments = if let Some(system_bus_name) = system_bus_name {
        vec![
            OsString::from("--system-bus-name"),
            OsString::from(system_bus_name),
        ]
    } else {
        vec![
            OsString::from("--process"),
            OsString::from(process_id.to_string()),
        ]
    };
    arguments.extend([
        OsString::from("--notify-fd"),
        OsString::from(NOTIFY_FD.to_string()),
        OsString::from("--fallback"),
    ]);
    arguments
}

fn duplicate_notify_fd(source: RawFd) -> io::Result<()> {
    if source == NOTIFY_FD {
        let flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
        if flags == -1
            || unsafe { libc::fcntl(source, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1
        {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    if unsafe { libc::dup2(source, NOTIFY_FD) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn stop(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_for_the_calling_process() {
        assert_eq!(
            arguments(42, None),
            ["--process", "42", "--notify-fd", "3", "--fallback"].map(OsString::from)
        );
    }

    #[test]
    fn prefers_the_calling_system_bus_name() {
        assert_eq!(
            arguments(42, Some(":1.23")),
            [
                "--system-bus-name",
                ":1.23",
                "--notify-fd",
                "3",
                "--fallback",
            ]
            .map(OsString::from)
        );
    }
}
