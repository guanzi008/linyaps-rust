use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

use linyaps_api::UabMetaInfo;
use linyaps_repository::UabFile;
use serde::Serialize;

mod readonly_fs;

use readonly_fs::mount_read_only;

const USAGE: &str = "Linglong Universal Application Bundle

An offline distribution executable bundle of linglong.

Usage:
uabBundle [uabOptions...] [-- loaderOptions...]

Options:
    --extract=PATH extract the read-only filesystem image which is in the 'linglong.bundle' segment of uab to PATH. [exclusive]
    --mount=PATH mount the read-only filesystem image which is in the 'linglong.bundle' segment of uab to PATH, use ctrl+c to stop. [exclusive]
    --print-meta print content of json which from the 'linglong.meta' segment of uab to STDOUT [exclusive]
    --help print usage of uab [exclusive]
";

static RECEIVED_SIGNAL: AtomicI32 = AtomicI32::new(0);
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

#[derive(Debug, Default, Eq, PartialEq)]
struct HeaderOptions {
    help: bool,
    print_meta: bool,
    extract: Option<PathBuf>,
    mount: Option<PathBuf>,
    loader_arguments: Vec<OsString>,
}

#[derive(Debug)]
struct ArgumentError {
    message: String,
    code: i32,
}

fn parse_arguments(arguments: &[OsString]) -> Result<HeaderOptions, ArgumentError> {
    let splitter = arguments
        .iter()
        .position(|argument| argument == OsStr::new("--"));
    let option_arguments = splitter.map_or(arguments, |index| &arguments[..index]);
    let loader_arguments = splitter
        .map(|index| arguments[index + 1..].to_vec())
        .unwrap_or_default();
    let mut options = HeaderOptions {
        loader_arguments,
        ..HeaderOptions::default()
    };
    let mut exclusive = 0;
    let mut index = 0;
    let mut positional_seen = false;
    while index < option_arguments.len() {
        let argument = &option_arguments[index];
        let bytes = argument.as_os_str().as_bytes();
        if !bytes.starts_with(b"-") || positional_seen {
            positional_seen |= env::var_os("POSIXLY_CORRECT").is_some();
            index += 1;
            continue;
        }
        let Some(long) = bytes.strip_prefix(b"--") else {
            return Err(ArgumentError {
                message: format!("unrecognized option '{}'", argument.to_string_lossy()),
                code: libc::EINVAL,
            });
        };
        let (name, attached) = long
            .iter()
            .position(|byte| *byte == b'=')
            .map_or((long, None), |position| {
                (&long[..position], Some(&long[position + 1..]))
            });
        let matches = [b"help".as_slice(), b"print-meta", b"extract", b"mount"]
            .into_iter()
            .filter(|candidate| candidate.starts_with(name))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(ArgumentError {
                message: format!("unrecognized option '{}'", argument.to_string_lossy()),
                code: libc::EINVAL,
            });
        }
        match matches[0] {
            b"help" | b"print-meta" if attached.is_some() => {
                return Err(ArgumentError {
                    message: format!(
                        "option '--{}' doesn't allow an argument",
                        String::from_utf8_lossy(matches[0])
                    ),
                    code: libc::EINVAL,
                });
            }
            b"help" => {
                options.help = true;
                exclusive += 1;
            }
            b"print-meta" => {
                options.print_meta = true;
                exclusive += 1;
            }
            b"extract" | b"mount" => {
                let value = if let Some(value) = attached {
                    OsString::from(OsStr::from_bytes(value))
                } else {
                    index += 1;
                    option_arguments
                        .get(index)
                        .cloned()
                        .ok_or_else(|| ArgumentError {
                            message: format!(
                                "option '--{}' requires an argument",
                                String::from_utf8_lossy(matches[0])
                            ),
                            code: libc::EINVAL,
                        })?
                };
                if !value.is_empty() {
                    if matches[0] == b"extract" {
                        options.extract = Some(PathBuf::from(value));
                    } else {
                        options.mount = Some(PathBuf::from(value));
                    }
                }
                exclusive += 1;
            }
            _ => unreachable!(),
        }
        index += 1;
    }
    if exclusive > 1 {
        return Err(ArgumentError {
            message: "exclusive options has been detected".to_string(),
            code: -1,
        });
    }
    Ok(options)
}

extern "C" fn signal_handler(signal: libc::c_int) {
    RECEIVED_SIGNAL.store(signal, Ordering::Relaxed);
    let child = CHILD_PID.load(Ordering::Relaxed);
    if child > 0 {
        unsafe {
            libc::kill(child, signal);
        }
    }
}

fn install_signal_handlers() -> io::Result<()> {
    let signals = [
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGHUP,
        libc::SIGABRT,
    ];
    unsafe {
        let mut action = std::mem::zeroed::<libc::sigaction>();
        action.sa_sigaction = signal_handler as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in signals {
            libc::sigaddset(&mut action.sa_mask, signal);
        }
        for signal in signals {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

struct MountedBundle {
    path: PathBuf,
    created: bool,
    session: Option<fuser::BackgroundSession>,
}

impl MountedBundle {
    fn temporary(uab: &UabFile, metadata: &UabMetaInfo) -> Result<Self, String> {
        let runtime = match env::var_os("XDG_RUNTIME_DIR") {
            Some(path) => PathBuf::from(path),
            None => {
                eprintln!("failed to get XDG_RUNTIME_DIR, fallback to /tmp");
                PathBuf::from("/tmp")
            }
        };
        let runtime = runtime
            .canonicalize()
            .map_err(|error| format!("failed to resolve path:{error}"))?;
        let path = runtime.join("linglong/UAB").join(&metadata.uuid);
        fs::create_dir_all(&path)
            .map_err(|error| format!("couldn't create mount point {}: {error}", path.display()))?;
        Self::mount(uab, path, true)
    }

    fn at_path(uab: &UabFile, path: &Path) -> Result<Self, String> {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("failed to status {}: {error}", path.display()))?;
        if !metadata.is_dir() {
            return Err(format!("{}is not a directory", path.display()));
        }
        Self::mount(uab, path.to_path_buf(), false)
    }

    fn mount(uab: &UabFile, path: PathBuf, created: bool) -> Result<Self, String> {
        let (file, bundle) = uab.bundle_source().map_err(|error| error.to_string())?;
        let session = mount_read_only(file, bundle.offset, bundle.size, &path)
            .map_err(|error| error.to_string())?;
        Ok(Self {
            path,
            created,
            session: Some(session),
        })
    }
}

impl Drop for MountedBundle {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                session.umount_and_join()
            }));
        }
        if self.created {
            let _ = clear_path(&self.path);
        }
    }
}

fn clear_path(path: &Path) -> io::Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    let created = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() => false,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", destination.display()),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(destination)?;
            true
        }
        Err(error) => return Err(error),
    };
    if created {
        fs::set_permissions(destination, fs::symlink_metadata(source)?.permissions())?;
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.is_dir() {
            copy_tree(&source, &destination)?;
        } else if metadata.file_type().is_symlink() {
            if fs::symlink_metadata(&destination).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", destination.display()),
                ));
            }
            symlink(fs::read_link(&source)?, &destination)?;
        } else if metadata.is_file() {
            let mut input = File::open(&source)?;
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&destination)?;
            io::copy(&mut input, &mut output)?;
            fs::set_permissions(&destination, metadata.permissions())?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported file type: {}", source.display()),
            ));
        }
    }
    Ok(())
}

fn print_metadata(metadata: &UabMetaInfo) -> Result<(), String> {
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
    metadata
        .serialize(&mut serializer)
        .map_err(|error| error.to_string())?;
    writeln!(output).map_err(|error| error.to_string())
}

fn run_loader(bundle: &Path, arguments: &[OsString]) -> Result<i32, String> {
    let loader = bundle.join("loader");
    if !loader.exists() {
        println!("This UAB is not support for running");
        return Ok(0);
    }
    let mut child = Command::new(&loader)
        .args(arguments)
        .env("LINGLONG_UAB_LOADER_ONLY_APP", "true")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("execv({}) error: {error}", loader.display()))?;
    CHILD_PID.store(child.id() as i32, Ordering::Relaxed);
    let status = child
        .wait()
        .map_err(|error| format!("waitpid failed:{error}"))?;
    CHILD_PID.store(0, Ordering::Relaxed);
    if let Some(code) = status.code() {
        return Ok(code);
    }
    if let Some(signal) = status.signal() {
        return Ok(128 + signal);
    }
    Err("unknown exit state of loader".to_string())
}

fn execute() -> Result<i32, String> {
    install_signal_handlers().map_err(|error| error.to_string())?;
    let mut process_arguments = env::args_os();
    let executable = process_arguments
        .next()
        .ok_or_else(|| "couldn't get executable path".to_string())?;
    let arguments = process_arguments.collect::<Vec<_>>();
    let options = parse_arguments(&arguments).map_err(|error| {
        if !error.message.is_empty() {
            eprintln!("{}", error.message);
        }
        format!("__argument_exit__{}", error.code)
    })?;
    if options.help {
        println!("{USAGE}");
        return Ok(0);
    }
    let uab = UabFile::open(&executable).map_err(|error| error.to_string())?;
    let metadata = uab.metadata().map_err(|error| error.to_string())?;
    if options.print_meta {
        print_metadata(&metadata)?;
        return Ok(0);
    }
    uab.verify().map_err(|error| error.to_string())?;
    let mounted = if let Some(path) = options.mount.as_deref() {
        MountedBundle::at_path(&uab, path)?
    } else {
        MountedBundle::temporary(&uab, &metadata)?
    };
    if options.mount.is_some() {
        while RECEIVED_SIGNAL.load(Ordering::Relaxed) == 0 {
            unsafe {
                libc::pause();
            }
        }
        return Ok(128 + RECEIVED_SIGNAL.load(Ordering::Relaxed));
    }
    if let Some(destination) = options.extract.as_deref() {
        fs::create_dir_all(destination)
            .map_err(|error| format!("failed to create {}: {error}", destination.display()))?;
        copy_tree(&mounted.path, destination).map_err(|error| {
            format!(
                "failed to extract bundle from {} to {}: {error}",
                mounted.path.display(),
                destination.display()
            )
        })?;
        return Ok(0);
    }
    if !metadata.only_app.unwrap_or(false) {
        println!("This UAB is not support for running");
        return Ok(0);
    }
    let app = metadata
        .layers
        .iter()
        .find(|layer| layer.info.kind == "app")
        .ok_or_else(|| "__coded_exit__1:failed to find appID and module".to_string())?;
    if app.info.id.is_empty() || app.info.module.is_empty() {
        return Err("__coded_exit__1:failed to find appID and module".to_string());
    }
    unsafe {
        env::set_var(
            "LINGLONG_UAB_APPROOT",
            mounted
                .path
                .join("layers")
                .join(&app.info.id)
                .join(&app.info.module)
                .join("files"),
        );
    }
    run_loader(&mounted.path, &options.loader_arguments)
}

fn main() {
    let code = match execute() {
        Ok(code) => code,
        Err(error) => {
            if let Some(code) = error.strip_prefix("__argument_exit__") {
                code.parse().unwrap_or(libc::EINVAL)
            } else if let Some(error) = error.strip_prefix("__coded_exit__") {
                let (code, message) = error.split_once(':').unwrap_or(("-1", error));
                eprintln!("{message}");
                code.parse().unwrap_or(-1)
            } else {
                eprintln!("{error}");
                -1
            }
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn splits_loader_arguments() {
        let arguments = [
            OsString::from("--"),
            OsString::from("--flag"),
            OsString::from("value"),
        ];
        let options = parse_arguments(&arguments).unwrap();
        assert_eq!(
            options.loader_arguments,
            [OsString::from("--flag"), OsString::from("value")]
        );
    }

    #[test]
    fn rejects_multiple_exclusive_options() {
        let arguments = [OsString::from("--help"), OsString::from("--print-meta")];
        let error = parse_arguments(&arguments).unwrap_err();
        assert_eq!(error.code, -1);
        assert_eq!(error.message, "exclusive options has been detected");
    }

    #[test]
    fn accepts_unique_long_option_prefixes_and_empty_paths() {
        assert!(
            parse_arguments(&[OsString::from("--pri")])
                .unwrap()
                .print_meta
        );
        let options = parse_arguments(&[OsString::from("--extract=")]).unwrap();
        assert!(options.extract.is_none());
    }

    #[test]
    fn extraction_merges_directories_without_overwriting_files() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let destination = temporary.path().join("destination");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(destination.join("nested")).unwrap();
        fs::write(source.join("nested/new"), "new").unwrap();
        fs::write(destination.join("nested/old"), "old").unwrap();
        copy_tree(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("nested/new")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(destination.join("nested/old")).unwrap(),
            "old"
        );
        assert!(copy_tree(&source, &destination).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("nested/new")).unwrap(),
            "new"
        );
    }

    #[test]
    fn bash_loader_exit_status_is_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let loader = temporary.path().join("loader");
        fs::write(&loader, "#!/bin/sh\nexit 41\n").unwrap();
        fs::set_permissions(&loader, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(run_loader(temporary.path(), &[]).unwrap(), 41);
    }
}
