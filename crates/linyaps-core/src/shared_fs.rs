use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const SHARED_DIRECTORY_MODE: u32 = 0o755;
pub const SHARED_FILE_MODE: u32 = 0o644;

pub fn write_file_with_mode(
    path: impl AsRef<Path>,
    content: impl AsRef<[u8]>,
    mode: u32,
) -> std::io::Result<()> {
    let path = path.as_ref();
    fs::write(path, content)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

pub fn ensure_directory_with_mode(path: impl AsRef<Path>, mode: u32) -> std::io::Result<()> {
    let path = path.as_ref();
    let mut missing = Vec::<PathBuf>::new();
    let mut current = path;
    loop {
        match current.try_exists() {
            Ok(true) => break,
            Ok(false) => missing.push(current.to_path_buf()),
            Err(error) => return Err(error),
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }

    fs::create_dir_all(path)?;
    if missing.is_empty() {
        missing.push(path.to_path_buf());
    }
    for directory in missing {
        fs::set_permissions(directory, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_replaces_permissions() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        write_file_with_mode(&path, "new", SHARED_FILE_MODE).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            SHARED_FILE_MODE
        );
    }

    #[test]
    fn ensure_directory_replaces_permissions_for_target() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("state");
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

        ensure_directory_with_mode(&path, SHARED_DIRECTORY_MODE).unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            SHARED_DIRECTORY_MODE
        );
    }

    #[test]
    fn ensure_directory_sets_every_created_component() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("state");
        let path = parent.join("nested");

        ensure_directory_with_mode(&path, SHARED_DIRECTORY_MODE).unwrap();

        for directory in [parent, path] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                SHARED_DIRECTORY_MODE
            );
        }
    }
}
