use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub fn run_legacy(arguments: &[OsString]) -> Result<()> {
    if arguments.len() != 3 {
        bail!("systemd user generator expects normal, early and late output directories");
    }
    generate(Path::new(&arguments[2]))
}

pub fn generate(late: &Path) -> Result<()> {
    let root = env::var_os("LINGLONG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/linglong"));
    generate_from(&root.join("entries/lib/systemd/user"), late)
}

fn generate_from(source: &Path, late: &Path) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(late)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = late.join(entry.file_name());
        let _ = link_tree(&entry.path(), &destination);
    }
    Ok(())
}

fn link_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let _ = link_tree(&entry.path(), &destination.join(entry.file_name()));
        }
        return Ok(());
    }
    if fs::symlink_metadata(destination).is_ok() {
        return Ok(());
    }
    symlink(source, destination).with_context(|| {
        format!(
            "failed to export systemd user unit {}",
            destination.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn links_units_into_late_generator_directory() {
        let temporary = tempdir().unwrap();
        let source = temporary.path().join("source");
        let late = temporary.path().join("late");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("org.example.service"), "unit").unwrap();
        fs::create_dir_all(source.join("example.target.wants")).unwrap();
        fs::write(
            source.join("example.target.wants/org.example.service"),
            "unit",
        )
        .unwrap();
        generate_from(&source, &late).unwrap();
        assert_eq!(
            fs::read_link(late.join("org.example.service")).unwrap(),
            source.join("org.example.service")
        );
        assert!(late.join("example.target.wants").is_dir());
        assert_eq!(
            fs::read_link(late.join("example.target.wants/org.example.service")).unwrap(),
            source.join("example.target.wants/org.example.service")
        );
    }
}
