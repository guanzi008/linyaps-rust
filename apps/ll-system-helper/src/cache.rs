use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn run_font_legacy(arguments: &[OsString]) -> Result<()> {
    if arguments.len() != 2 {
        bail!("font cache generator expects CACHE_ROOT and APP_ID");
    }
    generate_font_cache(Path::new(&arguments[0]), &arguments[1].to_string_lossy())
}

pub fn run_ld_legacy(arguments: &[OsString]) -> Result<()> {
    if arguments.len() != 3 {
        bail!("ld cache generator expects CACHE_ROOT, APP_ID and TRIPLET");
    }
    generate_ld_cache(
        Path::new(&arguments[0]),
        &arguments[1].to_string_lossy(),
        &arguments[2].to_string_lossy(),
    )
}

pub fn generate_font_cache(cache_root: &Path, app_id: &str) -> Result<()> {
    let font_root = cache_root.join("fonts");
    fs::create_dir_all(&font_root)?;
    fs::write(
        font_root.join("fonts.conf"),
        format!(
            "<?xml version=\"1.0\"?>\n<!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n<fontconfig>\n  <dir>/run/linglong/fonts</dir>\n  <include ignore_missing=\"yes\">/opt/apps/{app_id}/files/etc/fonts/fonts.conf</include>\n</fontconfig>\n"
        ),
    )?;
    run_tool("LINGLONG_FC_CACHE", "fc-cache", &[OsString::from("-f")])
}

pub fn generate_ld_cache(cache_root: &Path, app_id: &str, triplet: &str) -> Result<()> {
    fs::create_dir_all(cache_root)?;
    fs::write(
        cache_root.join("ld.so.conf"),
        format!(
            "/runtime/lib\n/runtime/lib/{triplet}\ninclude /runtime/etc/ld.so.conf\n/opt/apps/{app_id}/files/lib\n/opt/apps/{app_id}/files/lib/{triplet}\ninclude /opt/apps/{app_id}/files/etc/ld.so.conf\n"
        ),
    )?;
    run_tool(
        "LINGLONG_LDCONFIG",
        "ldconfig",
        &[
            OsString::from("-C"),
            cache_root.join("ld.so.cache").into_os_string(),
        ],
    )
}

fn run_tool(variable: &str, name: &str, arguments: &[OsString]) -> Result<()> {
    let executable = env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(name));
    let status = Command::new(&executable)
        .args(arguments)
        .status()
        .with_context(|| format!("failed to execute {}", executable.display()))?;
    if !status.success() {
        bail!("{} exited with {status}", executable.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn writes_cache_configuration() {
        let temporary = tempdir().unwrap();
        let tool = temporary.path().join("tool");
        fs::write(&tool, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        unsafe {
            env::set_var("LINGLONG_FC_CACHE", &tool);
            env::set_var("LINGLONG_LDCONFIG", &tool);
        }
        generate_font_cache(temporary.path(), "org.example.App").unwrap();
        generate_ld_cache(temporary.path(), "org.example.App", "x86_64-linux-gnu").unwrap();
        unsafe {
            env::remove_var("LINGLONG_FC_CACHE");
            env::remove_var("LINGLONG_LDCONFIG");
        }
        assert!(
            fs::read_to_string(temporary.path().join("fonts/fonts.conf"))
                .unwrap()
                .contains("/opt/apps/org.example.App/files/etc/fonts/fonts.conf")
        );
        assert!(
            fs::read_to_string(temporary.path().join("ld.so.conf"))
                .unwrap()
                .contains("/runtime/lib/x86_64-linux-gnu")
        );
    }
}
