use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

const TARGETS: &[(&str, &str)] = &[
    ("share/applications", "desktop"),
    ("share/dbus-1/services", "service"),
    ("lib/systemd/user", "service"),
    ("share/systemd/user", "service"),
    ("share/applications/context-menus", "conf"),
];

pub fn run_legacy(arguments: &[OsString]) -> Result<()> {
    if arguments.len() != 2 {
        bail!("app configuration generator expects APP_ID and FILES");
    }
    rewrite(&arguments[0].to_string_lossy(), Path::new(&arguments[1]))
}

pub fn rewrite(app_id: &str, files: &Path) -> Result<()> {
    for (relative, extension) in TARGETS {
        let directory = files.join(relative);
        if !directory.is_dir() {
            continue;
        }
        let mut paths = fs::read_dir(directory)?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file() && path.extension().is_some_and(|value| value == *extension)
            })
            .collect::<Vec<PathBuf>>();
        paths.sort();
        for path in paths {
            rewrite_file(&path, app_id, *extension == "desktop")?;
        }
    }
    Ok(())
}

fn rewrite_file(path: &Path, app_id: &str, desktop: bool) -> Result<()> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("configuration isn't UTF-8: {}", path.display()))?;
    let final_newline = content.ends_with('\n');
    let mut lines = content
        .split_terminator('\n')
        .map(str::to_string)
        .collect::<Vec<_>>();
    if desktop {
        let mut marked = Vec::with_capacity(lines.len());
        for line in lines {
            let has_desktop_entry = line.contains("[Desktop Entry]");
            marked.push(line);
            if has_desktop_entry {
                marked.push(format!("X-linglong={app_id}"));
            }
        }
        lines = marked
            .into_iter()
            .map(|line| {
                if line.contains("TryExec") {
                    "TryExec=ll-cli".to_string()
                } else {
                    line
                }
            })
            .collect();
    }
    for line in &mut lines {
        if matches!(key(line), Some("Exec" | "ExecStart")) {
            let equals = line.find('=').expect("key contains equals sign");
            *line = format!(
                "{}/usr/bin/ll-cli run {app_id} -- {}",
                &line[..=equals],
                &line[equals + 1..]
            );
        }
    }
    let mut output = lines.join("\n");
    if final_newline {
        output.push('\n');
    }
    fs::write(path, output)?;
    Ok(())
}

fn key(line: &str) -> Option<&str> {
    let equals = line.find('=')?;
    let key = line[..equals].trim_end_matches([' ', '\t']);
    (!key.starts_with([' ', '\t'])).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rewrites_legacy_generator_inputs() {
        let temporary = tempdir().unwrap();
        let path = temporary
            .path()
            .join("share/applications/org.example.App.desktop");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[Desktop Entry]\nTryExec=demo\nExec=demo %f\n").unwrap();
        rewrite("org.example.App", temporary.path()).unwrap();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("X-linglong=org.example.App"));
        assert!(content.contains("Exec=/usr/bin/ll-cli run org.example.App -- demo %f"));
    }

    #[test]
    fn matches_legacy_sed_substrings_and_pass_order() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("share/applications/unusual.desktop");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# [Desktop Entry] TryExec\nComment=TryExecNot\n Exec=ignored\nExec =demo\r\n",
        )
        .unwrap();

        rewrite("org.example.App", temporary.path()).unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "TryExec=ll-cli\nX-linglong=org.example.App\nTryExec=ll-cli\n Exec=ignored\nExec =/usr/bin/ll-cli run org.example.App -- demo\r\n"
        );
    }
}
