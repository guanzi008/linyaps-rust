use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use linyaps_api::RunContextConfig;
use linyaps_core::{Reference, cdi};
use linyaps_repository::LocalRepository;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn initialize(
    repository: &LocalRepository,
    config_json: &str,
    container_id: &str,
) -> Result<()> {
    let runtime = env::var_os("LINGLONG_OCI_RUNTIME").unwrap_or_else(|| "ll-box".into());
    initialize_with_runtime(repository, config_json, container_id, Path::new(&runtime))
}

fn initialize_with_runtime(
    repository: &LocalRepository,
    config_json: &str,
    container_id: &str,
    runtime: &Path,
) -> Result<()> {
    let config: RunContextConfig =
        serde_json::from_str(config_json).context("failed to parse run context config")?;
    if config.version != "1" {
        bail!(
            "run context config version mismatch: config version {}, expected version 1",
            config.version
        );
    }
    let expected_id = container_id_for(&config)?;
    if expected_id != container_id {
        bail!("container id mismatch");
    }
    let resolved = resolve(repository, &config)?;
    let target_item = repository.layer_item(&resolved.target, "binary")?;
    let cache = repository
        .root()
        .join("cache")
        .join(&target_item.commit)
        .join(container_id);
    let saved_config = cache.join(".config");
    if saved_config.exists() {
        return Ok(());
    }
    fs::create_dir_all(&cache)
        .with_context(|| format!("failed to create container cache {}", cache.display()))?;
    let ld_conf = make_ld_conf(&resolved)?;
    fs::write(cache.join("ld.so.conf"), ld_conf)?;
    if !cache.join("ld.so.cache").exists() {
        fs::write(cache.join("ld.so.cache"), [])?;
    }

    let bundle = runtime_directory().join(format!("{container_id}.init"));
    clear_path(&bundle)?;
    let _cleanup = BundleCleanup(bundle.clone());
    let rootfs = bundle.join("rootfs");
    fs::create_dir_all(&rootfs)?;
    copy_tree(&resolved.base, &rootfs).with_context(|| {
        format!(
            "failed to prepare init rootfs from {}",
            resolved.base.display()
        )
    })?;
    prepare_mount_points(&rootfs, &resolved)?;
    let configuration = oci_configuration(&resolved, &cache, &config)?;
    fs::write(
        bundle.join("config.json"),
        format!("{}\n", serde_json::to_string(&configuration)?),
    )?;
    let state = runtime_directory().join("box");
    fs::create_dir_all(&state)?;
    let status = Command::new(runtime)
        .arg("--root")
        .arg(&state)
        .arg("--cgroup-manager=disabled")
        .arg("run")
        .arg(format!("--bundle={}", bundle.display()))
        .arg("--config=config.json")
        .arg(format!("{container_id}.init"))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to execute {}", runtime.display()))?;
    if !status.success() {
        bail!("init run context exited with code {status}");
    }
    let temporary = cache.join(format!(".config.{}.tmp", std::process::id()));
    fs::write(&temporary, config_json)?;
    fs::rename(&temporary, &saved_config)?;
    Ok(())
}

struct ResolvedContext {
    base: PathBuf,
    runtime: Option<(Reference, PathBuf)>,
    app: Option<(Reference, PathBuf)>,
    extensions: Vec<(Reference, PathBuf)>,
    target: Reference,
}

fn resolve(repository: &LocalRepository, config: &RunContextConfig) -> Result<ResolvedContext> {
    let base_reference =
        parse_reference(config.base.as_deref().context("base layer is required")?)?;
    let base = layer_files(repository, &base_reference)?;
    let runtime = config
        .runtime
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| {
            let reference = parse_reference(value)?;
            let path = layer_files(repository, &reference)?;
            Ok::<_, anyhow::Error>((reference, path))
        })
        .transpose()?;
    let app = config
        .app
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| {
            let reference = parse_reference(value)?;
            let path = layer_files(repository, &reference)?;
            Ok::<_, anyhow::Error>((reference, path))
        })
        .transpose()?;
    let mut seen = BTreeSet::new();
    let mut extensions = Vec::new();
    for value in config
        .extensions
        .iter()
        .flat_map(|extensions| extensions.values())
        .flatten()
    {
        let reference = parse_reference(value)?;
        if !seen.insert(reference.to_string()) {
            continue;
        }
        let item = repository.layer_item(&reference, "binary")?;
        if item.info.kind != "extension" {
            bail!("invalid extension kind in config.extensions");
        }
        extensions.push((reference.clone(), layer_files(repository, &reference)?));
    }
    let target = app
        .as_ref()
        .map(|(reference, _)| reference.clone())
        .or_else(|| runtime.as_ref().map(|(reference, _)| reference.clone()))
        .unwrap_or_else(|| base_reference.clone());
    Ok(ResolvedContext {
        base,
        runtime,
        app,
        extensions,
        target,
    })
}

fn parse_reference(value: &str) -> Result<Reference> {
    value
        .parse::<Reference>()
        .with_context(|| format!("failed to parse layer reference {value}"))
}

fn layer_files(repository: &LocalRepository, reference: &Reference) -> Result<PathBuf> {
    let path = repository.merged_layer_path(reference)?.join("files");
    if !path.is_dir() {
        bail!("layer {reference} has no files directory");
    }
    Ok(path)
}

fn container_id_for(config: &RunContextConfig) -> Result<String> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(config)?);
    Ok(format!("{:x}", hasher.finalize()))
}

fn make_ld_conf(context: &ResolvedContext) -> Result<String> {
    let triplet = context.target.architecture.triplet();
    let mut factors = Vec::new();
    let mut content = String::new();
    if let Some((_, path)) = &context.runtime {
        append_ld_prefix(&mut content, "/runtime", triplet);
        factors.push(path.to_string_lossy().into_owned());
    }
    if let Some((reference, path)) = &context.app {
        append_ld_prefix(
            &mut content,
            &format!("/opt/apps/{}/files", reference.id),
            triplet,
        );
        factors.push(path.to_string_lossy().into_owned());
    }
    for (reference, path) in &context.extensions {
        append_ld_prefix(
            &mut content,
            &format!("/opt/extensions/{}", reference.id),
            triplet,
        );
        factors.push(path.to_string_lossy().into_owned());
    }
    factors.sort();
    let mut hasher = Sha256::new();
    for factor in factors {
        hasher.update(factor.as_bytes());
    }
    Ok(format!("# {:x}\n{content}", hasher.finalize()))
}

fn append_ld_prefix(content: &mut String, prefix: &str, triplet: &str) {
    content.push_str(prefix);
    content.push_str("/lib\n");
    content.push_str(prefix);
    content.push_str("/lib/");
    content.push_str(triplet);
    content.push('\n');
    content.push_str("include ");
    content.push_str(prefix);
    content.push_str("/etc/ld.so.conf\n");
}

fn prepare_mount_points(rootfs: &Path, context: &ResolvedContext) -> Result<()> {
    fs::create_dir_all(rootfs.join("run/linglong/cache"))?;
    fs::create_dir_all(rootfs.join("etc/ld.so.conf.d"))?;
    replace_with_file(&rootfs.join("etc/ld.so.cache"))?;
    replace_with_file(&rootfs.join("etc/ld.so.conf.d/zz_deepin-linglong-app.conf"))?;
    if context.runtime.is_some() {
        replace_with_directory(&rootfs.join("runtime"))?;
    }
    if let Some((reference, _)) = &context.app {
        replace_with_directory(&rootfs.join("opt/apps").join(&reference.id).join("files"))?;
    }
    for (reference, _) in &context.extensions {
        replace_with_directory(&rootfs.join("opt/extensions").join(&reference.id))?;
    }
    Ok(())
}

fn replace_with_file(path: &Path) -> Result<()> {
    clear_path(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, [])?;
    Ok(())
}

fn replace_with_directory(path: &Path) -> Result<()> {
    clear_path(path)?;
    fs::create_dir_all(path)?;
    Ok(())
}

fn oci_configuration(
    context: &ResolvedContext,
    cache: &Path,
    config: &RunContextConfig,
) -> Result<Value> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let mut mounts = vec![
        json!({"destination":"/proc","type":"proc","source":"proc"}),
        json!({"destination":"/dev","type":"tmpfs","source":"tmpfs","options":["nosuid","strictatime","mode=755","size=65536k"]}),
        json!({"destination":"/dev/pts","type":"devpts","source":"devpts","options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620"]}),
        json!({"destination":"/dev/shm","type":"tmpfs","source":"shm","options":["nosuid","noexec","nodev","mode=1777","size=65536k"]}),
        bind_mount(cache, "/run/linglong/cache", false),
        bind_mount(
            &cache.join("ld.so.conf"),
            "/etc/ld.so.conf.d/zz_deepin-linglong-app.conf",
            true,
        ),
    ];
    if let Some((_, path)) = &context.runtime {
        mounts.push(bind_mount(path, "/runtime", true));
    }
    if let Some((reference, path)) = &context.app {
        mounts.push(bind_mount(
            path,
            &format!("/opt/apps/{}/files", reference.id),
            true,
        ));
    }
    for (reference, path) in &context.extensions {
        mounts.push(bind_mount(
            path,
            &format!("/opt/extensions/{}", reference.id),
            true,
        ));
    }
    let mut environment = BTreeMap::from([
        (
            "PATH".to_string(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ),
        ("LINYAPS_INIT_SKIP_LOCK".to_string(), "YES".to_string()),
    ]);
    let mut hooks = BTreeMap::<String, Vec<Value>>::new();
    for device in config.cdi_devices.as_deref().into_iter().flatten() {
        let edits = cdi::get_device_edits(device).with_context(|| {
            format!(
                "failed to resolve CDI device edits {}={}",
                device.kind, device.name
            )
        })?;
        for value in edits.env.as_deref().into_iter().flatten() {
            if let Some((key, value)) = value.split_once('=') {
                environment
                    .entry(key.to_string())
                    .or_insert_with(|| value.to_string());
            }
        }
        for node in edits.device_nodes.as_deref().into_iter().flatten() {
            mounts.push(simple_bind_mount(
                Path::new(node.host_path.as_deref().unwrap_or(&node.path)),
                &node.path,
            ));
        }
        for mount in edits.mounts.as_deref().into_iter().flatten() {
            let options = mount.options.clone().unwrap_or_default();
            let mount_type = mount.mount_type.clone().unwrap_or_else(|| {
                if options
                    .iter()
                    .any(|option| matches!(option.as_str(), "bind" | "rbind"))
                {
                    "bind".to_string()
                } else {
                    "none".to_string()
                }
            });
            mounts.push(json!({
                "destination": mount.container_path,
                "type": mount_type,
                "source": mount.host_path,
                "options": options,
            }));
        }
        for hook in edits.hooks.as_deref().into_iter().flatten() {
            let mut value = json!({"path": hook.path});
            if let Some(arguments) = &hook.args {
                value["args"] = json!(arguments);
            }
            if let Some(hook_environment) = &hook.env {
                value["env"] = json!(hook_environment);
            }
            if let Some(timeout) = hook.timeout {
                value["timeout"] = json!(timeout);
            }
            hooks.entry(hook.hook_name.clone()).or_default().push(value);
        }
    }
    let mut configuration = json!({
        "ociVersion": "1.0.1",
        "root": {"path": "rootfs", "readonly": false},
        "hostname": "linglong",
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": ["/sbin/ldconfig", "-X", "-C", "/run/linglong/cache/ld.so.cache"],
            "env": environment.into_iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>(),
            "cwd": "/",
            "capabilities": {"bounding": [], "effective": [], "inheritable": [], "permitted": [], "ambient": []},
            "noNewPrivileges": true
        },
        "mounts": mounts,
        "linux": {
            "namespaces": [{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"},{"type":"user"}],
            "uidMappings": [{"containerID": 0, "hostID": uid, "size": 1}],
            "gidMappings": [{"containerID": 0, "hostID": gid, "size": 1}],
            "maskedPaths": []
        }
    });
    if !hooks.is_empty() {
        configuration["hooks"] = serde_json::to_value(hooks)?;
    }
    Ok(configuration)
}

fn bind_mount(source: &Path, destination: &str, read_only: bool) -> Value {
    let mut options = vec!["rbind", "rprivate"];
    if read_only {
        options.push("ro");
    } else {
        options.push("rw");
    }
    json!({
        "destination": destination,
        "type": "none",
        "source": source,
        "options": options
    })
}

fn simple_bind_mount(source: &Path, destination: &str) -> Value {
    json!({
        "destination": destination,
        "type": "bind",
        "source": source,
        "options": ["bind"]
    })
}

fn runtime_directory() -> PathBuf {
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/linglong-runtime-{}", unsafe {
                libc::getuid()
            }))
        })
        .join("linglong")
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.is_dir() {
            copy_tree(&source_path, &destination_path)?;
            fs::set_permissions(&destination_path, metadata.permissions())?;
        } else if metadata.file_type().is_symlink() {
            symlink(fs::read_link(&source_path)?, &destination_path)?;
        } else if metadata.is_file() {
            if fs::hard_link(&source_path, &destination_path).is_err() {
                fs::copy(&source_path, &destination_path)?;
            }
            fs::set_permissions(
                &destination_path,
                fs::Permissions::from_mode(metadata.permissions().mode()),
            )?;
        }
    }
    Ok(())
}

fn clear_path(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

struct BundleCleanup(PathBuf);

impl Drop for BundleCleanup {
    fn drop(&mut self) {
        let _ = clear_path(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linyaps_api::PackageInfoV2;
    use linyaps_repository::reference_from_info;

    #[test]
    fn container_id_is_sha256_of_canonical_config() {
        let config = RunContextConfig {
            app: Some("stable/org.example.App/1.0.0.0/x86_64".to_string()),
            base: Some("stable/org.example.Base/1.0.0.0/x86_64".to_string()),
            cdi_devices: None,
            extensions: Some(Default::default()),
            instance: None,
            mounts: None,
            overlayfs: None,
            resolv_conf: None,
            runtime: None,
            timezone: None,
            version: "1".to_string(),
        };
        let bytes = serde_json::to_vec(&config).unwrap();
        let expected = format!("{:x}", Sha256::digest(bytes));
        assert_eq!(container_id_for(&config).unwrap(), expected);
    }

    #[test]
    fn ld_config_contains_runtime_app_and_extension_paths() {
        let architecture = linyaps_core::Architecture::X86_64;
        let reference = |id: &str| Reference {
            channel: "stable".to_string(),
            id: id.to_string(),
            version: linyaps_core::Version::parse("1.0.0.0").unwrap(),
            architecture,
        };
        let context = ResolvedContext {
            base: PathBuf::from("/layers/base/files"),
            runtime: Some((
                reference("org.example.Runtime"),
                PathBuf::from("/layers/runtime/files"),
            )),
            app: Some((
                reference("org.example.App"),
                PathBuf::from("/layers/app/files"),
            )),
            extensions: vec![(
                reference("org.example.Extension"),
                PathBuf::from("/layers/ext/files"),
            )],
            target: reference("org.example.App"),
        };
        let config = make_ld_conf(&context).unwrap();
        assert!(config.contains("/runtime/lib/x86_64-linux-gnu"));
        assert!(config.contains("/opt/apps/org.example.App/files/lib"));
        assert!(config.contains("/opt/extensions/org.example.Extension/lib"));
    }

    #[test]
    fn init_container_writes_ld_cache_through_cache_directory() {
        let reference = Reference {
            channel: "stable".to_string(),
            id: "org.example.App".to_string(),
            version: linyaps_core::Version::parse("1.0.0.0").unwrap(),
            architecture: linyaps_core::Architecture::X86_64,
        };
        let context = ResolvedContext {
            base: PathBuf::from("/layers/base/files"),
            runtime: None,
            app: Some((reference.clone(), PathBuf::from("/layers/app/files"))),
            extensions: Vec::new(),
            target: reference,
        };
        let run_context = RunContextConfig {
            app: Some("stable:org.example.App/1.0.0.0/x86_64".to_string()),
            base: Some("stable:org.example.Base/1.0.0.0/x86_64".to_string()),
            cdi_devices: None,
            extensions: Some(Default::default()),
            instance: None,
            mounts: None,
            overlayfs: None,
            resolv_conf: None,
            runtime: None,
            timezone: None,
            version: "1".to_string(),
        };
        let configuration = oci_configuration(&context, Path::new("/cache"), &run_context).unwrap();
        assert_eq!(
            configuration.pointer("/process/args").unwrap(),
            &json!([
                "/sbin/ldconfig",
                "-X",
                "-C",
                "/run/linglong/cache/ld.so.cache"
            ])
        );
        let destinations = configuration["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|mount| mount["destination"].as_str())
            .collect::<Vec<_>>();
        assert!(destinations.contains(&"/run/linglong/cache"));
        assert!(!destinations.contains(&"/etc/ld.so.cache"));
    }

    #[tokio::test]
    async fn initialization_writes_reusable_container_cache() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("repository");
        fs::create_dir_all(&root).unwrap();
        let mut repository = LocalRepository::create(&root, crate::default_config())
            .await
            .unwrap();
        let architecture = linyaps_core::Architecture::current().unwrap().to_string();
        let base_info = package_info("org.example.Base", "base", "", &architecture);
        let base_reference = reference_from_info(&base_info).unwrap();
        let app_info = package_info(
            "org.example.App",
            "app",
            &base_reference.to_string(),
            &architecture,
        );
        let base = temporary.path().join("base");
        let app = temporary.path().join("app");
        fs::create_dir_all(base.join("files/etc")).unwrap();
        fs::create_dir_all(app.join("files/bin")).unwrap();
        fs::write(
            base.join("info.json"),
            serde_json::to_vec(&base_info).unwrap(),
        )
        .unwrap();
        fs::write(
            app.join("info.json"),
            serde_json::to_vec(&app_info).unwrap(),
        )
        .unwrap();
        fs::write(app.join("files/bin/demo"), b"demo").unwrap();
        repository.import_layer_dir(&base, &[], None).await.unwrap();
        repository.import_layer_dir(&app, &[], None).await.unwrap();
        repository.merge_modules().unwrap();

        let app_reference = reference_from_info(&app_info).unwrap();
        let config = RunContextConfig {
            app: Some(app_reference.to_string()),
            base: Some(base_reference.to_string()),
            cdi_devices: None,
            extensions: Some(Default::default()),
            instance: None,
            mounts: None,
            overlayfs: None,
            resolv_conf: None,
            runtime: None,
            timezone: None,
            version: "1".to_string(),
        };
        let config_json = serde_json::to_string(&config).unwrap();
        let container_id = container_id_for(&config).unwrap();
        initialize_with_runtime(
            &repository,
            &config_json,
            &container_id,
            Path::new("/bin/true"),
        )
        .unwrap();
        let item = repository.layer_item(&app_reference, "binary").unwrap();
        let cache = root.join("cache").join(item.commit).join(container_id);
        assert_eq!(
            fs::read_to_string(cache.join(".config")).unwrap(),
            config_json
        );
        assert!(
            fs::read_to_string(cache.join("ld.so.conf"))
                .unwrap()
                .contains("/opt/apps/org.example.App/files/lib")
        );
    }

    fn package_info(id: &str, kind: &str, base: &str, architecture: &str) -> PackageInfoV2 {
        PackageInfoV2 {
            arch: vec![architecture.to_string()],
            base: base.to_string(),
            channel: "stable".to_string(),
            command: None,
            compatible_version: None,
            description: None,
            extension_implementation: None,
            extensions: None,
            id: id.to_string(),
            kind: kind.to_string(),
            module: "binary".to_string(),
            name: id.to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 0,
            uuid: None,
            version: "1.0.0.0".to_string(),
        }
    }
}
