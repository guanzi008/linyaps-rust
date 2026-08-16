use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use linyaps_core::Architecture;
use serde_json::{Value, json};

use crate::source::{clear_path, copy_tree};

pub struct ContainerExtension {
    pub source: PathBuf,
    pub id: String,
    pub environment: BTreeMap<String, String>,
    pub devices: Vec<(PathBuf, PathBuf)>,
}

pub struct ContainerRequest<'a> {
    pub base: &'a Path,
    pub runtime: Option<&'a Path>,
    pub project_directory: &'a Path,
    pub internal_directory: &'a Path,
    pub application: Option<(&'a Path, &'a str)>,
    pub output: Option<(&'a Path, &'a str)>,
    pub extensions: Vec<ContainerExtension>,
    pub arguments: Vec<String>,
    pub working_directory: String,
    pub isolate_network: bool,
    pub writable_root: bool,
}

pub fn run(request: ContainerRequest<'_>) -> Result<i32> {
    run_inner(request, None)
}

pub struct ContainerWriteback<'a> {
    pub root: &'a Path,
    pub runtime: Option<&'a Path>,
}

pub fn run_with_writeback(
    request: ContainerRequest<'_>,
    writeback: ContainerWriteback<'_>,
) -> Result<i32> {
    run_inner(request, Some(writeback))
}

fn run_inner(
    request: ContainerRequest<'_>,
    writeback: Option<ContainerWriteback<'_>>,
) -> Result<i32> {
    let identifier = random_identifier();
    let bundle = request
        .internal_directory
        .join("containers")
        .join(&identifier);
    clear_path(&bundle)?;
    fs::create_dir_all(&bundle)?;
    let rootfs = bundle.join("rootfs");
    copy_tree(request.base, &rootfs).with_context(|| {
        format!(
            "failed to prepare base rootfs from {}",
            request.base.display()
        )
    })?;
    let mut mounts = vec![
        bind_mount(request.project_directory, "/project", &["rbind", "rw"]),
        json!({
            "destination": "/proc",
            "type": "proc",
            "source": "proc",
            "options": ["nosuid", "noexec", "nodev"]
        }),
        bind_mount("/dev", "/dev", &["rbind", "rw", "rslave"]),
        bind_mount("/sys", "/sys", &["rbind", "ro", "rslave"]),
        bind_mount("/tmp", "/tmp", &["rbind", "rw", "rslave"]),
    ];
    if Path::new("/etc/resolv.conf").exists() {
        mounts.push(bind_mount(
            "/etc/resolv.conf",
            "/etc/resolv.conf",
            &["bind", "ro"],
        ));
    }
    let runtime_root = if let Some(runtime) = request.runtime {
        if writeback.as_ref().and_then(|value| value.runtime).is_some() {
            let copied = bundle.join("runtime");
            copy_tree(runtime, &copied).with_context(|| {
                format!("failed to prepare runtime root from {}", runtime.display())
            })?;
            mounts.push(bind_mount(&copied, "/runtime", &["rbind", "rw", "rslave"]));
            Some(copied)
        } else {
            mounts.push(bind_mount(runtime, "/runtime", &["rbind", "ro", "rslave"]));
            None
        }
    } else {
        None
    };
    if let Some((application, identifier)) = request.application {
        mounts.push(bind_mount(
            application,
            format!("/opt/apps/{identifier}/files"),
            &["rbind", "ro", "rslave"],
        ));
    }
    if let Some((output, prefix)) = request.output {
        mounts.push(bind_mount(output, prefix, &["rbind", "rw", "rslave"]));
    }
    for extension in &request.extensions {
        mounts.push(bind_mount(
            &extension.source,
            format!("/opt/extensions/{}", extension.id),
            &["rbind", "ro", "rslave"],
        ));
        for (source, destination) in &extension.devices {
            mounts.push(bind_mount(source, destination, &["rbind", "rw", "rslave"]));
        }
    }
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mut namespaces = vec![
        json!({"type": "user"}),
        json!({"type": "mount"}),
        json!({"type": "pid"}),
        json!({"type": "ipc"}),
        json!({"type": "uts"}),
    ];
    if request.isolate_network {
        namespaces.push(json!({"type": "network"}));
    }
    let mut environment = inherited_environment();
    for extension in &request.extensions {
        let prefix = format!("/opt/extensions/{}", extension.id);
        for (key, value) in &extension.environment {
            let origin = environment.get(key).cloned().unwrap_or_default();
            environment.insert(
                key.clone(),
                value
                    .replace("$PREFIX", &prefix)
                    .replace("$ORIGIN", &origin),
            );
        }
    }
    environment.insert(
        "TRIPLET".to_string(),
        Architecture::current()?.triplet().to_string(),
    );
    if let Some((_, prefix)) = request.output {
        environment.insert("PREFIX".to_string(), prefix.to_string());
        environment.insert(
            "LINGLONG_LD_SO_CACHE".to_string(),
            "/etc/ld.so.cache".to_string(),
        );
    }
    let configuration = json!({
        "ociVersion": "1.0.1",
        "hostname": "linglong",
        "root": {"path": "rootfs", "readonly": !request.writable_root},
        "process": {
            "terminal": io::stdout().is_terminal(),
            "user": {"uid": uid, "gid": gid},
            "args": request.arguments,
            "env": environment.into_iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>(),
            "cwd": request.working_directory,
            "noNewPrivileges": true,
            "capabilities": {
                "bounding": builder_capabilities(),
                "effective": builder_capabilities(),
                "inheritable": [],
                "permitted": builder_capabilities(),
                "ambient": []
            }
        },
        "mounts": mounts,
        "linux": {
            "namespaces": namespaces,
            "uidMappings": [{"containerID": uid, "hostID": uid, "size": 1}],
            "gidMappings": [{"containerID": gid, "hostID": gid, "size": 1}],
            "maskedPaths": []
        }
    });
    fs::write(
        bundle.join("config.json"),
        format!("{}\n", serde_json::to_string(&configuration)?),
    )?;
    let state = request.internal_directory.join("box");
    fs::create_dir_all(&state)?;
    let executable = box_executable();
    let status = Command::new(&executable)
        .arg("--root")
        .arg(&state)
        .arg("--cgroup-manager=disabled")
        .arg("run")
        .arg(format!("--bundle={}", bundle.display()))
        .arg("--config=config.json")
        .arg(&identifier)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to execute {}", executable.display()))?;
    if let Some(writeback) = writeback {
        replace_tree(&rootfs, writeback.root)?;
        if let Some(destination) = writeback.runtime {
            let source = runtime_root.context("runtime writeback requested without runtime")?;
            replace_tree(&source, destination)?;
        }
    }
    if env::var_os("LINGLONG_BUILDER_DEBUG").is_none() {
        let _ = clear_path(&bundle);
    }
    if let Some(code) = status.code() {
        return Ok(code);
    }
    if let Some(signal) = status.signal() {
        return Ok(128 + signal);
    }
    bail!("container has no exit status")
}

fn replace_tree(source: &Path, destination: &Path) -> Result<()> {
    clear_path(destination)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_tree(source, destination)?;
            clear_path(source)?;
            Ok(())
        }
    }
}

fn bind_mount(source: impl AsRef<Path>, destination: impl AsRef<Path>, options: &[&str]) -> Value {
    json!({
        "destination": destination.as_ref(),
        "type": "bind",
        "source": source.as_ref(),
        "options": options
    })
}

fn inherited_environment() -> BTreeMap<String, String> {
    env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

fn builder_capabilities() -> Vec<&'static str> {
    vec![
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_FOWNER",
        "CAP_FSETID",
        "CAP_KILL",
        "CAP_NET_BIND_SERVICE",
        "CAP_SETFCAP",
        "CAP_SETGID",
        "CAP_SETPCAP",
        "CAP_SETUID",
        "CAP_SYS_CHROOT",
    ]
}

fn box_executable() -> PathBuf {
    if let Some(path) = env::var_os("LINGLONG_BOX") {
        return PathBuf::from(path);
    }
    if let Ok(current) = env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("ll-box");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("ll-box")
}

fn random_identifier() -> String {
    let mut bytes = [0_u8; 16];
    if fs::read("/proc/sys/kernel/random/uuid")
        .map(|value| {
            for (index, byte) in value
                .into_iter()
                .filter(u8::is_ascii_hexdigit)
                .take(16)
                .enumerate()
            {
                bytes[index] = byte;
            }
        })
        .is_err()
    {
        bytes.copy_from_slice(&std::process::id().to_le_bytes().repeat(4)[..16]);
    }
    let value = String::from_utf8_lossy(&bytes);
    format!("ll-builder-{}-{}", std::process::id(), value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_match_upstream_builder_set() {
        assert_eq!(builder_capabilities().len(), 11);
        assert!(builder_capabilities().contains(&"CAP_SYS_CHROOT"));
    }
}
