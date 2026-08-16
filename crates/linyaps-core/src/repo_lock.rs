use std::ffi::OsString;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use rustix::fs::{FlockOperation, Mode, OFlags, fcntl_lock, open};

pub const REPO_LOCK_ENV: &str = "LINGLONG_REPO_LOCK";
pub const DEFAULT_REPO_LOCK_PATH: &str = "/run/linglong/lock";

static LOCAL_LOCK: (Mutex<LocalLockState>, Condvar) = (
    Mutex::new(LocalLockState {
        pid: 0,
        held: false,
    }),
    Condvar::new(),
);

struct LocalLockState {
    pid: u32,
    held: bool,
}

#[derive(Debug)]
pub struct RepoLock {
    fd: OwnedFd,
    pid: u32,
}

impl RepoLock {
    pub fn shared() -> io::Result<Self> {
        Self::shared_at(&repo_lock_path())
    }

    pub fn try_exclusive() -> io::Result<Option<Self>> {
        Self::try_exclusive_at(&repo_lock_path())
    }

    fn shared_at(path: &Path) -> io::Result<Self> {
        let pid = reserve_local_blocking();
        match open_and_lock(path, OFlags::RDONLY, FlockOperation::LockShared) {
            Ok(fd) => Ok(Self { fd, pid }),
            Err(error) => {
                release_local(pid);
                Err(error)
            }
        }
    }

    fn try_exclusive_at(path: &Path) -> io::Result<Option<Self>> {
        let Some(pid) = reserve_local_nonblocking() else {
            return Ok(None);
        };
        let fd = match open_lock_file(path, OFlags::WRONLY) {
            Ok(fd) => fd,
            Err(error) => {
                release_local(pid);
                return Err(error);
            }
        };
        match fcntl_lock(&fd, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Self { fd, pid })),
            Err(error) if lock_would_block(error) => {
                release_local(pid);
                Ok(None)
            }
            Err(error) => {
                release_local(pid);
                Err(lock_error(path, error))
            }
        }
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if current_pid() != self.pid {
            return;
        }
        let _ = fcntl_lock(&self.fd, FlockOperation::Unlock);
        release_local(self.pid);
    }
}

pub fn repo_lock_path() -> PathBuf {
    repo_lock_path_from(std::env::var_os(REPO_LOCK_ENV))
}

fn repo_lock_path_from(configured: Option<OsString>) -> PathBuf {
    configured
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_REPO_LOCK_PATH))
}

fn open_and_lock(path: &Path, flags: OFlags, operation: FlockOperation) -> io::Result<OwnedFd> {
    let fd = open_lock_file(path, flags)?;
    fcntl_lock(&fd, operation).map_err(|error| lock_error(path, error))?;
    Ok(fd)
}

fn open_lock_file(path: &Path, flags: OFlags) -> io::Result<OwnedFd> {
    open(
        path,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to open repository lock {}: {error}", path.display()),
        )
    })
}

fn lock_error(path: &Path, error: rustix::io::Errno) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("failed to lock repository {}: {error}", path.display()),
    )
}

fn lock_would_block(error: rustix::io::Errno) -> bool {
    matches!(error, rustix::io::Errno::AGAIN | rustix::io::Errno::ACCESS)
}

fn current_pid() -> u32 {
    std::process::id()
}

fn reserve_local_blocking() -> u32 {
    let pid = current_pid();
    let (mutex, available) = &LOCAL_LOCK;
    let mut state = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_after_fork(&mut state, pid);
    while state.held {
        state = available
            .wait(state)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_after_fork(&mut state, pid);
    }
    state.held = true;
    pid
}

fn reserve_local_nonblocking() -> Option<u32> {
    let pid = current_pid();
    let mut state = LOCAL_LOCK
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_after_fork(&mut state, pid);
    if state.held {
        return None;
    }
    state.held = true;
    Some(pid)
}

fn release_local(pid: u32) {
    let (mutex, available) = &LOCAL_LOCK;
    let mut state = mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    reset_after_fork(&mut state, current_pid());
    if state.pid == pid && state.held {
        state.held = false;
        available.notify_all();
    }
}

fn reset_after_fork(state: &mut LocalLockState, pid: u32) {
    if state.pid != pid {
        state.pid = pid;
        state.held = false;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn configured_path_overrides_system_default() {
        assert_eq!(
            repo_lock_path_from(Some(OsString::from("/custom/repository.lock"))),
            Path::new("/custom/repository.lock")
        );
        assert_eq!(
            repo_lock_path_from(Some(OsString::new())),
            Path::new(DEFAULT_REPO_LOCK_PATH)
        );
    }

    #[test]
    fn exclusive_lock_observes_shared_lock_from_another_process() {
        let temporary = tempdir().unwrap();
        let lock_path = temporary.path().join("repository.lock");
        let ready_path = temporary.path().join("ready");
        let release_path = temporary.path().join("release");
        fs::write(&lock_path, []).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "repo_lock::tests::subprocess_holds_shared_lock",
            ])
            .env("LINYAPS_TEST_LOCK_PATH", &lock_path)
            .env("LINYAPS_TEST_LOCK_READY", &ready_path)
            .env("LINYAPS_TEST_LOCK_RELEASE", &release_path)
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() && Instant::now() < deadline {
            assert!(
                child.try_wait().unwrap().is_none(),
                "lock holder exited early"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(ready_path.exists(), "lock holder did not become ready");
        assert!(RepoLock::try_exclusive_at(&lock_path).unwrap().is_none());

        fs::write(&release_path, []).unwrap();
        assert!(child.wait().unwrap().success());
        assert!(RepoLock::try_exclusive_at(&lock_path).unwrap().is_some());
    }

    #[test]
    #[ignore]
    fn subprocess_holds_shared_lock() {
        let Some(lock_path) = std::env::var_os("LINYAPS_TEST_LOCK_PATH") else {
            return;
        };
        let ready_path = PathBuf::from(std::env::var_os("LINYAPS_TEST_LOCK_READY").unwrap());
        let release_path = PathBuf::from(std::env::var_os("LINYAPS_TEST_LOCK_RELEASE").unwrap());
        let _lock = RepoLock::shared_at(Path::new(&lock_path)).unwrap();
        fs::write(ready_path, []).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !release_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(release_path.exists(), "parent did not release lock holder");
    }
}
