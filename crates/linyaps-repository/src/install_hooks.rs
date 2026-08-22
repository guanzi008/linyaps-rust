use std::env;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

const PRE_INSTALL: &str = "ll-pre-install=";
const POST_INSTALL: &str = "ll-post-install=";
const POST_UNINSTALL: &str = "ll-post-uninstall=";

#[derive(Default)]
pub struct InstallHooks {
    pre_install: Vec<String>,
    post_install: Vec<String>,
    post_uninstall: Vec<String>,
}

impl InstallHooks {
    pub fn load() -> Result<Self> {
        let directory = env::var_os("LINGLONG_INSTALL_HOOKS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/linglong/config.d"));
        Self::load_from(&directory)
    }

    fn load_from(directory: &Path) -> Result<Self> {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut hooks = Self::default();
        for entry in entries {
            let entry = entry?;
            if !entry.metadata()?.is_file() {
                continue;
            }
            let path = entry.path();
            for (index, line) in fs::read_to_string(&path)?.lines().enumerate() {
                let destination = if let Some(command) = parse_command(line, PRE_INSTALL)
                    .with_context(|| {
                        format!("invalid install hook in {}:{}", path.display(), index + 1)
                    })? {
                    Some((&mut hooks.pre_install, command))
                } else if let Some(command) =
                    parse_command(line, POST_INSTALL).with_context(|| {
                        format!("invalid install hook in {}:{}", path.display(), index + 1)
                    })?
                {
                    Some((&mut hooks.post_install, command))
                } else if let Some(command) =
                    parse_command(line, POST_UNINSTALL).with_context(|| {
                        format!("invalid install hook in {}:{}", path.display(), index + 1)
                    })?
                {
                    Some((&mut hooks.post_uninstall, command))
                } else {
                    None
                };
                if let Some((commands, command)) = destination {
                    commands.push(command);
                }
            }
        }
        Ok(hooks)
    }

    pub fn pre_install(&self, file: &File) -> Result<()> {
        if self.pre_install.is_empty() {
            return Ok(());
        }
        let link = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            file.as_raw_fd()
        ));
        let path = fs::read_link(&link).with_context(|| {
            format!(
                "failed to resolve install file descriptor {}",
                link.display()
            )
        })?;
        execute(
            &self.pre_install,
            &[("LINGLONG_UAB_PATH", path.as_os_str())],
        )
    }

    pub fn post_install(&self, app_id: &str, path: &Path) -> Result<()> {
        execute(
            &self.post_install,
            &[
                ("LINGLONG_APPID", app_id.as_ref()),
                ("LINGLONG_APP_INSTALL_PATH", path.as_os_str()),
            ],
        )
    }

    pub fn post_uninstall(&self, app_id: &str) -> Result<()> {
        execute(&self.post_uninstall, &[("LINGLONG_APPID", app_id.as_ref())])
    }
}

fn parse_command(line: &str, prefix: &str) -> Result<Option<String>> {
    let line = line.trim_start_matches(char::is_whitespace);
    let Some(command) = line.strip_prefix(prefix) else {
        return Ok(None);
    };
    let command = command.trim_start_matches(char::is_whitespace);
    let Some(quote) = command
        .chars()
        .next()
        .filter(|value| matches!(value, '\'' | '"'))
    else {
        return Ok(Some(command.to_string()));
    };
    let command = &command[quote.len_utf8()..];
    let trimmed = command.trim_end_matches(char::is_whitespace);
    let Some(command) = trimmed.strip_suffix(quote) else {
        bail!("unterminated quoted command");
    };
    Ok(Some(command.to_string()))
}

fn execute(commands: &[String], environment: &[(&str, &std::ffi::OsStr)]) -> Result<()> {
    for command in commands {
        let mut process = Command::new("sh");
        process.arg("-c").arg(command);
        for (name, value) in environment {
            process.env(name, value);
        }
        let status = process
            .status()
            .with_context(|| format!("failed to execute hook command '{command}'"))?;
        if !status.success() {
            bail!("hook command '{command}' failed with {status}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_supported_command_forms() {
        assert_eq!(
            parse_command(" ll-pre-install=echo one", PRE_INSTALL).unwrap(),
            Some("echo one".to_string())
        );
        assert_eq!(
            parse_command("ll-pre-install='echo two'  ", PRE_INSTALL).unwrap(),
            Some("echo two".to_string())
        );
        assert_eq!(parse_command("unrelated=value", PRE_INSTALL).unwrap(), None);
        assert!(parse_command("ll-pre-install='broken", PRE_INSTALL).is_err());
    }

    #[test]
    fn loads_and_executes_post_hooks() {
        let temporary = tempdir().unwrap();
        let output = temporary.path().join("output");
        let command = format!(
            "printf '%s:%s' \"$LINGLONG_APPID\" \"$LINGLONG_APP_INSTALL_PATH\" > '{}'",
            output.display()
        );
        fs::write(
            temporary.path().join("hooks.conf"),
            format!("ll-post-install='{command}'\n"),
        )
        .unwrap();
        let hooks = InstallHooks::load_from(temporary.path()).unwrap();
        hooks
            .post_install("org.example.App", Path::new("/layer/path"))
            .unwrap();
        assert_eq!(
            fs::read_to_string(output).unwrap(),
            "org.example.App:/layer/path"
        );
    }
}
