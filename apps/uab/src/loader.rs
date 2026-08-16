use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use linyaps_api::PackageInfoV2;
use linyaps_core::apply_oci_configuration_patches;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

static RECEIVED_SIGNAL: AtomicI32 = AtomicI32::new(0);
static CHILD_PID: AtomicI32 = AtomicI32::new(0);

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

struct ContainerBundle {
    path: PathBuf,
}

impl Drop for ContainerBundle {
    fn drop(&mut self) {
        if env::var_os("LINGLONG_UAB_DEBUG").is_none() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn random_identifier() -> String {
    const CHARACTERS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0_u8; 16];
    let random = fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if random.is_ok() {
        for byte in &mut bytes {
            *byte = CHARACTERS[usize::from(*byte) % CHARACTERS.len()];
        }
    } else {
        let mut seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for byte in &mut bytes {
            *byte = CHARACTERS[(seed % CHARACTERS.len() as u128) as usize];
            seed = seed.rotate_left(7) ^ 0x9e3779b97f4a7c15_u128;
        }
    }
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn load_package_info(path: &Path) -> Result<PackageInfoV2, String> {
    let content = fs::read(path).map_err(|_| format!("couldn't open {}", path.display()))?;
    let value: Value =
        serde_json::from_slice(&content).map_err(|error| format!("parsing error: {error}"))?;
    if let Ok(info) = serde_json::from_value::<PackageInfoV2>(value.clone()) {
        return Ok(info);
    }
    let object = value
        .as_object()
        .ok_or_else(|| "legacy package info is not an object".to_string())?;
    let required_string = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| format!("legacy package info has no {key}"))
    };
    let arch = object
        .get("arch")
        .and_then(Value::as_array)
        .ok_or_else(|| "legacy package info has no arch".to_string())?
        .iter()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let command = object
        .get("command")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        });
    Ok(PackageInfoV2 {
        arch,
        base: required_string("base")?,
        channel: object
            .get("channel")
            .and_then(Value::as_str)
            .unwrap_or("main")
            .to_string(),
        command,
        compatible_version: None,
        description: object
            .get("description")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        extension_implementation: None,
        extensions: None,
        id: required_string("appid")?,
        kind: required_string("kind")?,
        module: required_string("module")?,
        name: required_string("name")?,
        permissions: object
            .get("permissions")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| format!("invalid package permissions: {error}"))?,
        runtime: object
            .get("runtime")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        schema_version: "1.0".to_string(),
        size: object
            .get("size")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        uuid: None,
        version: required_string("version")?,
    })
}

fn find_application(layers: &Path) -> Result<PackageInfoV2, String> {
    let entries = fs::read_dir(layers).map_err(|error| {
        format!("couldn't find directory 'layers', maybe filesystem error:{error}")
    })?;
    for layer in entries {
        let layer = layer.map_err(|error| format!("uab internal layer error: {error}"))?;
        if !layer
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let modules = fs::read_dir(layer.path()).map_err(|error| error.to_string())?;
        for module in modules {
            let module = module.map_err(|error| error.to_string())?;
            if module.file_name() != "binary"
                || !module
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_dir()
            {
                continue;
            }
            let info = load_package_info(&module.path().join("info.json"))?;
            if info.kind == "app" {
                return Ok(info);
            }
        }
    }
    Err("couldn't find meta info of application".to_string())
}

fn dependency_id(reference: &str) -> Result<String, String> {
    let after_channel = reference
        .split_once(':')
        .map_or(reference, |(_, reference)| reference);
    let id = after_channel.split('/').next().unwrap_or_default();
    if id.is_empty() {
        Err(format!("runtime may not valid: {reference}"))
    } else {
        Ok(id.to_string())
    }
}

fn compatible_path(bundle: &Path, layer_id: &str) -> Result<PathBuf, String> {
    let layer = bundle.join("layers").join(layer_id);
    if !layer.exists() {
        return Err(format!("{} not exist.", layer.display()));
    }
    for module in ["runtime", "binary"] {
        let files = layer.join(module).join("files");
        if files.exists() {
            return Ok(files);
        }
    }
    Err("layer runtime doesn't exist.".to_string())
}

fn architecture_triplet(architecture: &str) -> Result<&'static str, String> {
    match architecture {
        "x86_64" => Ok("x86_64-linux-gnu"),
        "arm64" | "aarch64" => Ok("aarch64-linux-gnu"),
        "loong64" | "loongarch64" => Ok("loongarch64-linux-gnu"),
        "sw64" => Ok("sw_64-linux-gnu"),
        "mips64" => Ok("mips64el-linux-gnuabi64"),
        _ => Err("unsupported architecture".to_string()),
    }
}

fn ld_configuration(
    triplet: &str,
    runtime: Option<&Path>,
    application: &Path,
    app_id: &str,
) -> String {
    let mut factors = runtime
        .into_iter()
        .chain(std::iter::once(application))
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    factors.sort();
    let mut hasher = Sha256::new();
    for factor in factors {
        hasher.update(factor.as_bytes());
    }
    let mut output = format!("# {:x}\n", hasher.finalize());
    if runtime.is_some() {
        output.push_str(&format!(
            "/runtime/lib\n/runtime/lib/{triplet}\ninclude /runtime/etc/ld.so.conf\n"
        ));
    }
    let prefix = format!("/opt/apps/{app_id}/files");
    output.push_str(&format!(
        "{prefix}/lib\n{prefix}/lib/{triplet}\ninclude {prefix}/etc/ld.so.conf\n"
    ));
    output
}

fn quote_bash_argument(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn write_entrypoint(path: &Path, arguments: &[String]) -> Result<(), String> {
    let mut content = "#!/usr/bin/env bash\nsource /etc/profile\nexec ".to_string();
    for argument in arguments {
        content.push_str(&quote_bash_argument(argument));
        content.push(' ');
    }
    fs::write(path, content).map_err(|error| error.to_string())?;
    let mut permissions = fs::metadata(path)
        .map_err(|error| error.to_string())?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o100);
    fs::set_permissions(path, permissions).map_err(|error| error.to_string())
}

fn inherited_environment(app_id: &str) -> BTreeMap<String, String> {
    let mut environment = env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect::<BTreeMap<_, _>>();
    environment.insert("LINGLONG_APPID".to_string(), app_id.to_string());
    environment
}

fn write_environment_script(
    path: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut output = String::new();
    for (key, value) in environment {
        output.push_str("export ");
        output.push_str(key);
        output.push_str("='");
        output.push_str(&value.replace('\'', "'\\''"));
        output.push_str("'\n");
    }
    fs::write(path, output).map_err(|error| error.to_string())
}

fn mount(source: impl AsRef<Path>, destination: impl AsRef<Path>, options: &[&str]) -> Value {
    json!({
        "destination": destination.as_ref(),
        "options": options,
        "source": source.as_ref(),
        "type": "bind"
    })
}

fn tmpfs(destination: impl AsRef<Path>) -> Value {
    json!({
        "destination": destination.as_ref(),
        "options": ["nodev", "nosuid", "mode=700"],
        "source": "tmpfs",
        "type": "tmpfs"
    })
}

fn root_mounts(random_file_name: &str, generated_profile: bool) -> Result<Vec<Value>, String> {
    let mut mounts = Vec::new();
    for entry in fs::read_dir("/").map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some("etc" | "opt" | "runtime" | "tmp")) {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_symlink() {
            mounts.push(mount(&path, &path, &["rbind", "rw", "copy-symlink"]));
        } else {
            mounts.push(mount(&path, &path, &["rbind", "rw"]));
        }
    }
    mounts.push(tmpfs("/etc"));
    for entry in fs::read_dir("/etc").map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        if name == "ld.so.cache"
            || name == "ld.so.conf.d"
            || name == random_file_name
            || (generated_profile && name == "profile")
        {
            continue;
        }
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_symlink() {
            mounts.push(mount(&path, &path, &["rbind", "rw", "copy-symlink"]));
        } else {
            mounts.push(mount(&path, &path, &["rbind", "rw"]));
        }
    }
    mounts.push(tmpfs("/etc/ld.so.conf.d"));
    if let Ok(entries) = fs::read_dir("/etc/ld.so.conf.d") {
        for entry in entries {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            if entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_symlink()
            {
                mounts.push(mount(&path, &path, &["rbind", "rw", "copy-symlink"]));
            } else {
                mounts.push(mount(&path, &path, &["rbind", "rw"]));
            }
        }
    }
    Ok(mounts)
}

struct ConfigurationInput<'a> {
    app: &'a PackageInfoV2,
    runtime: Option<&'a Path>,
    application: &'a Path,
    extra: &'a Path,
    entrypoint: &'a Path,
    ld_cache: &'a Path,
    ld_conf: &'a Path,
    random_file: &'a Path,
    environment_script: &'a Path,
}

fn build_configuration(input: ConfigurationInput<'_>) -> Result<Value, String> {
    let profile = input.extra.join("profile");
    let triplet_list = input.extra.join("linglong-triplet-list");
    let generated_profile = triplet_list.exists() && profile.exists();
    let random_name = input
        .random_file
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "invalid random mount name".to_string())?;
    let mut mounts = root_mounts(random_name, generated_profile)?;
    if let Some(runtime) = input.runtime {
        mounts.push(mount(runtime, "/runtime", &["rbind", "ro", "rslave"]));
    }
    mounts.push(tmpfs("/opt"));
    mounts.push(mount(
        input.application,
        PathBuf::from("/opt/apps").join(&input.app.id).join("files"),
        &["rbind", "ro", "rslave"],
    ));
    mounts.push(mount("/tmp", "/tmp", &["rbind"]));
    mounts.push(mount(input.ld_cache, "/etc/ld.so.cache", &["bind"]));
    mounts.push(mount(
        input.random_file,
        PathBuf::from("/etc").join(random_name),
        &["ro", "bind"],
    ));
    mounts.push(mount(
        input.ld_conf,
        "/etc/ld.so.conf.d/zz_deepin-linglong.ld.so.conf",
        &["ro", "bind"],
    ));
    if triplet_list.exists() {
        mounts.push(mount(
            &triplet_list,
            "/etc/linglong-triplet-list",
            &["ro", "bind"],
        ));
        if profile.exists() {
            mounts.push(mount(&profile, "/etc/profile", &["ro", "bind"]));
        }
    }
    mounts.push(mount(input.entrypoint, "/entrypoint.sh", &["bind", "ro"]));
    mounts.push(mount(
        input.environment_script,
        "/etc/profile.d/00env.sh",
        &["bind", "ro"],
    ));
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let environment = inherited_environment(&input.app.id)
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    let mut configuration = json!({
        "ociVersion": "1.0.1",
        "hostname": "linglong",
        "root": {"path": "rootfs", "readonly": false},
        "process": {
            "terminal": io::stdout().is_terminal(),
            "user": {"uid": uid, "gid": gid},
            "args": ["/entrypoint.sh"],
            "env": environment,
            "cwd": "/"
        },
        "mounts": mounts,
        "hooks": {
            "startContainer": [
                {"path": "/sbin/ldconfig", "args": ["/sbin/ldconfig", "-C", "/tmp/ld.so.cache"]},
                {"path": "/bin/sh", "args": ["/bin/sh", "-c", "cat /tmp/ld.so.cache > /etc/ld.so.cache"]}
            ]
        },
        "linux": {
            "rootfsPropagation": "slave",
            "namespaces": [{"type": "user"}, {"type": "mount"}],
            "uidMappings": [{"containerID": uid, "hostID": uid, "size": 1}],
            "gidMappings": [{"containerID": gid, "hostID": gid, "size": 1}],
            "maskedPaths": []
        }
    });
    apply_oci_configuration_patches(
        &mut configuration,
        &input.app.id,
        Path::new("/usr/lib/linglong/container/config.d"),
    )?;
    Ok(configuration)
}

fn execute(arguments: &[String]) -> Result<i32, String> {
    install_signal_handlers().map_err(|error| error.to_string())?;
    let executable = fs::read_link("/proc/self/exe").map_err(|error| error.to_string())?;
    let bundle = executable
        .parent()
        .ok_or_else(|| "loader has no parent directory".to_string())?
        .to_path_buf();
    let layers = bundle.join("layers");
    if !layers.exists() {
        return Err("couldn't find directory 'layers', maybe filesystem error:".to_string());
    }
    let app = find_application(&layers)?;
    if app.base.is_empty() {
        return Err("couldn't find base of application".to_string());
    }
    let container_id = random_identifier();
    let container_path = bundle
        .parent()
        .ok_or_else(|| "bundle has no parent directory".to_string())?
        .join(&container_id);
    fs::create_dir_all(container_path.join("rootfs")).map_err(|error| {
        format!(
            "couldn't create directory {} :{error}",
            container_path.display()
        )
    })?;
    let container = ContainerBundle {
        path: container_path,
    };
    let extra = bundle.join("extra");
    if !extra.exists() {
        return Err(format!("{} not exist.", extra.display()));
    }
    let box_binary = extra.join("ll-box");
    if !box_binary.exists() {
        return Err(format!("{} not exist.", box_binary.display()));
    }
    let runtime = app
        .runtime
        .as_deref()
        .map(dependency_id)
        .transpose()?
        .map(|id| compatible_path(&bundle, &id))
        .transpose()?;
    let application = compatible_path(&bundle, &app.id)?;
    let triplet = architecture_triplet(
        app.arch
            .first()
            .ok_or_else(|| "unsupported architecture".to_string())?,
    )?;
    let ld_cache = container.path.join("ld.so.cache");
    fs::write(&ld_cache, []).map_err(|error| error.to_string())?;
    let ld_conf = container.path.join("ld.so.conf");
    fs::write(
        &ld_conf,
        ld_configuration(triplet, runtime.as_deref(), &application, &app.id),
    )
    .map_err(|error| error.to_string())?;
    let random_file = container.path.join(random_identifier());
    fs::write(&random_file, []).map_err(|error| error.to_string())?;
    let mut command = app
        .command
        .clone()
        .unwrap_or_else(|| vec!["/bin/bash".to_string()]);
    command.extend(arguments.iter().cloned());
    let entrypoint = container.path.join("entrypoint.sh");
    write_entrypoint(&entrypoint, &command)?;
    let environment_script = container.path.join("00env.sh");
    write_environment_script(&environment_script, &inherited_environment(&app.id))?;
    let configuration = build_configuration(ConfigurationInput {
        app: &app,
        runtime: runtime.as_deref(),
        application: &application,
        extra: &extra,
        entrypoint: &entrypoint,
        ld_cache: &ld_cache,
        ld_conf: &ld_conf,
        random_file: &random_file,
        environment_script: &environment_script,
    })?;
    fs::write(
        container.path.join("config.json"),
        format!(
            "{}\n",
            serde_json::to_string(&configuration).map_err(|error| error.to_string())?
        ),
    )
    .map_err(|error| error.to_string())?;
    if env::var_os("LINGLONG_UAB_DEBUG").is_some() {
        println!("dump container config:");
        println!(
            "{}",
            serde_json::to_string_pretty(&configuration).map_err(|error| error.to_string())?
        );
    }
    let mut child = Command::new(&box_binary)
        .arg("--cgroup-manager=disabled")
        .arg("run")
        .arg(format!("--bundle={}", container.path.display()))
        .arg("--config=config.json")
        .arg(&container_id)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("failed to execute {}: {error}", box_binary.display()))?;
    CHILD_PID.store(child.id() as i32, Ordering::Relaxed);
    let status = child
        .wait()
        .map_err(|error| format!("waitpid() err:{error}"))?;
    CHILD_PID.store(0, Ordering::Relaxed);
    let received_signal = RECEIVED_SIGNAL.load(Ordering::Relaxed);
    if received_signal != 0 {
        return Ok(128 + received_signal);
    }
    if let Some(code) = status.code() {
        eprintln!("loader: container exit: {code}");
        return Ok(code);
    }
    if let Some(signal) = status.signal() {
        eprintln!("loader: container exit with signal: {signal}");
        return Ok(128 + signal);
    }
    Err("unknown exit status".to_string())
}

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let code = match execute(&arguments) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            -1
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_bash_arguments() {
        assert_eq!(quote_bash_argument(""), "''");
        assert_eq!(quote_bash_argument("hello world"), "'hello world'");
        assert_eq!(quote_bash_argument("let's go"), "'let'\\''s go'");
    }

    #[test]
    fn parses_dependency_identifiers() {
        assert_eq!(
            dependency_id("main:org.deepin.Runtime/23.1.0").unwrap(),
            "org.deepin.Runtime"
        );
        assert_eq!(
            dependency_id("org.deepin.Runtime/23.1.0").unwrap(),
            "org.deepin.Runtime"
        );
    }

    #[test]
    fn maps_supported_architectures() {
        assert_eq!(architecture_triplet("x86_64").unwrap(), "x86_64-linux-gnu");
        assert_eq!(
            architecture_triplet("loongarch64").unwrap(),
            "loongarch64-linux-gnu"
        );
        assert!(architecture_triplet("unknown").is_err());
    }
}
