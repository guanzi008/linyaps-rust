use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const PROCESS_STATE_ROOT_ENV: &str = "LINGLONG_PROCESS_STATE_ROOT";
pub const OCI_RUNTIME_ENV: &str = "LINGLONG_OCI_RUNTIME";
pub const DEFAULT_OCI_RUNTIME: &str = "ll-box";

pub fn oci_runtime_binary() -> OsString {
    std::env::var_os(OCI_RUNTIME_ENV)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from(DEFAULT_OCI_RUNTIME))
}

pub fn executable_path(command: &OsStr, search_paths: &[PathBuf]) -> Option<PathBuf> {
    let path = Path::new(command);
    if path.is_absolute() || path.components().count() > 1 {
        return executable(path).then(|| path.to_path_buf());
    }
    if !search_paths.is_empty() {
        return search_paths
            .iter()
            .map(|directory| directory.join(command))
            .find(|candidate| executable(candidate));
    }
    std::env::var_os("PATH")
        .unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"))
        .to_string_lossy()
        .split(':')
        .filter(|directory| !directory.is_empty())
        .map(|directory| Path::new(directory).join(command))
        .find(|candidate| executable(candidate))
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn process_state_base() -> PathBuf {
    process_state_base_from(
        std::env::var_os(PROCESS_STATE_ROOT_ENV),
        Path::new("/run/linglong").is_dir(),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
}

pub fn user_process_state_root(uid: u32) -> PathBuf {
    process_state_base().join(uid.to_string())
}

fn process_state_base_from(
    configured: Option<OsString>,
    standard_exists: bool,
    xdg_runtime: Option<OsString>,
) -> PathBuf {
    if let Some(configured) = configured.filter(|value| !value.is_empty()) {
        return PathBuf::from(configured);
    }
    if standard_exists {
        return PathBuf::from("/run/linglong");
    }
    if let Some(runtime) = xdg_runtime.filter(|value| !value.is_empty()) {
        return PathBuf::from(runtime).join("linglong/processes");
    }
    PathBuf::from("/run/linglong")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_explicit_process_state_root() {
        assert_eq!(
            process_state_base_from(
                Some(OsString::from("/custom/state")),
                true,
                Some(OsString::from("/run/user/1000")),
            ),
            Path::new("/custom/state")
        );
    }

    #[test]
    fn preserves_installed_system_path() {
        assert_eq!(
            process_state_base_from(None, true, Some(OsString::from("/run/user/1000"))),
            Path::new("/run/linglong")
        );
    }

    #[test]
    fn falls_back_to_xdg_for_uninstalled_runs() {
        assert_eq!(
            process_state_base_from(None, false, Some(OsString::from("/tmp/runtime"))),
            Path::new("/tmp/runtime/linglong/processes")
        );
    }

    #[test]
    fn resolves_executables_and_ignores_non_executable_files() {
        use std::fs;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime");
        fs::write(&path, "runtime").unwrap();
        assert_eq!(
            executable_path(OsStr::new("runtime"), &[directory.path().to_path_buf()]),
            None
        );
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        assert_eq!(
            executable_path(OsStr::new("runtime"), &[directory.path().to_path_buf()]),
            Some(path)
        );
    }
}
