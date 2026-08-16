use std::env;
use std::path::{Path, PathBuf};

use anyhow::Result;

const DEFAULT_ROOT: &str = "/var/lib/linglong";

pub fn data_dirs() -> String {
    let root = env::var_os("LINGLONG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT));
    let export = env::var_os("LINGLONG_EXPORT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("share"));
    data_dirs_with(
        env::var_os("XDG_DATA_DIRS")
            .as_deref()
            .and_then(|value| value.to_str()),
        &root,
        &export,
    )
}

pub fn print_environment() -> Result<()> {
    println!("XDG_DATA_DIRS={}", data_dirs());
    Ok(())
}

fn data_dirs_with(current: Option<&str>, root: &Path, export: &Path) -> String {
    let mut value = current
        .filter(|value| !value.is_empty())
        .unwrap_or("/usr/local/share:/usr/share")
        .to_string();
    append_unique(&mut value, &root.join("entries/share"));
    if export != Path::new("share") {
        prepend_unique(&mut value, &root.join("entries").join(export));
    }
    value
}

fn path_to_add(path: &Path) -> String {
    let value = path.to_string_lossy();
    value
        .strip_suffix('/')
        .unwrap_or(value.as_ref())
        .to_string()
}

fn contains_path(value: &str, path: &str) -> bool {
    value.split(':').any(|entry| entry == path)
}

fn append_unique(value: &mut String, path: &Path) {
    let path = path_to_add(path);
    if path.is_empty() || contains_path(value, &path) {
        return;
    }
    if !value.is_empty() {
        value.push(':');
    }
    value.push_str(&path);
}

fn prepend_unique(value: &mut String, path: &Path) {
    let path = path_to_add(path);
    if path.is_empty() || contains_path(value, &path) {
        return;
    }
    if value.is_empty() {
        *value = path;
    } else {
        *value = format!("{path}:{value}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_default_and_prepends_custom_export() {
        assert_eq!(
            data_dirs_with(
                Some("/usr/share:/custom/"),
                Path::new("/var/lib/linglong"),
                Path::new("share-overlay")
            ),
            "/var/lib/linglong/entries/share-overlay:/usr/share:/custom/:/var/lib/linglong/entries/share"
        );
    }

    #[test]
    fn does_not_duplicate_paths() {
        assert_eq!(
            data_dirs_with(
                Some("/usr/share:/var/lib/linglong/entries/share"),
                Path::new("/var/lib/linglong"),
                Path::new("share")
            ),
            "/usr/share:/var/lib/linglong/entries/share"
        );
    }

    #[test]
    fn preserves_empty_and_trailing_slash_entries_like_shell_helper() {
        assert_eq!(
            data_dirs_with(
                Some("/usr/share::/var/lib/linglong/entries/share/"),
                Path::new("/var/lib/linglong"),
                Path::new("share")
            ),
            "/usr/share::/var/lib/linglong/entries/share/:/var/lib/linglong/entries/share"
        );
    }
}
