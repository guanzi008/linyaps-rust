use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Seek, SeekFrom};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::symlink;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use linyaps_api::{
    ApplicationConfigurationPermissions, ContainerProcessStateInfo, ExtensionDefine, PackageInfoV2,
    RunContextConfig, RuntimeConfigure,
};
use linyaps_core::cdi;
use linyaps_core::repo_lock::RepoLock;
use linyaps_core::runtime_config;
use linyaps_core::{Architecture, FuzzyReference, Reference, apply_oci_configuration_patches};
use linyaps_repository::LocalRepository;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    Run, format_package_manager_error, oci_runtime_binary, package_manager_client,
    process_state_root, xdg_runtime_dir,
};

pub(super) async fn run(options: Run, no_dbus: bool) -> Result<(), String> {
    let repository = super::open_local_repository().await?;
    let runtime_config = runtime_config::load_runtime_config(
        &options.app,
        options.instance.as_deref().unwrap_or_default(),
    )
    .map_err(|error| error.to_string())?;
    let resolved = if let Some(config) = options.run_context.as_deref() {
        resolve_external_context(&repository, config)?
    } else {
        resolve_requested_context(&repository, &options, runtime_config.as_ref())?
    };
    let app_reference = resolved.target;
    let app_info = resolved.info;
    let base_reference = resolved.base;
    let runtime_reference = resolved.runtime;
    let extensions = resolved.extensions;
    let extension_references = extensions
        .iter()
        .map(|extension| extension.reference.clone())
        .collect::<Vec<_>>();
    let run_context = resolved.config;
    let run_context_json =
        serde_json::to_string(&run_context).map_err(|error| error.to_string())?;
    let container_id = container_id(&run_context)?;
    let target_item = repository
        .layer_item(&app_reference, "binary")
        .map_err(|error| error.to_string())?;
    let app_cache = repository
        .root()
        .join("cache")
        .join(&target_item.commit)
        .join(&container_id);
    let process_args = process_arguments(&options, &app_info)?;
    let package_manager = package_manager_client(no_dbus).await?;
    package_manager
        .init_run_context(&run_context_json, &container_id)
        .await
        .map_err(format_package_manager_error)?;
    drop(package_manager);
    if options.run_context.is_none() && !crate::namespace::has_effective_sys_admin()? {
        let startup_lock = match acquire_startup_lock_or_reuse(&container_id, &process_args)? {
            StartupLock::New(lock) => lock,
            StartupLock::Reused(status) => {
                return status
                    .success()
                    .then_some(())
                    .ok_or_else(|| format!("application exited with {status}"));
            }
        };
        let status = crate::namespace::run(&run_context_json);
        let cleanup =
            BundleCleanup::new(xdg_runtime_dir().join("linglong").join(&container_id)).finish();
        drop(startup_lock);
        let status = status?;
        cleanup?;
        return status
            .success()
            .then_some(())
            .ok_or_else(|| format!("namespace child exited with {status}"));
    }
    let startup_lock = if crate::namespace::is_child() {
        None
    } else {
        match acquire_startup_lock_or_reuse(&container_id, &process_args)? {
            StartupLock::New(lock) => Some(lock),
            StartupLock::Reused(status) => {
                return status
                    .success()
                    .then_some(())
                    .ok_or_else(|| format!("application exited with {status}"));
            }
        }
    };
    let bundle = xdg_runtime_dir().join("linglong").join(&container_id);
    clear_path(&bundle).map_err(|error| error.to_string())?;
    let bundle_cleanup = BundleCleanup::new(bundle.clone());
    let rootfs = bundle.join("rootfs");
    fs::create_dir_all(&rootfs).map_err(|error| error.to_string())?;
    let base_layer = repository
        .merged_layer_path(&base_reference)
        .map_err(|error| error.to_string())?
        .join("files");
    if !base_layer.is_dir() {
        return Err(format!("layer {} has no files directory", base_reference));
    }
    let overlay = run_context
        .overlayfs
        .as_deref()
        .map(|mode| OverlayMount::mount(mode, &base_layer, &app_cache, &bundle))
        .transpose()?;
    if overlay.is_none() {
        overlay_tree(&base_layer, &rootfs).map_err(|error| error.to_string())?;
    }

    #[cfg(feature = "wayland-security-context")]
    let wayland_security = crate::wayland_security::WaylandSecurityContext::create(
        &bundle,
        &app_info.id,
        &container_id,
    )?;
    #[cfg(feature = "wayland-security-context")]
    let wayland_socket = Some(wayland_security.socket_path());
    #[cfg(not(feature = "wayland-security-context"))]
    let wayland_socket: Option<&Path> = None;
    let xdp_documents = xdp_documents_mount(&options, &app_info.id, runtime_config.as_ref()).await;
    let config = oci_config(
        &options,
        &app_reference,
        &container_id,
        &process_args,
        runtime_reference.as_ref(),
        &app_cache,
        &repository,
        &extensions,
        &run_context,
        &rootfs,
        runtime_config.as_ref(),
        xdp_documents.as_deref(),
        wayland_socket,
    )?;
    fs::write(
        bundle.join("config.json"),
        serde_json::to_vec_pretty(&config).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;

    let mut child = Command::new(oci_runtime_binary())
        .args(["--cgroup-manager=disabled", "run", "--bundle"])
        .arg(&bundle)
        .arg(&container_id)
        .spawn()
        .map_err(|error| format!("failed to execute OCI runtime: {error}"))?;
    let pid = wait_for_container_pid(&container_id, child.id())?;
    let state_path = write_process_state(
        pid,
        &container_id,
        &app_reference,
        &base_reference,
        runtime_reference.as_ref(),
        &extension_references,
    )?;
    let status = child
        .wait()
        .map_err(|error| format!("failed to wait for OCI runtime: {error}"));
    let _ = fs::remove_file(state_path);
    drop(overlay);
    bundle_cleanup.finish()?;
    let status = status?;
    drop(startup_lock);
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("application exited with {status}"))
}

struct ResolvedRunContext {
    target: Reference,
    info: PackageInfoV2,
    base: Reference,
    runtime: Option<Reference>,
    extensions: Vec<ResolvedExtension>,
    config: RunContextConfig,
}

struct ResolvedExtension {
    definition: ExtensionDefine,
    for_reference: String,
    reference: Reference,
}

fn resolve_requested_context(
    repository: &LocalRepository,
    options: &Run,
    runtime_config: Option<&RuntimeConfigure>,
) -> Result<ResolvedRunContext, String> {
    let fuzzy = options
        .app
        .parse::<FuzzyReference>()
        .map_err(|error| error.to_string())?;
    let app = repository
        .resolve_local(&fuzzy, false)
        .map_err(|error| error.to_string())?;
    let info = repository
        .read_layer_info(&app, "binary")
        .map_err(|error| error.to_string())?;
    if info.kind != "app" {
        return Err(linyaps_i18n::format(
            "{} is not an application.",
            &[&app.id],
        ));
    }
    let base = resolve_dependency(
        repository,
        options.base.as_deref().unwrap_or(&info.base),
        &info.channel,
    )?;
    let runtime = options
        .runtime
        .as_deref()
        .or(info.runtime.as_deref())
        .filter(|value| !value.is_empty())
        .map(|value| resolve_dependency(repository, value, &info.channel))
        .transpose()?;
    let base_info = repository
        .read_layer_info(&base, "binary")
        .map_err(|error| error.to_string())?;
    let runtime_info = runtime
        .as_ref()
        .map(|reference| repository.read_layer_info(reference, "binary"))
        .transpose()
        .map_err(|error| error.to_string())?;
    let mut extensions = Vec::new();
    let mut extension_map = BTreeMap::new();
    resolve_declared_extensions(
        repository,
        &base,
        &base_info,
        &mut extensions,
        &mut extension_map,
        runtime_config,
    )?;
    if let (Some(reference), Some(runtime_info)) = (&runtime, runtime_info.as_ref()) {
        resolve_declared_extensions(
            repository,
            reference,
            runtime_info,
            &mut extensions,
            &mut extension_map,
            runtime_config,
        )?;
    }
    resolve_declared_extensions(
        repository,
        &app,
        &info,
        &mut extensions,
        &mut extension_map,
        runtime_config,
    )?;
    for requested in &options.extensions {
        let reference = resolve_dependency(repository, requested, &info.channel)?;
        ensure_extension_kind(repository, &reference)?;
        let fuzzy = requested
            .parse::<FuzzyReference>()
            .map_err(|error| error.to_string())?;
        append_extension(
            &mut extensions,
            &mut extension_map,
            ResolvedExtension {
                definition: ExtensionDefine {
                    allow_env: None,
                    directory: format!("/opt/extensions/{}", reference.id),
                    name: fuzzy.id,
                    version: fuzzy.version.unwrap_or_default(),
                },
                for_reference: app.to_string(),
                reference,
            },
        );
    }
    let config = RunContextConfig {
        app: Some(app.to_string()),
        base: Some(base.to_string()),
        cdi_devices: resolve_cdi_devices(options, runtime_config)?,
        extensions: Some(extension_map),
        instance: options.instance.clone(),
        mounts: resolved_mounts(runtime_config),
        overlayfs: Some(resolve_overlay_mode(repository, &base)?),
        resolv_conf: resolve_resolv_conf()?,
        runtime: runtime.as_ref().map(ToString::to_string),
        timezone: resolve_timezone()?,
        version: "1".to_string(),
    };
    Ok(ResolvedRunContext {
        target: app,
        info,
        base,
        runtime,
        extensions,
        config,
    })
}

fn resolve_external_context(
    repository: &LocalRepository,
    config_json: &str,
) -> Result<ResolvedRunContext, String> {
    let config: RunContextConfig = serde_json::from_str(config_json)
        .map_err(|error| format!("failed to parse run context: {error}"))?;
    if config.version != "1" {
        return Err(format!(
            "run context config version mismatch: config version {}, expected version 1",
            config.version
        ));
    }
    let base = exact_local_reference(
        repository,
        config
            .base
            .as_deref()
            .ok_or_else(|| "base layer is required".to_string())?,
    )?;
    let runtime = config
        .runtime
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| exact_local_reference(repository, value))
        .transpose()?;
    let app = config
        .app
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| exact_local_reference(repository, value))
        .transpose()?;
    let target = app
        .clone()
        .or_else(|| runtime.clone())
        .unwrap_or_else(|| base.clone());
    let info = repository
        .read_layer_info(&target, "binary")
        .map_err(|error| error.to_string())?;
    let mut extensions = Vec::new();
    for (target, values) in config.extensions.iter().flatten() {
        let (target_reference, target_info) = find_context_target(
            target,
            app.as_ref()
                .map(|reference| (reference, repository.read_layer_info(reference, "binary"))),
            runtime
                .as_ref()
                .map(|reference| (reference, repository.read_layer_info(reference, "binary"))),
            (&base, repository.read_layer_info(&base, "binary")),
        )?;
        for value in values {
            let extension = exact_local_reference(repository, value)?;
            ensure_extension_kind(repository, &extension)?;
            let definition = target_info
                .extensions
                .as_deref()
                .into_iter()
                .flatten()
                .find(|definition| {
                    enabled_extension_name(&definition.name)
                        .is_some_and(|name| name == extension.id)
                })
                .cloned()
                .unwrap_or_else(|| ExtensionDefine {
                    allow_env: None,
                    directory: format!("/opt/extensions/{}", extension.id),
                    name: extension.id.clone(),
                    version: extension.version.to_string(),
                });
            extensions.push(ResolvedExtension {
                definition,
                for_reference: target_reference.to_string(),
                reference: extension,
            });
        }
    }
    Ok(ResolvedRunContext {
        target,
        info,
        base,
        runtime,
        extensions,
        config,
    })
}

fn resolve_declared_extensions(
    repository: &LocalRepository,
    target: &Reference,
    info: &PackageInfoV2,
    extensions: &mut Vec<ResolvedExtension>,
    extension_map: &mut BTreeMap<String, Vec<String>>,
    runtime_config: Option<&RuntimeConfigure>,
) -> Result<(), String> {
    let mut definitions = info.extensions.clone().unwrap_or_default();
    if let Some(external) = runtime_config.and_then(|config| config.extension_definitions.as_ref())
    {
        for (pattern, values) in external {
            let Ok(fuzzy) = pattern.parse::<FuzzyReference>() else {
                continue;
            };
            if fuzzy.id == target.id
                && fuzzy
                    .version
                    .as_ref()
                    .is_none_or(|version| target.version.semantic_match(version))
            {
                definitions.extend(values.iter().cloned());
            }
        }
    }
    for definition in &definitions {
        let Some(name) = enabled_extension_name(&definition.name) else {
            continue;
        };
        let fuzzy = FuzzyReference::new(
            Some(info.channel.clone()),
            name,
            (!definition.version.is_empty()).then(|| definition.version.clone()),
            Some(target.architecture),
        )
        .map_err(|error| error.to_string())?;
        let Ok(reference) = repository.resolve_local(&fuzzy, true) else {
            continue;
        };
        ensure_extension_kind(repository, &reference)?;
        append_extension(
            extensions,
            extension_map,
            ResolvedExtension {
                definition: definition.clone(),
                for_reference: target.to_string(),
                reference,
            },
        );
    }
    Ok(())
}

fn append_extension(
    extensions: &mut Vec<ResolvedExtension>,
    extension_map: &mut BTreeMap<String, Vec<String>>,
    extension: ResolvedExtension,
) {
    let values = extension_map
        .entry(extension.for_reference.clone())
        .or_default();
    let reference = extension.reference.to_string();
    if values.contains(&reference) {
        return;
    }
    values.push(reference);
    extensions.push(extension);
}

fn ensure_extension_kind(
    repository: &LocalRepository,
    reference: &Reference,
) -> Result<(), String> {
    let info = repository
        .read_layer_info(reference, "binary")
        .map_err(|error| error.to_string())?;
    (info.kind == "extension")
        .then_some(())
        .ok_or_else(|| format!("{reference} is not an extension"))
}

fn enabled_extension_name(name: &str) -> Option<String> {
    if name != "org.deepin.driver.display.nvidia" {
        return Some(name.to_string());
    }
    let version = fs::read_to_string("/sys/module/nvidia/version").ok()?;
    let version = version.trim().replace('.', "-");
    (!version.is_empty()).then(|| format!("{name}.{version}"))
}

fn find_context_target<'a>(
    requested: &str,
    app: Option<(
        &'a Reference,
        Result<PackageInfoV2, linyaps_repository::RepositoryError>,
    )>,
    runtime: Option<(
        &'a Reference,
        Result<PackageInfoV2, linyaps_repository::RepositoryError>,
    )>,
    base: (
        &'a Reference,
        Result<PackageInfoV2, linyaps_repository::RepositoryError>,
    ),
) -> Result<(&'a Reference, PackageInfoV2), String> {
    let fuzzy = requested
        .parse::<FuzzyReference>()
        .map_err(|error| format!("failed to parse target layer reference: {error}"))?;
    for candidate in [app, runtime, Some(base)].into_iter().flatten() {
        let (reference, info) = candidate;
        if fuzzy.id == reference.id
            && fuzzy
                .channel
                .as_ref()
                .is_none_or(|channel| channel == &reference.channel)
            && fuzzy
                .version
                .as_ref()
                .is_none_or(|version| reference.version.semantic_match(version))
            && fuzzy
                .architecture
                .is_none_or(|architecture| architecture == reference.architecture)
        {
            return info
                .map(|info| (reference, info))
                .map_err(|error| error.to_string());
        }
    }
    Err(format!("target layer not found: {requested}"))
}

fn exact_local_reference(repository: &LocalRepository, value: &str) -> Result<Reference, String> {
    let reference = value
        .parse::<Reference>()
        .map_err(|error| error.to_string())?;
    repository
        .layer_item(&reference, "binary")
        .map_err(|error| error.to_string())?;
    Ok(reference)
}

fn resolve_dependency(
    repository: &LocalRepository,
    raw: &str,
    channel: &str,
) -> Result<Reference, String> {
    if raw.is_empty() {
        return Err("application has no base reference".to_string());
    }
    let mut fuzzy = raw
        .parse::<FuzzyReference>()
        .map_err(|error| error.to_string())?;
    if fuzzy.channel.is_none() {
        fuzzy.channel = Some(channel.to_string());
    }
    if fuzzy.architecture.is_none() {
        fuzzy.architecture = Some(Architecture::current().map_err(|error| error.to_string())?);
    }
    repository
        .resolve_local(&fuzzy, true)
        .map_err(|error| error.to_string())
}

fn resolve_cdi_devices(
    options: &Run,
    runtime_config: Option<&RuntimeConfigure>,
) -> Result<Option<Vec<linyaps_api::CdiDeviceEntry>>, String> {
    let directories = options
        .cdi_spec_dir
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if !options.device.is_empty() {
        return cdi::get_devices(&directories, Some(&options.device))
            .map(Some)
            .map_err(|error| error.to_string());
    }
    if let Some(requested) = runtime_config.and_then(|config| config.devices.as_deref()) {
        return cdi::get_devices(&directories, Some(requested))
            .map(Some)
            .map_err(|error| error.to_string());
    }
    let devices = cdi::get_devices(&directories, None).map_err(|error| error.to_string())?;
    Ok(devices
        .into_iter()
        .find(|device| device.kind == "nvidia.com/gpu" && device.name == "all")
        .map(|device| vec![device]))
}

fn resolved_mounts(runtime_config: Option<&RuntimeConfigure>) -> Option<Vec<linyaps_api::Mount>> {
    runtime_config
        .and_then(|config| config.mounts.clone())
        .map(|mut mounts| {
            for mount in &mut mounts {
                if mount
                    .source_type
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                {
                    continue;
                }
                let source = Path::new(&mount.source);
                mount.source_type = if source.is_dir() {
                    Some("dir".to_string())
                } else if source.exists() {
                    Some("file".to_string())
                } else {
                    None
                };
            }
            mounts
        })
}

fn process_arguments(options: &Run, info: &PackageInfoV2) -> Result<Vec<String>, String> {
    let mut arguments = if options.command.is_empty() {
        info.command
            .clone()
            .ok_or_else(|| format!("application {} has no command", info.id))?
    } else {
        options.command.clone()
    };
    replace_field_codes(&mut arguments, &options.files, &options.urls);
    if options.command.is_empty() {
        arguments.extend(options.files.iter().cloned());
        arguments.extend(options.urls.iter().cloned());
    }
    if options.debug {
        let mut debug = vec!["gdbserver".to_string(), options.debug_listen.clone()];
        debug.extend(arguments);
        arguments = debug;
    }
    if arguments.is_empty() {
        return Err("application command is empty".to_string());
    }
    Ok(arguments)
}

fn replace_field_codes(arguments: &mut [String], files: &[String], urls: &[String]) {
    for argument in arguments {
        match argument.as_str() {
            "%f" | "%F" if !files.is_empty() => *argument = files.join(" "),
            "%u" | "%U" if !urls.is_empty() => *argument = urls.join(" "),
            _ => {}
        }
    }
}

enum StartupLock {
    New(File),
    Reused(ExitStatus),
}

fn acquire_startup_lock_or_reuse(
    container_id: &str,
    arguments: &[String],
) -> Result<StartupLock, String> {
    let directory = xdg_runtime_dir().join("linglong");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("failed to create {}: {error}", directory.display()))?;
    let path = directory.join(format!(".cli.{container_id}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    loop {
        match rustix::fs::fcntl_lock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(StartupLock::New(lock)),
            Err(error) if lock_would_block(error) => {}
            Err(error) => {
                return Err(format!("failed to lock {}: {error}", path.display()));
            }
        }
        if let Some(status) = reuse_running_container(container_id, arguments)? {
            return Ok(StartupLock::Reused(status));
        }
        thread::sleep(Duration::from_secs(3));
    }
}

fn reuse_running_container(
    container_id: &str,
    arguments: &[String],
) -> Result<Option<ExitStatus>, String> {
    let path = xdg_runtime_dir()
        .join("linglong")
        .join(container_id)
        .join(".lock");
    let mut lock = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to open {}: {error}", path.display())),
    };
    match rustix::fs::fcntl_lock(&lock, rustix::fs::FlockOperation::NonBlockingLockShared) {
        Ok(()) => {}
        Err(error) if lock_would_block(error) => return Ok(None),
        Err(error) => return Err(format!("failed to lock {}: {error}", path.display())),
    }
    lock.seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to seek {}: {error}", path.display()))?;
    let mut state = String::new();
    lock.read_to_string(&mut state)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    if state != "running" {
        return Ok(None);
    }

    let user = format!(
        "{}:{}",
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw()
    );
    let mut command = Command::new(oci_runtime_binary());
    command.args(["exec", "--user", &user]);
    if std::io::stdin().is_terminal() {
        command.arg("--tty");
    }
    let status = command
        .arg(container_id)
        .args([
            "/run/linglong/container-init",
            "/bin/bash",
            "--noprofile",
            "--norc",
            "-c",
            &entrypoint_script(arguments),
        ])
        .status()
        .map_err(|error| format!("failed to execute OCI runtime: {error}"))?;
    Ok(Some(status))
}

fn lock_would_block(error: rustix::io::Errno) -> bool {
    matches!(error, rustix::io::Errno::AGAIN | rustix::io::Errno::ACCESS)
}

#[allow(clippy::too_many_arguments)]
fn oci_config(
    options: &Run,
    app: &Reference,
    container_id: &str,
    arguments: &[String],
    runtime: Option<&Reference>,
    app_cache: &Path,
    repository: &LocalRepository,
    extensions: &[ResolvedExtension],
    run_context: &RunContextConfig,
    rootfs: &Path,
    runtime_config: Option<&RuntimeConfigure>,
    xdp_documents: Option<&Path>,
    wayland_socket: Option<&Path>,
) -> Result<Value, String> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let mut environment = inherited_environment();
    let bundle = rootfs
        .parent()
        .ok_or_else(|| format!("rootfs {} has no bundle directory", rootfs.display()))?;
    let HostIntegration {
        mut mounts,
        masked_paths,
        xdp_enabled,
    } = host_integration(
        options,
        &app.id,
        container_id,
        bundle,
        &mut environment,
        run_context,
        runtime_config,
        xdp_documents,
        wayland_socket,
    )?;
    let app_info = repository
        .read_layer_info(app, "binary")
        .map_err(|error| error.to_string())?;
    append_permission_mounts(&mut mounts, app_info.permissions.as_ref(), rootfs)?;
    if let Some(runtime) = runtime {
        let source = repository
            .merged_layer_path(runtime)
            .map_err(|error| error.to_string())?
            .join("files");
        mounts.push(bind_mount(&source, Path::new("/runtime"), true));
    }
    let app_source = repository
        .merged_layer_path(app)
        .map_err(|error| error.to_string())?
        .join("files");
    mounts.push(json!({
        "destination": "/opt",
        "type": "tmpfs",
        "source": "tmpfs",
        "options": ["nodev", "nosuid", "mode=700"]
    }));
    mounts.push(bind_mount(
        &app_source,
        &PathBuf::from(format!("/opt/apps/{}/files", app.id)),
        true,
    ));
    for extension in extensions {
        let source = repository
            .merged_layer_path(&extension.reference)
            .map_err(|error| error.to_string())?
            .join("files");
        mounts.push(json!({
            "destination": PathBuf::from(format!("/opt/extensions/{}", extension.reference.id)),
            "type": "bind",
            "source": source,
            "options": ["rbind", "ro"]
        }));
        let info = repository
            .read_layer_info(&extension.reference, "binary")
            .map_err(|error| error.to_string())?;
        if let Some(implementation) = &info.extension_implementation {
            for device in implementation.device_nodes.as_deref().into_iter().flatten() {
                let source = Path::new(device.host_path.as_deref().unwrap_or(&device.path));
                mounts.push(simple_bind_mount(source, Path::new(&device.path), false));
            }
            for (key, template) in implementation.env.iter().flatten() {
                let default = if let Some(allowed) = extension.definition.allow_env.as_ref() {
                    let Some(default) = allowed.get(key) else {
                        continue;
                    };
                    default.as_str()
                } else {
                    ""
                };
                let origin = environment.get(key).map(String::as_str).unwrap_or(default);
                let value = template
                    .replace(
                        "$PREFIX",
                        &format!("/opt/extensions/{}", extension.reference.id),
                    )
                    .replace("$ORIGIN", origin);
                environment.insert(key.clone(), value);
            }
        }
        append_permission_mounts(&mut mounts, info.permissions.as_ref(), rootfs)?;
    }
    for (key, value) in runtime_config
        .and_then(|config| config.env.as_ref())
        .into_iter()
        .flatten()
    {
        environment.insert(key.clone(), value.clone());
    }
    for value in &options.environment {
        if let Some((key, value)) = value.split_once('=') {
            environment.insert(key.to_string(), value.to_string());
        }
    }
    if let Some(url) = &options.debug_debuginfod {
        environment.insert("DEBUGINFOD_URLS".to_string(), url.clone());
    }
    apply_cdi_edits(
        &mut environment,
        &mut mounts,
        run_context,
        !device_passthrough_enabled(options, runtime_config),
    )?;
    mounts.push(bind_mount(
        app_cache,
        Path::new("/run/linglong/cache"),
        true,
    ));
    mounts.push(simple_bind_mount(
        &app_cache.join("ld.so.cache"),
        Path::new("/etc/ld.so.cache"),
        true,
    ));
    mounts.push(simple_bind_mount(
        &app_cache.join("ld.so.conf"),
        Path::new("/etc/ld.so.conf.d/zz_deepin-linglong-app.conf"),
        true,
    ));
    for file in &options.files {
        let path = Path::new(file);
        if path.is_absolute() && path.exists() {
            mounts.push(bind_mount(path, path, false));
        }
    }
    if let Some(symbols) = options.debug_symbol_dir.as_deref().map(Path::new)
        && symbols.is_absolute()
        && symbols.exists()
    {
        mounts.push(bind_mount(symbols, symbols, true));
    }
    if options.privileged && uid != 0 {
        return Err("privileged mode requires running as root".to_string());
    }
    let mut capabilities = if options.privileged {
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
            "CAP_NET_RAW",
            "CAP_NET_ADMIN",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for capability in &options.caps_add {
        if !capabilities.contains(capability) {
            capabilities.push(capability.clone());
        }
    }
    environment.insert("LINGLONG_APPID".to_string(), app.id.clone());
    if xdp_enabled {
        environment
            .entry("GTK_USE_PORTAL".to_string())
            .or_insert_with(|| "1".to_string());
        environment
            .entry("QT_QPA_PLATFORMTHEME".to_string())
            .or_insert_with(|| "xdgdesktopportal".to_string());
    }
    prepare_container_environment(bundle, &environment, &mut mounts)?;
    let mut namespaces = vec![
        json!({"type": "pid"}),
        json!({"type": "mount"}),
        json!({"type": "uts"}),
    ];
    if !options.privileged {
        namespaces.push(json!({"type": "user"}));
    }
    let mut linux = json!({
        "rootfsPropagation": "slave",
        "namespaces": namespaces,
        "maskedPaths": masked_paths
    });
    if !options.privileged {
        linux["uidMappings"] = json!([{"containerID": uid, "hostID": uid, "size": 1}]);
        linux["gidMappings"] = json!([{"containerID": gid, "hostID": gid, "size": 1}]);
    }
    let mut annotations = BTreeMap::from([(
        "cn.org.linyaps.runtime.ns_last_pid".to_string(),
        std::process::id().to_string(),
    )]);
    if let Some(socket) = wayland_socket {
        annotations.insert(
            "cn.org.linyaps.runtime.ws.path".to_string(),
            socket.to_string_lossy().into_owned(),
        );
    }
    let process = json!({
        "args": ["bash"],
        "env": environment.into_iter().map(|(key, value)| format!("{key}={value}")).collect::<Vec<_>>(),
        "cwd": "/",
        "capabilities": {
            "bounding": capabilities,
            "effective": capabilities,
            "permitted": capabilities
        }
    });
    let mut configuration = json!({
        "ociVersion": "1.0.1",
        "root": {"path": "rootfs", "readonly": true},
        "hostname": "linglong",
        "process": process,
        "mounts": mounts,
        "linux": linux,
        "annotations": annotations
    });
    apply_oci_configuration_patches(
        &mut configuration,
        &app.id,
        Path::new("/usr/lib/linglong/container/config.d"),
    )?;
    finalize_container_process(
        &mut configuration,
        bundle,
        arguments,
        options.workdir.as_deref(),
        uid,
        gid,
    )?;
    Ok(configuration)
}

fn set_process_terminal(process: &mut serde_json::Map<String, Value>, terminal: bool) {
    if terminal {
        process.insert("terminal".to_string(), Value::Bool(true));
    }
}

async fn xdp_documents_mount(
    options: &Run,
    app_id: &str,
    runtime_config: Option<&RuntimeConfigure>,
) -> Option<PathBuf> {
    if xdp_is_disabled(options, app_id, runtime_config) {
        return None;
    }
    let result = async {
        let connection = zbus::Connection::session()
            .await
            .map_err(|error| error.to_string())?;
        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.portal.Documents",
            "/org/freedesktop/portal/documents",
            "org.freedesktop.portal.Documents",
        )
        .await
        .map_err(|error| error.to_string())?;
        let mut bytes: Vec<u8> = proxy
            .call("GetMountPoint", &())
            .await
            .map_err(|error| error.to_string())?;
        if let Some(end) = bytes.iter().position(|byte| *byte == 0) {
            bytes.truncate(end);
        }
        (!bytes.is_empty())
            .then(|| PathBuf::from(OsString::from_vec(bytes)))
            .ok_or_else(|| "Documents portal mount point is empty".to_string())
    }
    .await;
    match result {
        Ok(path) => Some(path),
        Err(error) => {
            eprintln!("warning: failed to get XDP Documents mount point: {error}");
            None
        }
    }
}

fn xdp_is_disabled(options: &Run, app_id: &str, runtime_config: Option<&RuntimeConfigure>) -> bool {
    if options.enable_xdp {
        return false;
    }
    if options.disable_xdp {
        return true;
    }
    if !valid_xdp_app_id(app_id) {
        return true;
    }
    runtime_config
        .and_then(|config| config.disable_xdp)
        .unwrap_or(false)
}

fn valid_xdp_app_id(app_id: &str) -> bool {
    let segments = app_id.split('.').collect::<Vec<_>>();
    segments.len() >= 2
        && segments.iter().enumerate().all(|(index, segment)| {
            !segment.is_empty()
                && segment.chars().all(|character| {
                    character.is_ascii_alphanumeric()
                        || character == '_'
                        || (index + 1 == segments.len() && character == '-')
                })
        })
}

fn inherited_environment() -> BTreeMap<String, String> {
    let mut environment = BTreeMap::from([(
        "PATH".to_string(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
    )]);
    let keys = [
        "LANG",
        "LANGUAGE",
        "TERM",
        "XDG_SESSION_DESKTOP",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "DESKTOP_SESSION",
        "GDMSESSION",
        "GNOME_DESKTOP_SESSION_ID",
        "GIO_LAUNCHED_DESKTOP_FILE",
        "D_DISABLE_RT_SCREEN_SCALE",
        "DEEPIN_WINE_SCALE",
        "XCURSOR_SIZE",
        "XMODIFIERS",
        "XIM",
        "CLUTTER_IM_MODULE",
        "QT4_IM_MODULE",
        "QT_IM_MODULE",
        "QT_IM_MODULES",
        "GTK_IM_MODULE",
        "SDL_IM_MODULE",
        "QT_QPA_PLATFORM",
        "QT_WAYLAND_SHELL_INTEGRATION",
        "QT_WAYLAND_FORCE_DPI",
        "all_proxy",
        "auto_proxy",
        "http_proxy",
        "https_proxy",
        "ftp_proxy",
        "SOCKS_SERVER",
        "no_proxy",
        "USER",
        "LINGLONG_ROOT",
        "__NV_PRIME_RENDER_OFFLOAD",
        "__GLX_VENDOR_LIBRARY_NAME",
        "__VK_LAYER_NV_optimus",
        "DRI_PRIME",
    ];
    for key in keys {
        if let Ok(value) = std::env::var(key) {
            environment.insert(key.to_string(), value);
        }
    }
    environment
}

struct HostIntegration {
    mounts: Vec<Value>,
    masked_paths: Vec<String>,
    xdp_enabled: bool,
}

#[allow(clippy::too_many_arguments)]
fn host_integration(
    options: &Run,
    app_id: &str,
    container_id: &str,
    bundle: &Path,
    environment: &mut BTreeMap<String, String>,
    run_context: &RunContextConfig,
    runtime_config: Option<&RuntimeConfigure>,
    xdp_documents: Option<&Path>,
    wayland_socket: Option<&Path>,
) -> Result<HostIntegration, String> {
    let mut mounts = vec![
        json!({"destination":"/sys","type":"bind","source":"/sys","options":["rbind","nosuid","noexec","nodev","rslave"]}),
        json!({"destination":"/proc","type":"proc","source":"proc","options":["nosuid","noexec","nodev"]}),
    ];
    if device_passthrough_enabled(options, runtime_config) {
        mounts.push(json!({"destination":"/dev","type":"bind","source":"/dev","options":["rbind","rslave"]}));
    } else {
        mounts.extend([
            json!({"destination":"/dev","type":"tmpfs","source":"tmpfs","options":["nosuid","strictatime","mode=0755","size=65536k"]}),
            json!({"destination":"/dev/pts","type":"devpts","source":"devpts","options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620"]}),
            json!({"destination":"/dev/shm","type":"tmpfs","source":"shm","options":["nosuid","noexec","nodev","mode=1777"]}),
            json!({"destination":"/dev/mqueue","type":"bind","source":"/dev/mqueue","options":["rbind","nosuid","noexec","nodev","rslave"]}),
        ]);
        let mut device_nodes = fs::read_dir("/dev")
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| {
                        let name = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("");
                        matches!(name, "snd" | "dri" | "jmgpu")
                            || name.starts_with("video")
                            || name.starts_with("nvidia")
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        device_nodes.sort();
        for path in device_nodes {
            mounts.push(json!({
                "destination": path,
                "type": "bind",
                "source": path,
                "options": ["rbind", "rslave"]
            }));
        }
    }
    mounts.extend([
        json!({"destination":"/sys/fs/cgroup","type":"cgroup","source":"cgroup","options":["nosuid","noexec","nodev","relatime","ro"]}),
        json!({"destination":"/run","type":"tmpfs","source":"tmpfs","options":["nosuid","nodev","mode=0755","size=65536k"]}),
        json!({"destination":"/tmp","type":"bind","source":"/tmp","options":["rbind","rslave"]}),
        json!({"destination":"/run/host","type":"tmpfs","source":"tmpfs","options":["nodev","nosuid","mode=700"]}),
        json!({"destination":"/run/host/rootfs","type":"bind","source":"/","options":["rbind","ro","rslave"]}),
    ]);
    bind_if_exists(
        &mut mounts,
        Path::new("/run/udev"),
        Path::new("/run/udev"),
        true,
    );
    for path in [
        "/etc/machine-id",
        "/usr/lib/locale",
        "/usr/share/fonts",
        "/usr/share/icons",
        "/usr/share/themes",
        "/var/cache/fontconfig",
    ] {
        let path = Path::new(path);
        bind_if_exists(&mut mounts, path, path, true);
    }
    for path in ["/media", "/run/media", "/mnt"] {
        let path = Path::new(path);
        bind_if_exists(&mut mounts, path, path, false);
    }

    append_network_and_timezone_mounts(&mut mounts, run_context);
    append_minimal_user_group_mounts(&mut mounts, bundle)?;

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())?;
    if home.as_os_str().is_empty() || !home.is_absolute() || !home.exists() {
        return Err(format!("invalid HOME directory: {}", home.display()));
    }
    let private_root = home.join(".linglong");
    let private_app = private_root.join(app_id);
    fs::create_dir_all(&private_app)
        .map_err(|error| format!("failed to create {}: {error}", private_app.display()))?;
    let mut masked_paths = vec![private_root.to_string_lossy().into_owned()];
    append_home_mounts(&mut mounts, environment, &home, &private_app)?;
    append_private_mounts(&mut mounts, &home, &private_app)?;

    let host_runtime = xdg_runtime_dir();
    let container_runtime =
        PathBuf::from("/run/user").join(rustix::process::geteuid().as_raw().to_string());
    let app_runtime = host_runtime.join("linglong/apps").join(app_id);
    fs::create_dir_all(&app_runtime)
        .map_err(|error| format!("failed to create {}: {error}", app_runtime.display()))?;
    fs::set_permissions(&app_runtime, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("failed to chmod {}: {error}", app_runtime.display()))?;
    mounts.push(json!({
        "destination": container_runtime,
        "type": "bind",
        "source": app_runtime,
        "options": ["bind", "rslave"]
    }));
    environment.insert(
        "XDG_RUNTIME_DIR".to_string(),
        container_runtime.to_string_lossy().into_owned(),
    );
    append_ipc_mounts(&mut mounts, environment, &host_runtime, &container_runtime);
    append_display_mounts(
        &mut mounts,
        environment,
        &host_runtime,
        &container_runtime,
        wayland_socket,
    );

    if options
        .enable_pipewire
        .or_else(|| runtime_config.and_then(|config| config.enable_pipewire))
        == Some(true)
    {
        bind_if_exists(
            &mut mounts,
            &host_runtime.join("pipewire-0"),
            &container_runtime.join("pipewire-0"),
            false,
        );
    }
    if options
        .enable_atspi
        .or_else(|| runtime_config.and_then(|config| config.enable_atspi))
        == Some(true)
    {
        bind_if_exists(
            &mut mounts,
            &host_runtime.join("at-spi/bus_0"),
            &container_runtime.join("at-spi/bus_0"),
            false,
        );
    }

    let xdp_enabled = if let Some(documents) = xdp_documents {
        mounts.push(json!({
            "destination": container_runtime.join("doc"),
            "type": "bind",
            "source": documents.join("by-app").join(app_id),
            "options": ["bind", "nosuid", "nodev", "relatime", "rslave"]
        }));
        let info = format!(
            "[General]\nLinyaps-version={}\n\n[Application]\nId={}\n\n[Instance]\nId={}\n\n[Context]\nNetwork=shared\n\n",
            env!("CARGO_PKG_VERSION"),
            app_id,
            container_id,
        );
        let path = bundle.join(".linyaps");
        fs::write(&path, info)
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
        mounts.push(simple_bind_mount(&path, Path::new("/.linyaps"), true));
        true
    } else {
        false
    };

    let init_socket = bundle.join("init");
    fs::create_dir_all(&init_socket)
        .map_err(|error| format!("failed to create {}: {error}", init_socket.display()))?;
    mounts.push(simple_bind_mount(
        &init_socket,
        Path::new("/run/linglong/init"),
        false,
    ));
    let container_lock = bundle.join(".lock");
    fs::write(&container_lock, "initializing")
        .map_err(|error| format!("failed to write {}: {error}", container_lock.display()))?;
    mounts.push(simple_bind_mount(
        &container_lock,
        Path::new("/run/linglong/.lock"),
        false,
    ));
    for mount in run_context.mounts.as_deref().into_iter().flatten() {
        mounts.push(json!({
            "destination": mount.destination,
            "type": mount.mount_type,
            "source": mount.source,
            "options": mount.options.clone().unwrap_or_default(),
        }));
    }
    masked_paths.sort();
    masked_paths.dedup();
    Ok(HostIntegration {
        mounts,
        masked_paths,
        xdp_enabled,
    })
}

fn append_network_and_timezone_mounts(mounts: &mut Vec<Value>, run_context: &RunContextConfig) {
    if let Some(source) = run_context.resolv_conf.as_deref().map(Path::new) {
        bind_if_exists(mounts, source, Path::new("/etc/resolv.conf"), true);
    }
    bind_if_exists(
        mounts,
        Path::new("/etc/hosts"),
        Path::new("/etc/hosts"),
        true,
    );
    bind_if_exists(
        mounts,
        Path::new("/etc/timezone"),
        Path::new("/etc/timezone"),
        true,
    );
    let zoneinfo = std::env::var_os("TZDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/zoneinfo"));
    bind_if_exists(mounts, &zoneinfo, Path::new("/usr/share/zoneinfo"), true);
    match run_context.timezone.as_deref() {
        Some("") => bind_if_exists(
            mounts,
            Path::new("/etc/localtime"),
            Path::new("/etc/localtime"),
            true,
        ),
        Some(timezone) => bind_if_exists(
            mounts,
            &zoneinfo.join(timezone),
            Path::new("/etc/localtime"),
            true,
        ),
        None => {}
    }
}

fn append_minimal_user_group_mounts(mounts: &mut Vec<Value>, bundle: &Path) -> Result<(), String> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let passwd = fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|content| account_line(&content, uid, true))
        .unwrap_or_default()
        + "nobody:x:65534:65534:nobody:/:/usr/sbin/nologin\n";
    let group = fs::read_to_string("/etc/group")
        .ok()
        .and_then(|content| account_line(&content, gid, false))
        .unwrap_or_default()
        + "nobody:x:65534:\n";
    let passwd_path = bundle.join("passwd");
    let group_path = bundle.join("group");
    fs::write(&passwd_path, passwd)
        .map_err(|error| format!("failed to write {}: {error}", passwd_path.display()))?;
    fs::write(&group_path, group)
        .map_err(|error| format!("failed to write {}: {error}", group_path.display()))?;
    mounts.push(simple_bind_mount(
        &passwd_path,
        Path::new("/etc/passwd"),
        true,
    ));
    mounts.push(simple_bind_mount(
        &group_path,
        Path::new("/etc/group"),
        true,
    ));
    Ok(())
}

fn account_line(content: &str, id: u32, passwd: bool) -> Option<String> {
    content.lines().find_map(|line| {
        let fields = line.split(':').collect::<Vec<_>>();
        if fields.get(2)?.parse::<u32>().ok()? != id {
            return None;
        }
        if passwd && fields.len() >= 7 {
            Some(format!(
                "{}:x:{}:{}:{}:{}:{}\n",
                fields[0], fields[2], fields[3], fields[4], fields[5], fields[6]
            ))
        } else if !passwd && fields.len() >= 3 {
            Some(format!("{}:x:{}:\n", fields[0], fields[2]))
        } else {
            None
        }
    })
}

fn append_home_mounts(
    mounts: &mut Vec<Value>,
    environment: &mut BTreeMap<String, String>,
    home: &Path,
    private_app: &Path,
) -> Result<(), String> {
    mounts.push(json!({
        "destination": "/home",
        "type": "tmpfs",
        "source": "tmpfs",
        "options": ["nodev", "nosuid", "mode=700"]
    }));
    mounts.push(json!({
        "destination": home,
        "type": "bind",
        "source": home,
        "options": ["rbind", "rslave"]
    }));
    environment.insert("HOME".to_string(), home.to_string_lossy().into_owned());

    let container_data = home.join(".local/share");
    let host_data = env_path("XDG_DATA_HOME", &container_data);
    mount_xdg_directory(mounts, &host_data, &container_data)?;
    environment.insert(
        "XDG_DATA_HOME".to_string(),
        container_data.to_string_lossy().into_owned(),
    );

    let container_config = home.join(".config");
    let host_config = env_path("XDG_CONFIG_HOME", &container_config);
    let selected_config =
        existing_private_path(private_app, "config").unwrap_or_else(|| host_config.clone());
    mount_xdg_directory(mounts, &selected_config, &container_config)?;
    environment.insert(
        "XDG_CONFIG_HOME".to_string(),
        container_config.to_string_lossy().into_owned(),
    );

    let container_cache = home.join(".cache");
    let host_cache = env_path("XDG_CACHE_HOME", &container_cache);
    let selected_cache =
        existing_private_path(private_app, "cache").unwrap_or_else(|| host_cache.clone());
    mount_xdg_directory(mounts, &selected_cache, &container_cache)?;
    environment.insert(
        "XDG_CACHE_HOME".to_string(),
        container_cache.to_string_lossy().into_owned(),
    );

    let container_state = home.join(".local/state");
    let host_state = env_path("XDG_STATE_HOME", &container_state);
    let selected_state = existing_private_path(private_app, "state").unwrap_or(host_state);
    mount_xdg_directory(mounts, &selected_state, &container_state)?;
    environment.insert(
        "XDG_STATE_HOME".to_string(),
        container_state.to_string_lossy().into_owned(),
    );

    if host_config != container_config {
        for (relative, recursive) in [
            ("systemd/user", true),
            ("dconf", true),
            ("user-dirs.dirs", false),
            ("user-dirs.locale", false),
        ] {
            let source = host_config.join(relative);
            if source.exists() {
                let destination = container_config.join(relative);
                mounts.push(if recursive {
                    json!({"destination":destination,"type":"bind","source":source,"options":["rbind","rslave"]})
                } else {
                    simple_bind_mount(&source, &destination, false)
                });
            }
        }
    }
    if host_cache != container_cache {
        bind_if_exists(
            mounts,
            &host_cache.join("deepin/dde-api"),
            &container_cache.join("deepin/dde-api"),
            false,
        );
    }
    Ok(())
}

fn env_path(key: &str, fallback: &Path) -> PathBuf {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

fn existing_private_path(private_app: &Path, name: &str) -> Option<PathBuf> {
    let path = private_app.join(name);
    path.exists().then_some(path)
}

fn mount_xdg_directory(
    mounts: &mut Vec<Value>,
    source: &Path,
    destination: &Path,
) -> Result<(), String> {
    if source == destination {
        return Ok(());
    }
    fs::create_dir_all(source)
        .map_err(|error| format!("failed to create {}: {error}", source.display()))?;
    mounts.push(json!({
        "destination": destination,
        "type": "bind",
        "source": source,
        "options": ["rbind", "rslave"]
    }));
    Ok(())
}

fn append_private_mounts(
    mounts: &mut Vec<Value>,
    home: &Path,
    private_app: &Path,
) -> Result<(), String> {
    for name in [".ssh", ".gnupg"] {
        let destination = home.join(name);
        let relative = destination.strip_prefix("/").unwrap_or(&destination);
        let source = private_app.join("private").join(relative);
        fs::create_dir_all(&source)
            .map_err(|error| format!("failed to create {}: {error}", source.display()))?;
        mounts.push(json!({
            "destination": destination,
            "type": "bind",
            "source": source,
            "options": ["rbind", "rslave"]
        }));
    }
    Ok(())
}

fn append_ipc_mounts(
    mounts: &mut Vec<Value>,
    environment: &mut BTreeMap<String, String>,
    host_runtime: &Path,
    container_runtime: &Path,
) {
    bind_dbus_address(
        mounts,
        environment,
        "DBUS_SYSTEM_BUS_ADDRESS",
        Path::new("/run/dbus/system_bus_socket"),
        Some("unix:path=/var/run/dbus/system_bus_socket"),
    );
    bind_dbus_address(
        mounts,
        environment,
        "DBUS_SESSION_BUS_ADDRESS",
        &container_runtime.join("bus"),
        None,
    );
    bind_if_exists(
        mounts,
        &host_runtime.join("pulse"),
        &container_runtime.join("pulse"),
        false,
    );
    bind_if_exists(
        mounts,
        &host_runtime.join("gvfs"),
        &container_runtime.join("gvfs"),
        false,
    );
    let dconf = host_runtime.join("dconf");
    if dconf.exists() {
        mounts.push(json!({
            "destination": container_runtime.join("dconf"),
            "type": "bind",
            "source": dconf,
            "options": ["rbind", "rslave"]
        }));
    }
}

#[derive(Debug, Eq, PartialEq)]
struct DbusAddress {
    transport: String,
    options: BTreeMap<String, Vec<u8>>,
}

fn parse_dbus_addresses(raw: &str) -> Vec<DbusAddress> {
    raw.split(';')
        .filter(|address| !address.is_empty())
        .filter_map(|address| {
            let (transport, options) = address.split_once(':')?;
            let mut parsed = BTreeMap::new();
            for option in options.split(',').filter(|option| !option.is_empty()) {
                let (key, value) = option.split_once('=').unwrap_or((option, ""));
                let Some(value) = percent_decode(value.as_bytes()) else {
                    continue;
                };
                parsed.insert(key.to_string(), value);
            }
            Some(DbusAddress {
                transport: transport.to_string(),
                options: parsed,
            })
        })
        .collect()
}

fn percent_decode(value: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index] != b'%' {
            decoded.push(value[index]);
            index += 1;
            continue;
        }
        let high = *value.get(index + 1)?;
        let low = *value.get(index + 2)?;
        decoded.push(hex_value(high)? << 4 | hex_value(low)?);
        index += 3;
    }
    Some(decoded)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn percent_encode(value: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/') {
            encoded.push(char::from(*byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

fn bind_dbus_address(
    mounts: &mut Vec<Value>,
    environment: &mut BTreeMap<String, String>,
    name: &str,
    destination: &Path,
    default_address: Option<&str>,
) {
    let raw = std::env::var(name)
        .ok()
        .or_else(|| default_address.map(str::to_string));
    let Some(raw) = raw else {
        return;
    };
    for address in parse_dbus_addresses(&raw) {
        if address.transport != "unix" {
            continue;
        }
        let Some(path) = address.options.get("path") else {
            continue;
        };
        let source = PathBuf::from(OsString::from_vec(path.clone()));
        if !source.exists() {
            continue;
        }
        mounts.push(simple_bind_mount(&source, destination, false));
        let mut rewritten = format!("unix:path={}", destination.to_string_lossy());
        for (key, value) in address
            .options
            .iter()
            .filter(|(key, _)| key.as_str() != "path")
        {
            rewritten.push(',');
            rewritten.push_str(key);
            rewritten.push('=');
            rewritten.push_str(&percent_encode(value));
        }
        environment.insert(name.to_string(), rewritten);
        break;
    }
}

fn append_display_mounts(
    mounts: &mut Vec<Value>,
    environment: &mut BTreeMap<String, String>,
    host_runtime: &Path,
    container_runtime: &Path,
    wayland_socket: Option<&Path>,
) {
    if let Ok(display) = std::env::var("DISPLAY")
        && let Some(parsed) = parse_x_display(&display)
    {
        let (source, display_number, rewritten) = if parsed
            .protocol
            .as_deref()
            .is_some_and(|protocol| protocol == "unix")
            && parsed
                .host
                .as_deref()
                .is_some_and(|host| host.starts_with('/'))
        {
            let number = 1000;
            let rewritten = if parsed.screen == 0 {
                format!(":{number}")
            } else {
                format!(":{number}.{}", parsed.screen)
            };
            (PathBuf::from(parsed.host.unwrap()), number, rewritten)
        } else {
            (
                PathBuf::from(format!("/tmp/.X11-unix/X{}", parsed.display)),
                parsed.display,
                display.clone(),
            )
        };
        mounts.push(json!({
            "destination": "/tmp/.X11-unix",
            "type": "tmpfs",
            "source": "tmpfs",
            "options": ["nodev", "nosuid", "mode=700"]
        }));
        if source.exists() {
            mounts.push(simple_bind_mount(
                &source,
                &PathBuf::from(format!("/tmp/.X11-unix/X{display_number}")),
                false,
            ));
        }
        environment.insert("DISPLAY".to_string(), rewritten);
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let xauthority = std::env::var_os("XAUTHORITY")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".Xauthority"));
        if xauthority.exists() {
            mounts.push(simple_bind_mount(
                &xauthority,
                Path::new("/run/linglong/Xauthority"),
                false,
            ));
            environment.insert(
                "XAUTHORITY".to_string(),
                "/run/linglong/Xauthority".to_string(),
            );
        }
    }
    let source = wayland_socket.map(Path::to_path_buf).or_else(|| {
        let display = std::env::var("WAYLAND_DISPLAY").ok()?;
        if display.is_empty() {
            return None;
        }
        Some(if display.starts_with('/') {
            PathBuf::from(display)
        } else {
            host_runtime.join(display)
        })
    });
    if let Some(source) = source.filter(|source| source.exists()) {
        let destination = container_runtime.join("wayland-0");
        mounts.push(simple_bind_mount(&source, &destination, false));
        environment.insert(
            "WAYLAND_DISPLAY".to_string(),
            destination.to_string_lossy().into_owned(),
        );
    }
}

struct XDisplay {
    protocol: Option<String>,
    host: Option<String>,
    display: i32,
    screen: i32,
}

fn parse_x_display(raw: &str) -> Option<XDisplay> {
    let mut display = raw.strip_prefix("unix:").unwrap_or(raw);
    if display.starts_with('/') {
        if Path::new(display).exists() {
            return Some(XDisplay {
                protocol: Some("unix".to_string()),
                host: Some(display.to_string()),
                display: 0,
                screen: 0,
            });
        }
        let (path, screen) = display.rsplit_once('.')?;
        if !Path::new(path).exists() {
            return None;
        }
        return Some(XDisplay {
            protocol: Some("unix".to_string()),
            host: Some(path.to_string()),
            display: 0,
            screen: screen.parse::<i32>().ok().filter(|screen| *screen >= 0)?,
        });
    }
    let protocol = display.rfind('/').map(|slash| {
        let protocol = display[..slash].to_string();
        display = &display[slash + 1..];
        protocol
    });
    let colon = display.rfind(':')?;
    let host = (!display[..colon].is_empty()).then(|| display[..colon].to_string());
    let number = &display[colon + 1..];
    let (display_number, screen) = number.split_once('.').unwrap_or((number, "0"));
    let display_number = display_number
        .parse::<i32>()
        .ok()
        .filter(|display| *display >= 0)?;
    let screen = screen.parse::<i32>().ok().filter(|screen| *screen >= 0)?;
    Some(XDisplay {
        protocol,
        host,
        display: display_number,
        screen,
    })
}

fn bind_if_exists(mounts: &mut Vec<Value>, source: &Path, destination: &Path, read_only: bool) {
    if source.exists() {
        mounts.push(bind_mount(source, destination, read_only));
    }
}

fn prepare_container_environment(
    bundle: &Path,
    environment: &BTreeMap<String, String>,
    mounts: &mut Vec<Value>,
) -> Result<(), String> {
    let environment_path = bundle.join("00env.sh");
    let mut environment_script = String::new();
    for (key, value) in environment {
        environment_script.push_str("export ");
        environment_script.push_str(key);
        environment_script.push_str("='");
        environment_script.push_str(&value.replace('\'', "'\\''"));
        environment_script.push_str("'\n");
    }
    fs::write(&environment_path, environment_script)
        .map_err(|error| format!("failed to write {}: {error}", environment_path.display()))?;
    mounts.push(simple_bind_mount(
        &environment_path,
        Path::new("/etc/profile.d/00env.sh"),
        true,
    ));
    Ok(())
}

fn prepare_container_entrypoint(
    bundle: &Path,
    arguments: &[String],
    mounts: &mut Vec<Value>,
) -> Result<Vec<String>, String> {
    let entrypoint_path = bundle.join("entrypoint.sh");
    fs::write(&entrypoint_path, entrypoint_script(arguments))
        .map_err(|error| format!("failed to write {}: {error}", entrypoint_path.display()))?;
    fs::set_permissions(&entrypoint_path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("failed to chmod {}: {error}", entrypoint_path.display()))?;
    mounts.push(json!({
        "destination": "/run/linglong/entrypoint.sh",
        "type": "bind",
        "source": entrypoint_path,
        "options": ["ro", "rbind"]
    }));

    let container_init = container_init_binary()?;
    mounts.push(json!({
        "destination": "/run/linglong/container-init",
        "type": "bind",
        "source": container_init,
        "options": ["ro", "rbind"]
    }));
    Ok(vec![
        "/run/linglong/container-init".to_string(),
        "/bin/bash".to_string(),
        "--noprofile".to_string(),
        "--norc".to_string(),
        "-c".to_string(),
        "/run/linglong/entrypoint.sh".to_string(),
    ])
}

fn finalize_container_process(
    configuration: &mut Value,
    bundle: &Path,
    arguments: &[String],
    requested_workdir: Option<&str>,
    uid: u32,
    gid: u32,
) -> Result<(), String> {
    let process = configuration
        .as_object_mut()
        .ok_or_else(|| "OCI configuration is not an object".to_string())?
        .entry("process")
        .or_insert_with(|| json!({}));
    let process = process
        .as_object_mut()
        .ok_or_else(|| "OCI process is not an object".to_string())?;
    merge_final_process_fields(
        process,
        requested_workdir,
        uid,
        gid,
        std::io::stdin().is_terminal(),
    )?;
    let mut entrypoint_mounts = Vec::new();
    let process_arguments =
        prepare_container_entrypoint(bundle, arguments, &mut entrypoint_mounts)?;
    process.insert("args".to_string(), json!(process_arguments));
    configuration
        .get_mut("mounts")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "OCI mounts are not an array".to_string())?
        .extend(entrypoint_mounts);
    Ok(())
}

fn merge_final_process_fields(
    process: &mut serde_json::Map<String, Value>,
    requested_workdir: Option<&str>,
    uid: u32,
    gid: u32,
    terminal: bool,
) -> Result<(), String> {
    if let Some(requested) = requested_workdir.filter(|requested| !requested.is_empty()) {
        if !Path::new(requested).is_absolute() {
            return Err(format!("workdir must be an absolute path: {requested}"));
        }
        process.insert("cwd".to_string(), Value::String(requested.to_string()));
    } else if process
        .get("cwd")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        process.insert(
            "cwd".to_string(),
            Value::String(container_working_directory(None)?),
        );
    }
    if process.get("user").is_none_or(Value::is_null) {
        process.insert("user".to_string(), json!({"uid": uid, "gid": gid}));
    }
    set_process_terminal(process, terminal);
    Ok(())
}

fn entrypoint_script(arguments: &[String]) -> String {
    let mut script = "source /etc/profile\nexec ".to_string();
    for argument in arguments {
        script.push_str(&quote_bash_argument(argument));
        script.push(' ');
    }
    script
}

fn quote_bash_argument(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\\''"))
}

fn container_working_directory(requested: Option<&str>) -> Result<String, String> {
    if let Some(requested) = requested.filter(|requested| !requested.is_empty()) {
        if !Path::new(requested).is_absolute() {
            return Err(format!("workdir must be an absolute path: {requested}"));
        }
        return Ok(requested.to_string());
    }
    let current = std::env::current_dir()
        .map_err(|error| format!("failed to get current working directory: {error}"))?;
    Ok(Path::new("/run/host/rootfs")
        .join(current.strip_prefix("/").unwrap_or(&current))
        .to_string_lossy()
        .into_owned())
}

fn container_init_binary() -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("LINYAPS_CONTAINER_INIT") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("ll-init"));
    }
    candidates.extend([
        PathBuf::from("/usr/libexec/linglong/ll-init"),
        PathBuf::from("/usr/local/libexec/linglong/ll-init"),
        PathBuf::from("/usr/bin/ll-init"),
    ]);
    candidates
        .into_iter()
        .find(|path| {
            fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or_else(|| {
            "ll-init was not found; set LINYAPS_CONTAINER_INIT to its executable path".to_string()
        })
}

fn device_passthrough_enabled(options: &Run, runtime_config: Option<&RuntimeConfigure>) -> bool {
    options
        .device_mode
        .iter()
        .any(|mode| matches!(mode, super::DeviceMode::Passthru))
        || runtime_config
            .and_then(|config| config.device_mode.as_deref())
            .is_some_and(|modes| {
                modes
                    .iter()
                    .any(|mode| matches!(mode, linyaps_api::DeviceOption::Passthru))
            })
}

fn bind_mount(source: &Path, destination: &Path, read_only: bool) -> Value {
    let options = vec!["rbind", if read_only { "ro" } else { "rw" }, "rslave"];
    json!({
        "destination": destination,
        "type": "bind",
        "source": source,
        "options": options
    })
}

fn simple_bind_mount(source: &Path, destination: &Path, read_only: bool) -> Value {
    let mut options = vec!["bind"];
    if read_only {
        options.push("ro");
    }
    json!({
        "destination": destination,
        "type": "bind",
        "source": source,
        "options": options
    })
}

fn append_permission_mounts(
    mounts: &mut Vec<Value>,
    permissions: Option<&ApplicationConfigurationPermissions>,
    rootfs: &Path,
) -> Result<(), String> {
    let Some(permissions) = permissions else {
        return Ok(());
    };
    for binding in permissions.binds.as_deref().into_iter().flatten() {
        mounts.push(simple_bind_mount(
            Path::new(&binding.source),
            Path::new(&binding.destination),
            false,
        ));
    }
    for binding in permissions.inner_binds.as_deref().into_iter().flatten() {
        let source = rootfs_path(rootfs, Path::new(&binding.source))?;
        mounts.push(simple_bind_mount(
            &source,
            Path::new(&binding.destination),
            false,
        ));
    }
    Ok(())
}

fn apply_cdi_edits(
    environment: &mut BTreeMap<String, String>,
    mounts: &mut Vec<Value>,
    context: &RunContextConfig,
    mount_device_nodes: bool,
) -> Result<(), String> {
    for device in context.cdi_devices.as_deref().into_iter().flatten() {
        let edits = cdi::get_device_edits(device).map_err(|error| {
            format!(
                "failed to resolve CDI device edits {}={}: {error}",
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
        if mount_device_nodes {
            for node in edits.device_nodes.as_deref().into_iter().flatten() {
                mounts.push(simple_bind_mount(
                    Path::new(node.host_path.as_deref().unwrap_or(&node.path)),
                    Path::new(&node.path),
                    false,
                ));
            }
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
    }
    Ok(())
}

fn container_id(config: &RunContextConfig) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(config).map_err(|error| error.to_string())?);
    Ok(format!("{:x}", hasher.finalize()))
}

fn rootfs_path(rootfs: &Path, path: &Path) -> Result<PathBuf, String> {
    let mut output = rootfs.to_path_buf();
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::Normal(value) => output.push(value),
            std::path::Component::ParentDir | std::path::Component::Prefix(_) => {
                return Err(format!(
                    "mount destination {} is outside rootfs {}",
                    path.display(),
                    rootfs.display()
                ));
            }
        }
    }
    Ok(output)
}

fn resolve_resolv_conf() -> Result<Option<String>, String> {
    let path = Path::new("/etc/resolv.conf");
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "failed to get status of {}: {error}",
                path.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return match fs::canonicalize(path) {
            Ok(target) => Ok(Some(target.to_string_lossy().into_owned())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!(
                "failed to resolve symlink {}: {error}",
                path.display()
            )),
        };
    }
    Ok(Some(path.to_string_lossy().into_owned()))
}

fn resolve_timezone() -> Result<Option<String>, String> {
    let zoneinfo = std::env::var_os("TZDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/zoneinfo"));
    let localtime = Path::new("/etc/localtime");
    let metadata = match fs::symlink_metadata(localtime) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Some("UTC".to_string()));
        }
        Err(error) => {
            return Err(format!(
                "failed to get status of {}: {error}",
                localtime.display()
            ));
        }
    };
    if !metadata.file_type().is_symlink() {
        return Ok(Some(String::new()));
    }
    let target = fs::read_link(localtime)
        .map_err(|error| format!("failed to read symlink {}: {error}", localtime.display()))?;
    let target = if target.is_absolute() {
        target
    } else {
        localtime.parent().unwrap_or(Path::new("/")).join(target)
    };
    if let Some(timezone) = timezone_from_path(&target, &zoneinfo) {
        return Ok(Some(timezone));
    }
    let target = fs::canonicalize(&target)
        .map_err(|error| format!("failed to canonicalize {}: {error}", target.display()))?;
    Ok(timezone_from_path(&target, &zoneinfo))
}

fn timezone_from_path(path: &Path, zoneinfo: &Path) -> Option<String> {
    let path = lexical_normalize(path);
    let zoneinfo = lexical_normalize(zoneinfo);
    let relative = path.strip_prefix(zoneinfo).ok()?;
    (!relative.as_os_str().is_empty()).then(|| relative.to_string_lossy().into_owned())
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                output.pop();
            }
            component => output.push(component.as_os_str()),
        }
    }
    output
}

fn resolve_overlay_mode(repository: &LocalRepository, base: &Reference) -> Result<String, String> {
    let base_layer = repository
        .merged_layer_path(base)
        .map_err(|error| error.to_string())?
        .join("files");
    let owner = fs::metadata(&base_layer)
        .map_err(|error| {
            format!(
                "failed to inspect base layer {}: {error}",
                base_layer.display()
            )
        })?
        .uid();
    let current_uid = rustix::process::getuid().as_raw();
    let owner_matches = owner == current_uid;
    let fuse_available = executable_in_path("fuse-overlayfs");
    if !owner_matches {
        if let Some(mode) = select_overlay_mode(false, false, false, fuse_available) {
            return Ok(mode.to_string());
        }
        return Err(format!(
            "base layer {} is owned by uid {owner}; fuse-overlayfs is required for uid {current_uid}",
            base_layer.display()
        ));
    }
    let overlay_available = fs::read_to_string("/proc/filesystems").is_ok_and(|content| {
        content
            .lines()
            .any(|line| line.split_whitespace().last() == Some("overlay"))
    });
    let release_supports_userns = fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .and_then(|release| {
            let mut parts = release.trim().split('.');
            Some((
                parts.next()?.parse::<u32>().ok()?,
                parts.next()?.parse::<u32>().ok()?,
            ))
        })
        .is_some_and(|(major, minor)| major > 5 || (major == 5 && minor >= 11));
    select_overlay_mode(
        owner_matches,
        overlay_available,
        release_supports_userns,
        fuse_available,
    )
    .map(str::to_string)
    .ok_or_else(|| "no available overlayfs implementation".to_string())
}

fn select_overlay_mode(
    layer_owner_matches: bool,
    kernel_overlay_available: bool,
    release_supports_userns: bool,
    fuse_available: bool,
) -> Option<&'static str> {
    if !layer_owner_matches {
        return fuse_available.then_some("fuse");
    }
    if kernel_overlay_available && release_supports_userns {
        return Some("kernel");
    }
    fuse_available.then_some("fuse")
}

fn executable_in_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| {
            let path = directory.join(name);
            fs::metadata(path).is_ok_and(|metadata| {
                use std::os::unix::fs::PermissionsExt;
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    })
}

enum MountedOverlayKind {
    Kernel,
    Fuse,
}

struct OverlayMount {
    merged: PathBuf,
    kind: MountedOverlayKind,
}

impl OverlayMount {
    fn mount(
        mode: &str,
        base_layer: &Path,
        app_cache: &Path,
        bundle: &Path,
    ) -> Result<Self, String> {
        let merged = bundle.join("rootfs");
        let overlay = bundle.join("overlay");
        let upper = overlay.join("upperdir");
        let work = overlay.join("workdir");
        fs::create_dir_all(&upper).map_err(|error| {
            format!(
                "failed to create overlay upper directory {}: {error}",
                upper.display()
            )
        })?;
        fs::create_dir_all(&work).map_err(|error| {
            format!(
                "failed to create overlay work directory {}: {error}",
                work.display()
            )
        })?;

        let persistent_upper = app_cache.join("overlay/upperdir");
        let mut lower = Vec::with_capacity(2);
        if persistent_upper.is_dir() {
            lower.push(persistent_upper);
        }
        lower.push(base_layer.to_path_buf());
        let lower = lower
            .iter()
            .map(|path| escape_overlay_path(path))
            .collect::<Vec<_>>()
            .join(":");
        let options = format!(
            "lowerdir={lower},upperdir={},workdir={}",
            escape_overlay_path(&upper),
            escape_overlay_path(&work)
        );

        let kind = match mode {
            "kernel" => {
                mount_kernel_overlay(&merged, &format!("{options},userxattr"))?;
                MountedOverlayKind::Kernel
            }
            "fuse" => {
                let uid = rustix::process::getuid().as_raw();
                let gid = rustix::process::getgid().as_raw();
                let options = format!("{options},squash_to_uid={uid},squash_to_gid={gid}");
                let output = Command::new("fuse-overlayfs")
                    .args(["-o", &options])
                    .arg(&merged)
                    .output()
                    .map_err(|error| format!("failed to execute fuse-overlayfs: {error}"))?;
                if !output.status.success() {
                    return Err(format!(
                        "fuse-overlayfs failed for {}: {}",
                        merged.display(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    ));
                }
                MountedOverlayKind::Fuse
            }
            value => return Err(format!("invalid overlayfs mode: {value}")),
        };
        Ok(Self { merged, kind })
    }

    fn unmount(&self) {
        match self.kind {
            MountedOverlayKind::Kernel => {
                if unsafe { libc::umount(path_c_string(&self.merged).as_ptr()) } != 0 {
                    unsafe {
                        libc::umount2(path_c_string(&self.merged).as_ptr(), libc::MNT_DETACH);
                    }
                }
            }
            MountedOverlayKind::Fuse => {
                if unsafe { libc::umount(path_c_string(&self.merged).as_ptr()) } == 0 {
                    return;
                }
                let command = if executable_in_path("fusermount") {
                    "fusermount"
                } else {
                    "fusermount3"
                };
                let success = Command::new(command)
                    .arg("-u")
                    .arg(&self.merged)
                    .status()
                    .is_ok_and(|status| status.success());
                if !success {
                    let lazy_success = Command::new(command)
                        .args(["-z", "-u"])
                        .arg(&self.merged)
                        .status()
                        .is_ok_and(|status| status.success());
                    if !lazy_success {
                        unsafe {
                            libc::umount2(path_c_string(&self.merged).as_ptr(), libc::MNT_DETACH);
                        }
                    }
                }
            }
        }
    }
}

impl Drop for OverlayMount {
    fn drop(&mut self) {
        self.unmount();
    }
}

fn mount_kernel_overlay(merged: &Path, options: &str) -> Result<(), String> {
    let source = CString::new("none").expect("static string has no NUL byte");
    let filesystem = CString::new("overlay").expect("static string has no NUL byte");
    let target = path_c_string(merged);
    let options = CString::new(options)
        .map_err(|_| "overlayfs options contain an embedded NUL byte".to_string())?;
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            filesystem.as_ptr(),
            0,
            options.as_ptr().cast(),
        )
    } != 0
    {
        return Err(format!(
            "failed to mount overlayfs at {}: {}",
            merged.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn path_c_string(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).expect("filesystem path contains a NUL byte")
}

fn escape_overlay_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(':', "\\:")
        .replace(',', "\\,")
}

fn wait_for_container_pid(container_id: &str, fallback: u32) -> Result<i64, String> {
    let status = xdg_runtime_dir()
        .join("linglong/box")
        .join(container_id)
        .join("status.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(content) = fs::read(&status)
            && let Ok(value) = serde_json::from_slice::<Value>(&content)
            && let Some(pid) = value.get("pid").and_then(Value::as_i64)
            && pid > 0
        {
            return Ok(pid);
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(i64::from(fallback))
}

fn write_process_state(
    pid: i64,
    container_id: &str,
    app: &Reference,
    base: &Reference,
    runtime: Option<&Reference>,
    extensions: &[Reference],
) -> Result<PathBuf, String> {
    let _repo_lock = RepoLock::shared().map_err(|error| error.to_string())?;
    let root = process_state_root();
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let path = root.join(pid.to_string());
    let state = ContainerProcessStateInfo {
        app: app.to_string(),
        base: base.to_string(),
        container_id: container_id.to_string(),
        extensions: (!extensions.is_empty())
            .then(|| extensions.iter().map(ToString::to_string).collect()),
        runtime: runtime.map(ToString::to_string),
    };
    fs::write(
        &path,
        serde_json::to_vec(&state).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(path)
}

fn overlay_tree(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.is_dir() {
            if fs::symlink_metadata(&destination_path).is_ok_and(|current| !current.is_dir()) {
                clear_path(&destination_path)?;
            }
            fs::create_dir_all(&destination_path)?;
            overlay_tree(&source_path, &destination_path)?;
        } else {
            clear_path(&destination_path)?;
            if metadata.file_type().is_symlink() {
                symlink(fs::read_link(&source_path)?, &destination_path)?;
            } else if metadata.is_file() && fs::hard_link(&source_path, &destination_path).is_err()
            {
                fs::copy(&source_path, &destination_path)?;
                fs::set_permissions(&destination_path, metadata.permissions())?;
            }
        }
    }
    Ok(())
}

fn clear_path(path: &Path) -> Result<(), std::io::Error> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

struct BundleCleanup {
    path: PathBuf,
    armed: bool,
}

impl BundleCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn finish(mut self) -> Result<(), String> {
        self.armed = false;
        cleanup_bundle(&self.path, Duration::from_secs(3))
            .map_err(|error| format!("failed to clean up {}: {error}", self.path.display()))
    }
}

impl Drop for BundleCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = cleanup_bundle(&self.path, Duration::from_secs(3));
        }
    }
}

fn cleanup_bundle(path: &Path, timeout: Duration) -> Result<(), std::io::Error> {
    let rootfs = path.join("rootfs");
    if let Ok(rootfs) = CString::new(rootfs.as_os_str().as_bytes()) {
        unsafe {
            libc::umount2(rootfs.as_ptr(), libc::MNT_DETACH);
        }
    }
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    loop {
        if !path_is_mounted(&rootfs) {
            match make_directories_removable(path).and_then(|()| clear_path(path)) {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        if Instant::now() >= deadline {
            return Err(last_error.unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("{} remained mounted", rootfs.display()),
                )
            }));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn path_is_mounted(path: &Path) -> bool {
    let Some(path) = path.to_str() else {
        return true;
    };
    fs::read_to_string("/proc/self/mountinfo").is_ok_and(|mountinfo| {
        mountinfo
            .lines()
            .any(|line| line.split_whitespace().nth(4) == Some(path))
    })
}

fn make_directories_removable(path: &Path) -> Result<(), std::io::Error> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(permissions.mode() | 0o700);
    fs::set_permissions(path, permissions)?;
    for entry in fs::read_dir(path)? {
        make_directories_removable(&entry?.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package_info(name: &str) -> PackageInfoV2 {
        PackageInfoV2 {
            arch: vec!["x86_64".to_string()],
            base: String::new(),
            channel: "stable".to_string(),
            command: None,
            compatible_version: None,
            description: None,
            extension_implementation: None,
            extensions: None,
            id: "org.example.Shared".to_string(),
            kind: "app".to_string(),
            module: "binary".to_string(),
            name: name.to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 0,
            uuid: None,
            version: "1.0.0.0".to_string(),
        }
    }

    #[test]
    fn context_target_prefers_app_then_runtime_then_base() {
        let app = "stable:org.example.Shared/1.0.0.0/x86_64"
            .parse::<Reference>()
            .unwrap();
        let runtime = "stable:org.example.Shared/2.0.0.0/x86_64"
            .parse::<Reference>()
            .unwrap();
        let base = "stable:org.example.Shared/3.0.0.0/x86_64"
            .parse::<Reference>()
            .unwrap();

        let (selected, info) = find_context_target(
            "org.example.Shared",
            Some((&app, Ok(package_info("app")))),
            Some((&runtime, Ok(package_info("runtime")))),
            (&base, Ok(package_info("base"))),
        )
        .unwrap();

        assert_eq!(selected, &app);
        assert_eq!(info.name, "app");
    }

    #[test]
    fn replaces_desktop_field_codes() {
        let mut arguments = vec!["demo".to_string(), "%f".to_string(), "%u".to_string()];
        replace_field_codes(
            &mut arguments,
            &["/tmp/file".to_string()],
            &["https://example.com".to_string()],
        );
        assert_eq!(arguments[1], "/tmp/file");
        assert_eq!(arguments[2], "https://example.com");
    }

    #[test]
    fn overlays_later_files_and_preserves_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        let output = temporary.path().join("output");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::create_dir_all(&output).unwrap();
        fs::write(first.join("value"), "old").unwrap();
        fs::write(second.join("value"), "new").unwrap();
        symlink("value", second.join("link")).unwrap();
        overlay_tree(&first, &output).unwrap();
        overlay_tree(&second, &output).unwrap();
        assert_eq!(fs::read_to_string(output.join("value")).unwrap(), "new");
        assert_eq!(
            fs::read_link(output.join("link")).unwrap(),
            Path::new("value")
        );
    }

    #[test]
    fn foreign_owned_layers_require_fuse_overlay() {
        assert_eq!(select_overlay_mode(false, true, true, true), Some("fuse"));
        assert_eq!(select_overlay_mode(false, true, true, false), None);
        assert_eq!(select_overlay_mode(true, true, true, false), Some("kernel"));
    }

    #[test]
    fn bundle_cleanup_removes_restricted_overlay_workdirs() {
        let temporary = tempfile::tempdir().unwrap();
        let bundle = temporary.path().join("bundle");
        let work = bundle.join("overlay/workdir/work");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("index"), "test").unwrap();
        let mut permissions = fs::metadata(&work).unwrap().permissions();
        permissions.set_mode(0o0);
        fs::set_permissions(&work, permissions).unwrap();

        BundleCleanup::new(bundle.clone()).finish().unwrap();

        assert!(!bundle.exists());
    }

    #[test]
    fn container_id_is_stable_for_same_run_context() {
        let config = RunContextConfig {
            app: Some("stable/org.example.App/1.0.0.0/x86_64".to_string()),
            base: Some("stable/org.example.Base/1.0.0.0/x86_64".to_string()),
            cdi_devices: None,
            extensions: Some(Default::default()),
            instance: Some("test".to_string()),
            mounts: None,
            overlayfs: None,
            resolv_conf: None,
            runtime: None,
            timezone: None,
            version: "1".to_string(),
        };
        assert_eq!(
            container_id(&config).unwrap(),
            container_id(&config).unwrap()
        );
        assert_eq!(container_id(&config).unwrap().len(), 64);
    }

    #[test]
    fn binds_resolv_conf_without_rewriting_rootfs() {
        let temporary = tempfile::tempdir().unwrap();
        let rootfs = temporary.path().join("rootfs");
        fs::create_dir_all(rootfs.join("etc")).unwrap();
        fs::write(rootfs.join("etc/resolv.conf"), "from-base").unwrap();
        let config = RunContextConfig {
            app: None,
            base: Some("stable/org.example.Base/1.0.0.0/x86_64".to_string()),
            cdi_devices: None,
            extensions: None,
            instance: None,
            mounts: None,
            overlayfs: None,
            resolv_conf: Some("/etc/resolv.conf".to_string()),
            runtime: None,
            timezone: None,
            version: "1".to_string(),
        };

        let mut mounts = Vec::new();
        append_network_and_timezone_mounts(&mut mounts, &config);

        assert!(rootfs.join("etc").is_dir());
        assert_eq!(
            fs::read_to_string(rootfs.join("etc/resolv.conf")).unwrap(),
            "from-base"
        );
        assert!(mounts.iter().any(|mount| {
            mount["destination"] == "/etc/resolv.conf" && mount["source"] == "/etc/resolv.conf"
        }));
    }

    #[test]
    fn validates_xdg_desktop_portal_application_ids() {
        for valid in [
            "org.example.app",
            "org.example.my-app",
            "org.example_my.app",
            "org.2example.app",
            "org.app",
        ] {
            assert!(valid_xdp_app_id(valid), "{valid}");
        }
        for invalid in [
            "app",
            "org.my-app.app",
            "org..app",
            ".org.app",
            "org.app.",
            "org.example.app!",
            "org/example/app",
        ] {
            assert!(!valid_xdp_app_id(invalid), "{invalid}");
        }
    }

    #[test]
    fn parses_and_reencodes_dbus_addresses() {
        let addresses = parse_dbus_addresses(
            "tcp:host=example;unix:path=/run/user/1000/bus,guid=hello%20world;invalid",
        );
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[1].transport, "unix");
        assert_eq!(
            addresses[1].options.get("path").unwrap(),
            b"/run/user/1000/bus"
        );
        assert_eq!(addresses[1].options.get("guid").unwrap(), b"hello world");
        assert_eq!(percent_encode(b"hello world/%"), "hello%20world/%25");
        assert_eq!(percent_decode(b"%00%aF%FF").unwrap(), [0, 0xaf, 0xff]);
        assert!(percent_decode(b"broken%").is_none());
    }

    #[test]
    fn parses_standard_x_displays() {
        let local = parse_x_display(":0.1").unwrap();
        assert_eq!(local.protocol, None);
        assert_eq!(local.host, None);
        assert_eq!(local.display, 0);
        assert_eq!(local.screen, 1);

        let remote = parse_x_display("tcp/[2001:db8::1]:10.2").unwrap();
        assert_eq!(remote.protocol.as_deref(), Some("tcp"));
        assert_eq!(remote.host.as_deref(), Some("[2001:db8::1]"));
        assert_eq!(remote.display, 10);
        assert_eq!(remote.screen, 2);
        assert!(parse_x_display("localhost:abc").is_none());
        assert!(parse_x_display("localhost:0.abc").is_none());
    }

    #[test]
    fn creates_minimal_account_records() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\ndemo:*:1000:100:Demo:/home/demo:/bin/zsh\n";
        let group = "root:x:0:\nusers:*:100:\n";
        assert_eq!(
            account_line(passwd, 1000, true).as_deref(),
            Some("demo:x:1000:100:Demo:/home/demo:/bin/zsh\n")
        );
        assert_eq!(
            account_line(group, 100, false).as_deref(),
            Some("users:x:100:\n")
        );
    }

    #[test]
    fn quotes_entrypoint_arguments_for_bash() {
        assert_eq!(quote_bash_argument("simple"), "'simple'");
        assert_eq!(quote_bash_argument("let's go"), "'let'\\''s go'");
        assert_eq!(quote_bash_argument(""), "''");
    }

    #[test]
    fn maps_default_workdir_to_host_root() {
        let current = std::env::current_dir().unwrap();
        assert_eq!(
            container_working_directory(None).unwrap(),
            Path::new("/run/host/rootfs")
                .join(current.strip_prefix("/").unwrap())
                .to_string_lossy()
        );
        assert_eq!(
            container_working_directory(Some("/workspace")).unwrap(),
            "/workspace"
        );
        assert!(container_working_directory(Some("relative")).is_err());
    }

    #[test]
    fn final_process_merge_preserves_patched_defaults() {
        let mut process = json!({
            "args": ["patched"],
            "cwd": "/patched",
            "user": {"uid": 42, "gid": 43},
            "terminal": false
        });
        merge_final_process_fields(process.as_object_mut().unwrap(), None, 1000, 1000, false)
            .unwrap();
        assert_eq!(process["cwd"], "/patched");
        assert_eq!(process["user"], json!({"uid": 42, "gid": 43}));
        assert_eq!(process["terminal"], false);

        merge_final_process_fields(
            process.as_object_mut().unwrap(),
            Some("/requested"),
            1000,
            1000,
            true,
        )
        .unwrap();
        assert_eq!(process["cwd"], "/requested");
        assert_eq!(process["terminal"], true);
    }

    #[test]
    fn omits_false_terminal_to_match_optional_upstream_field() {
        let mut process = json!({"args": ["true"]});
        set_process_terminal(process.as_object_mut().unwrap(), false);
        assert!(process.get("terminal").is_none());

        set_process_terminal(process.as_object_mut().unwrap(), true);
        assert_eq!(process["terminal"], true);
    }
}
