use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use futures_lite::StreamExt;
use linyaps_core::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zvariant::OwnedValue;

const NVIDIA_IDENTIFY: &str = "org.deepin.driver.display.nvidia";
const NVIDIA_VERSION_FILE: &str = "/sys/module/nvidia/version";
const ACTION_INSTALL_NOW: &str = "install_now";
const ACTION_NOT_REMIND: &str = "not_remind";
const NOTIFICATION_ICON: &str = "/usr/share/icons/hicolor/scalable/apps/linyaps.svg";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Options {
    force: bool,
    check_only: bool,
    install_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ParseOutcome {
    Run(Options),
    Help(String),
    Error { code: i32, message: String },
}

fn parse_flag_value(name: &str, value: &str) -> Result<bool, String> {
    if value.is_empty() {
        return Ok(true);
    }
    match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        _ => Err(format!("Could not convert: --{name} = {value}")),
    }
}

fn parse_options_from(arguments: impl IntoIterator<Item = OsString>) -> ParseOutcome {
    let mut arguments = arguments.into_iter();
    let program = arguments
        .next()
        .unwrap_or_else(|| OsString::from("ll-driver-detect"))
        .to_string_lossy()
        .into_owned();
    let arguments = arguments.collect::<Vec<_>>();
    let mut options = Options::default();
    let mut unexpected = Vec::new();
    let mut conversion_error = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index].to_string_lossy();
        if argument == "-h" || argument == "--help" {
            return ParseOutcome::Help(program);
        }
        if argument == "--" {
            if index + 1 < arguments.len() {
                unexpected.push(argument.into_owned());
                unexpected.extend(
                    arguments[index + 1..]
                        .iter()
                        .map(|value| value.to_string_lossy().into_owned()),
                );
            }
            break;
        }
        if let Some(long) = argument.strip_prefix("--") {
            let (name, value) = long
                .split_once('=')
                .map_or((long, None), |(name, value)| (name, Some(value)));
            let target = match name {
                "force" => Some(&mut options.force),
                "check-only" => Some(&mut options.check_only),
                "install-only" => Some(&mut options.install_only),
                _ => None,
            };
            if let Some(target) = target {
                match value.map_or(Ok(true), |value| parse_flag_value(name, value)) {
                    Ok(value) => *target = value,
                    Err(error) if conversion_error.is_none() => conversion_error = Some(error),
                    Err(_) => {}
                }
            } else {
                unexpected.push(argument.into_owned());
            }
            index += 1;
            continue;
        }
        if let Some(shorts) = argument.strip_prefix('-').filter(|value| !value.is_empty()) {
            let mut offset = 0;
            for (position, short) in shorts.char_indices() {
                offset = position;
                match short {
                    'f' => options.force = true,
                    'c' => options.check_only = true,
                    'i' => options.install_only = true,
                    'h' => return ParseOutcome::Help(program),
                    _ => {
                        unexpected.push(format!("-{}", &shorts[offset..]));
                        break;
                    }
                }
                offset = position + short.len_utf8();
            }
            let _ = offset;
        } else {
            unexpected.push(argument.into_owned());
        }
        index += 1;
    }
    if let Some(message) = conversion_error {
        return ParseOutcome::Error { code: 104, message };
    }
    if !unexpected.is_empty() {
        unexpected.reverse();
        let message = if unexpected.len() == 1 {
            format!("The following argument was not expected: {}", unexpected[0])
        } else {
            format!(
                "The following arguments were not expected: {}",
                unexpected.join(" ")
            )
        };
        return ParseOutcome::Error { code: 109, message };
    }
    ParseOutcome::Run(options)
}

fn print_help(program: &str) {
    println!(
        "Linglong Graphics Driver Detection Tool\nUsage: {program} [OPTIONS]\n\nOptions:\n  -h,--help                   Print this help message and exit\n  -f,--force                  {}\n  -c,--check-only             {}\n  -i,--install-only           {}",
        linyaps_i18n::gettext("Force installation even if recently reminded"),
        linyaps_i18n::gettext("Check for drivers only without installing or notifying"),
        linyaps_i18n::gettext("Only install drivers without notifications"),
    );
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GraphicsDriverInfo {
    identify: String,
    package_name: String,
    package_version: String,
    repo_name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct DriverDetectionConfig {
    #[serde(rename = "neverRemind")]
    never_remind: bool,
}

struct ConfigManager {
    path: PathBuf,
    config: DriverDetectionConfig,
}

impl ConfigManager {
    fn load(path: PathBuf) -> Self {
        let config = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, config }
    }

    fn should_show_notification(&self) -> bool {
        !self.config.never_remind
    }

    fn save(&self) -> io::Result<()> {
        let mut output = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
        let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
        self.config
            .serialize(&mut serializer)
            .map_err(io::Error::other)?;
        let mut file = File::create(&self.path)?;
        file.write_all(&output)
    }

    fn never_remind(&mut self) {
        self.config.never_remind = true;
        let _ = self.save();
    }
}

struct SingletonLock {
    file: File,
    path: PathBuf,
}

#[derive(Default)]
struct ProcessLockedPaths {
    pid: libc::pid_t,
    paths: HashSet<PathBuf>,
}

static PROCESS_LOCKED_PATHS: OnceLock<Mutex<ProcessLockedPaths>> = OnceLock::new();

fn process_locked_paths() -> &'static Mutex<ProcessLockedPaths> {
    PROCESS_LOCKED_PATHS.get_or_init(|| {
        Mutex::new(ProcessLockedPaths {
            pid: unsafe { libc::getpid() },
            paths: HashSet::new(),
        })
    })
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn unregister_process_lock(path: &Path) {
    let current_pid = unsafe { libc::getpid() };
    if let Ok(mut state) = process_locked_paths().lock()
        && state.pid == current_pid
    {
        state.paths.remove(path);
    }
}

impl SingletonLock {
    fn acquire(path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let path = absolute_path(path)?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)?;
        {
            let current_pid = unsafe { libc::getpid() };
            let mut state = process_locked_paths()
                .lock()
                .map_err(|_| io::Error::other("process lock registry is poisoned"))?;
            if state.pid != current_pid {
                state.pid = current_pid;
                state.paths.clear();
            }
            if !state.paths.insert(path.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("process already holds a lock on file {}", path.display()),
                ));
            }
        }
        let mut lock = unsafe { std::mem::zeroed::<libc::flock>() };
        lock.l_type = libc::F_WRLCK as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &lock) };
        if result == 0 {
            Ok(Some(Self { file, path }))
        } else {
            let error = io::Error::last_os_error();
            unregister_process_lock(&path);
            if matches!(
                error.raw_os_error(),
                Some(libc::EACCES) | Some(libc::EAGAIN)
            ) {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        let mut lock = unsafe { std::mem::zeroed::<libc::flock>() };
        lock.l_type = libc::F_UNLCK as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        unsafe {
            libc::fcntl(self.file.as_raw_fd(), libc::F_SETLK, &lock);
        }
        unregister_process_lock(&self.path);
    }
}

fn config_path() -> Result<PathBuf, String> {
    let home = env::var_os("HOME").ok_or_else(|| "Couldn't get HOME env.".to_string())?;
    let directory = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".config"));
    Ok(directory.join("linglong/driver_detection.json"))
}

fn driver_version(path: &Path) -> io::Result<Option<String>> {
    if !path.exists() {
        eprintln!("NVIDIA version file not found: {}", path.display());
        return Ok(None);
    }
    let mut version = String::new();
    let Ok(mut file) = File::open(path) else {
        return Ok(None);
    };
    if file.read_to_string(&mut version).is_err() {
        return Ok(None);
    }
    let Some(version) = version.split_whitespace().next() else {
        return Ok(None);
    };
    Ok(Some(version.replace('.', "-")))
}

fn run_cli(arguments: &[&str]) -> Result<Output, String> {
    Command::new("ll-cli")
        .args(arguments)
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                "command not found: ll-cli".to_string()
            } else {
                format!("failed to execute ll-cli: {error}")
            }
        })
        .and_then(|output| {
            if output.status.success() {
                Ok(output)
            } else if let Some(code) = output.status.code() {
                Err(format!(
                    "command execute failed with exit code {code}: {}",
                    String::from_utf8_lossy(&output.stdout)
                ))
            } else if let Some(signal) = output.status.signal() {
                Err(format!("command killed by signal: {signal}"))
            } else {
                Err("command exited abnormally".to_string())
            }
        })
}

fn consider_remote_packages(
    package_name: &str,
    repo: &str,
    packages: &Value,
    selected: &mut Option<GraphicsDriverInfo>,
) {
    let Some(packages) = packages.as_array() else {
        return;
    };
    for package in packages {
        let Some(version) = package.get("version").and_then(Value::as_str) else {
            continue;
        };
        let replace = selected
            .as_ref()
            .is_none_or(|current| compare_versions(version, &current.package_version));
        if replace {
            *selected = Some(GraphicsDriverInfo {
                identify: NVIDIA_IDENTIFY.to_string(),
                package_name: package_name.to_string(),
                package_version: version.to_string(),
                repo_name: repo.to_string(),
            });
        }
    }
}

fn parse_remote_info(
    package_name: &str,
    output: &[u8],
) -> Result<Option<GraphicsDriverInfo>, String> {
    let document: Value = serde_json::from_slice(output)
        .map_err(|error| format!("Failed to parse search result JSON: {error}"))?;
    let mut selected: Option<GraphicsDriverInfo> = None;
    match &document {
        Value::Object(repositories) => {
            for (repo, packages) in repositories {
                consider_remote_packages(package_name, repo, packages, &mut selected);
            }
        }
        Value::Array(repositories) => {
            for (index, packages) in repositories.iter().enumerate() {
                consider_remote_packages(package_name, &index.to_string(), packages, &mut selected);
            }
        }
        _ => {}
    }
    Ok(selected)
}

fn parse_installed_info(package_name: &str, output: &[u8]) -> Result<Option<String>, String> {
    let packages: Value = serde_json::from_slice(output)
        .map_err(|error| format!("Failed to parse installed package JSON: {error}"))?;
    let packages = packages
        .as_array()
        .ok_or_else(|| "Invalid list result JSON: expected an array".to_string())?;
    for package in packages {
        if package.get("id").and_then(Value::as_str) != Some(package_name) {
            continue;
        }
        return package
            .get("version")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .map(Some)
            .ok_or_else(|| {
                format!("Installed package found but version field is missing: {package_name}")
            });
    }
    Ok(None)
}

fn compare_versions(left: &str, right: &str) -> bool {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left > right,
        _ => false,
    }
}

fn remote_info(package_name: &str) -> Result<Option<GraphicsDriverInfo>, String> {
    let output = run_cli(&["--json", "search", package_name])
        .map_err(|error| format!("Search command failed: {error}"))?;
    parse_remote_info(package_name, &output.stdout)
}

fn installed_version(package_name: &str) -> Result<Option<String>, String> {
    let output = run_cli(&["--json", "list", "--type=extension"])?;
    parse_installed_info(package_name, &output.stdout)
}

fn detect_nvidia_with<R, I>(
    version_path: &Path,
    mut get_remote_info: R,
    mut get_installed_version: I,
) -> Result<GraphicsDriverInfo, String>
where
    R: FnMut(&str) -> Result<Option<GraphicsDriverInfo>, String>,
    I: FnMut(&str) -> Result<Option<String>, String>,
{
    let version = driver_version(version_path)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Failed to get NVIDIA driver version".to_string())?;
    let package_name = format!("{NVIDIA_IDENTIFY}.{version}");
    let remote = get_remote_info(&package_name)
        .map_err(|error| format!("Failed to get package info from remote repo: {error}"))?;
    let installed = get_installed_version(&package_name)
        .map_err(|error| format!("Failed to check if package is installed: {error}"))?;
    if let Some(installed) = installed {
        let remote = remote
            .ok_or_else(|| "Cannot find NVIDIA driver package in remote repo.".to_string())?;
        let upgradable_remote = get_remote_info(&package_name).map_err(|error| {
            format!(
                "Failed to check if package is upgradable: Failed to get upgradable package info: {error}"
            )
        })?;
        let upgradable_remote = upgradable_remote.ok_or_else(|| {
            "Failed to check if package is upgradable: NVIDIA driver package not found in remote repo"
                .to_string()
        })?;
        let current = get_installed_version(&package_name).map_err(|error| {
            format!(
                "Failed to check if package is upgradable: Failed to get installed package info: {error}"
            )
        })?;
        let current = current.ok_or_else(|| {
            "Failed to check if package is upgradable: NVIDIA driver package is not installed"
                .to_string()
        })?;
        if compare_versions(&upgradable_remote.package_version, &current) {
            return Ok(remote);
        }
        let _ = installed;
        return Err("NVIDIA driver package is already installed and up-to-date.".to_string());
    }
    remote.ok_or_else(|| "NVIDIA driver package not found in remote repo".to_string())
}

fn detect_nvidia() -> Result<GraphicsDriverInfo, String> {
    detect_nvidia_with(
        Path::new(NVIDIA_VERSION_FILE),
        remote_info,
        installed_version,
    )
}

fn detect_available_drivers() -> Vec<GraphicsDriverInfo> {
    match detect_nvidia() {
        Ok(driver) => vec![driver],
        Err(error) => {
            eprintln!("Driver detection failed {NVIDIA_IDENTIFY}: {error}");
            Vec::new()
        }
    }
}

fn install_drivers(drivers: &[GraphicsDriverInfo]) -> Result<(), String> {
    for driver in drivers {
        let reference = format!("{}/{}", driver.package_name, driver.package_version);
        run_cli(&["install", &reference, "--repo", &driver.repo_name])
            .map_err(|error| format!("Installation command failed: {error}"))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    #[zbus(name = "Notify")]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal, name = "ActionInvoked")]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal, name = "NotificationClosed")]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

async fn interactive_notification() -> Result<Option<String>, String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| error.to_string())?;
    let proxy = NotificationsProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    let mut actions = proxy
        .receive_action_invoked()
        .await
        .map_err(|error| error.to_string())?;
    let mut closed = proxy
        .receive_notification_closed()
        .await
        .map_err(|error| error.to_string())?;
    let notification_id = proxy
        .notify(
            "linyaps",
            0,
            NOTIFICATION_ICON,
            &linyaps_i18n::gettext("Graphics Driver Available"),
            &linyaps_i18n::gettext(
                "Graphics driver package is available that can improve performance for some Linyaps applications.\nWould you like to install it?",
            ),
            vec![
                ACTION_INSTALL_NOW.to_string(),
                linyaps_i18n::gettext("Install Now").into_owned(),
                ACTION_NOT_REMIND.to_string(),
                linyaps_i18n::gettext("Don't Remind").into_owned(),
            ],
            HashMap::new(),
            25_000,
        )
        .await
        .map_err(|error| error.to_string())?;
    let deadline = tokio::time::sleep(Duration::from_millis(25_000));
    tokio::pin!(deadline);
    let mut selected = None;
    loop {
        tokio::select! {
            _ = &mut deadline => return Ok(selected),
            signal = actions.next() => {
                let Some(signal) = signal else {
                    return Ok(selected);
                };
                let arguments = signal.args().map_err(|error| error.to_string())?;
                if *arguments.id() == notification_id {
                    selected = Some(arguments.action_key().to_string());
                }
            }
            signal = closed.next() => {
                let Some(signal) = signal else {
                    return Ok(selected);
                };
                let arguments = signal.args().map_err(|error| error.to_string())?;
                if *arguments.id() == notification_id {
                    return Ok(selected);
                }
            }
        }
    }
}

async fn simple_notification(summary: &str, body: &str) -> Result<(), String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| error.to_string())?;
    let proxy = NotificationsProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    proxy
        .notify(
            "linyaps",
            0,
            "",
            summary,
            body,
            Vec::new(),
            HashMap::new(),
            5_000,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn run(options: Options) -> Result<(), String> {
    let config_path = config_path()?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let lock_path = config_path.with_file_name("ll-driver-detect.lock");
    let Some(_lock) = SingletonLock::acquire(&lock_path).map_err(|error| error.to_string())? else {
        return Ok(());
    };
    let mut config = ConfigManager::load(config_path);
    if !options.force && !config.should_show_notification() {
        return Ok(());
    }
    let drivers = detect_available_drivers();
    if drivers.is_empty() {
        return Ok(());
    }
    if options.check_only {
        println!(
            "CHECK ONLY: only check drivers, no notifications or installations will be performed."
        );
        println!("Detected drivers:");
        for driver in &drivers {
            println!("----------------------------------------");
            println!("  Identify: {}", driver.identify);
            println!("  Version: {}", driver.package_version);
            println!("  Package: {}", driver.package_name);
            println!("----------------------------------------");
        }
        return Ok(());
    }
    if options.install_only {
        println!("Install-only: installing detected drivers without notifications");
        if let Err(error) = install_drivers(&drivers) {
            eprintln!("Failed to install driver package : {error}");
        }
        println!("Successfully installed driver package");
        return Ok(());
    }
    match interactive_notification().await? {
        Some(action) if action == ACTION_INSTALL_NOW => {
            install_drivers(&drivers)?;
            if let Err(error) = simple_notification(
                &linyaps_i18n::gettext("Graphics Driver Installation Completed"),
                &linyaps_i18n::gettext(
                    "Graphics driver package has been installed.\nRestart the Linyaps app to experience the performance improvement.",
                ),
            )
            .await
            {
                eprintln!("Failed to send success notification: {error}");
            }
        }
        Some(action) if action == ACTION_NOT_REMIND => {
            config.never_remind();
            let _ = config.save();
        }
        _ => {}
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let options = match parse_options_from(env::args_os()) {
        ParseOutcome::Run(options) => options,
        ParseOutcome::Help(program) => {
            print_help(&program);
            return;
        }
        ParseOutcome::Error { code, message } => {
            eprintln!("{message}\nRun with --help for more information.");
            std::process::exit(code);
        }
    };
    if let Err(error) = run(options).await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(version: &str) -> Value {
        serde_json::json!({ "version": version })
    }

    #[test]
    fn reads_and_normalizes_nvidia_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("version");
        fs::write(&path, "550.144.03\n").unwrap();
        assert_eq!(
            driver_version(&path).unwrap(),
            Some("550-144-03".to_string())
        );
        assert_eq!(
            driver_version(&directory.path().join("missing")).unwrap(),
            None
        );
    }

    #[test]
    fn selects_latest_remote_version_and_repo() {
        let value = serde_json::json!({
            "stable": [package("1.2.0")],
            "testing": [package("1.10.0")]
        });
        let selected = parse_remote_info("org.example.Driver", value.to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(selected.package_version, "1.10.0");
        assert_eq!(selected.repo_name, "testing");
    }

    #[test]
    fn accepts_array_shaped_remote_results_like_nlohmann_items() {
        let value = serde_json::json!([[package("1.0.0")], [package("2.0.0")]]);
        let selected = parse_remote_info("org.example.Driver", value.to_string().as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(selected.package_version, "2.0.0");
        assert_eq!(selected.repo_name, "1");
    }

    #[test]
    fn finds_installed_extension_version() {
        let value = serde_json::json!([
            { "id": "org.example.Other", "version": "1.0.0" },
            { "id": "org.example.Driver", "version": "2.0.0" }
        ]);
        assert_eq!(
            parse_installed_info("org.example.Driver", value.to_string().as_bytes()).unwrap(),
            Some("2.0.0".to_string())
        );
    }

    #[test]
    fn saves_upstream_config_shape() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("driver_detection.json");
        let mut manager = ConfigManager::load(path.clone());
        manager.never_remind();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "{\n    \"neverRemind\": true\n}"
        );
    }

    #[test]
    fn rejects_duplicate_process_lock_and_releases_it_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("driver.lock");
        let first = SingletonLock::acquire(&path).unwrap().unwrap();
        let duplicate = SingletonLock::acquire(&path).err().unwrap();
        assert_eq!(duplicate.kind(), io::ErrorKind::AlreadyExists);
        drop(first);
        assert!(SingletonLock::acquire(&path).unwrap().is_some());
    }

    #[test]
    fn installed_driver_detection_rechecks_remote_and_installed_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("version");
        fs::write(&path, "550.1\n").unwrap();
        let mut remote_calls = 0;
        let mut installed_calls = 0;
        let result = detect_nvidia_with(
            &path,
            |package_name| {
                remote_calls += 1;
                Ok(Some(GraphicsDriverInfo {
                    identify: NVIDIA_IDENTIFY.to_string(),
                    package_name: package_name.to_string(),
                    package_version: if remote_calls == 1 { "2.0.0" } else { "3.0.0" }.to_string(),
                    repo_name: "stable".to_string(),
                }))
            },
            |_| {
                installed_calls += 1;
                Ok(Some("1.0.0".to_string()))
            },
        )
        .unwrap();
        assert_eq!(result.package_version, "2.0.0");
        assert_eq!(remote_calls, 2);
        assert_eq!(installed_calls, 2);
    }

    #[test]
    fn parses_cli11_boolean_and_grouped_flag_forms() {
        assert_eq!(
            parse_options_from(["driver", "-fci"].map(OsString::from)),
            ParseOutcome::Run(Options {
                force: true,
                check_only: true,
                install_only: true,
            })
        );
        assert_eq!(
            parse_options_from(["driver", "--force=false"].map(OsString::from)),
            ParseOutcome::Run(Options::default())
        );
        assert_eq!(
            parse_options_from(["driver", "--force=yes"].map(OsString::from)),
            ParseOutcome::Run(Options {
                force: true,
                ..Options::default()
            })
        );
    }

    #[test]
    fn preserves_cli11_parse_errors_and_codes() {
        assert_eq!(
            parse_options_from(["driver", "--version"].map(OsString::from)),
            ParseOutcome::Error {
                code: 109,
                message: "The following argument was not expected: --version".to_string(),
            }
        );
        assert_eq!(
            parse_options_from(["driver", "--force=banana"].map(OsString::from)),
            ParseOutcome::Error {
                code: 104,
                message: "Could not convert: --force = banana".to_string(),
            }
        );
        assert_eq!(
            parse_options_from(["driver", "--bad", "foo", "bar"].map(OsString::from)),
            ParseOutcome::Error {
                code: 109,
                message: "The following arguments were not expected: bar foo --bad".to_string(),
            }
        );
    }
}
