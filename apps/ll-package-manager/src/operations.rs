use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_lock::Mutex;
use linyaps_api::{
    CommonOptions, ContainerProcessStateInfo, ExtensionDefine, InteractionMessageType,
    PackageInfoV2, PackageManagerInstallParameters, PackageManagerPackage,
    PackageManagerUninstallParameters, PackageManagerUpdateParameters, Repo, RepoConfigV2,
    RepositoryCacheLayersItem, UabLayer,
};
use linyaps_core::repo_lock::RepoLock;
use linyaps_core::repository::priority_grouped_repos;
use linyaps_core::{Architecture, FuzzyReference, Reference};
use linyaps_dbus::{TaskCompletion, TaskContext, VariantMap, common_result, owned_string};
use linyaps_repository::{
    LocalRepository, RemotePackages, RemoteRefMetadata, RemoteRepositoryClient, UabFile,
    read_layer_info_from, reference_from_info, unpack_layer_file,
};

use crate::install_hooks::InstallHooks;

pub type SharedRepository = Arc<Mutex<LocalRepository>>;

const INSTALL_FAILED: i64 = 2001;
const INSTALL_NOT_FOUND: i64 = 2002;
const INSTALL_ALREADY_INSTALLED: i64 = 2003;
const INSTALL_NEED_DOWNGRADE: i64 = 2004;
const INSTALL_MODULE_REQUIRES_APP: i64 = 2006;
const INSTALL_MODULE_EXISTS: i64 = 2007;
const INSTALL_ARCH_MISMATCH: i64 = 2008;
const INSTALL_MODULE_NOT_FOUND: i64 = 2009;
const INSTALL_UNSUPPORTED_FILE: i64 = 2011;
const UNINSTALL_FAILED: i64 = 2101;
const UNINSTALL_NOT_FOUND: i64 = 2102;
const UNINSTALL_RUNNING: i64 = 2103;
const UNINSTALL_MULTIPLE_VERSIONS: i64 = 2105;
const UNINSTALL_BASE_OR_RUNTIME: i64 = 2106;
const UPDATE_FAILED: i64 = 2201;
const UPDATE_LOCAL_NOT_FOUND: i64 = 2202;
const NETWORK_ERROR: i64 = 3001;

#[derive(Debug)]
struct OperationError {
    code: i64,
    message: String,
}

impl OperationError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

struct RemoteSelection {
    repo: Repo,
    package: PackageInfoV2,
    packages: RemotePackages,
}

#[derive(Clone, Debug)]
struct PlannedRemoteModule {
    reference: Reference,
    repo: Repo,
    module: String,
    metadata: RemoteRefMetadata,
}

#[derive(Debug, Default)]
struct DownloadProgress {
    total_size: u64,
    needed_size: u64,
    fetched_size: u64,
}

#[derive(Clone, Debug, Default)]
struct GatheredUpdate {
    remote: Option<Reference>,
    package_info: Option<PackageInfoV2>,
}

pub async fn install(
    repository: SharedRepository,
    parameters: PackageManagerInstallParameters,
    context: TaskContext,
) -> Result<TaskCompletion, String> {
    match install_inner(repository, parameters, context).await {
        Ok(completion) => Ok(completion),
        Err(error) => Ok(TaskCompletion::failed(error.code, error.message)),
    }
}

pub async fn install_file(
    repository: SharedRepository,
    file: File,
    file_type: String,
    options: CommonOptions,
    context: TaskContext,
) -> Result<TaskCompletion, String> {
    if let Err(error) = InstallHooks::load().and_then(|hooks| hooks.pre_install(&file)) {
        return Ok(TaskCompletion::failed(
            INSTALL_FAILED,
            format!("install hook verification failed: {error:#}"),
        ));
    }
    let result = match file_type.as_str() {
        "layer" => install_layer_inner(repository, file, options, context).await,
        "uab" => install_uab_inner(repository, file, options, context).await,
        _ => Err(OperationError::new(
            INSTALL_UNSUPPORTED_FILE,
            format!("{file_type} is unsupported fileType"),
        )),
    };
    match result {
        Ok(completion) => Ok(completion),
        Err(error) => Ok(TaskCompletion::failed(error.code, error.message)),
    }
}

async fn install_uab_inner(
    repository: SharedRepository,
    file: File,
    options: CommonOptions,
    context: TaskContext,
) -> Result<TaskCompletion, OperationError> {
    let uab = UabFile::from_file(file)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    uab.verify()
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let metadata = uab
        .metadata()
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    if metadata.version != "1" {
        return Err(OperationError::new(
            INSTALL_FAILED,
            format!("unsupported UAB metadata version {}", metadata.version),
        ));
    }
    let executable_mode = metadata.only_app.unwrap_or(false);
    let (app_layers, other_layers) = validate_uab_layout(&metadata.layers, executable_mode)?;

    let selected_layers = if app_layers.is_empty() {
        &other_layers
    } else {
        &app_layers
    };
    let selected_info = &selected_layers[0].info;
    let target = reference_from_info(selected_info)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let extra_only = selected_layers
        .iter()
        .all(|layer| !matches!(layer.info.module.as_str(), "binary" | "runtime"));
    let fuzzy = FuzzyReference::new(
        Some(target.channel.clone()),
        &target.id,
        None,
        Some(target.architecture),
    )
    .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let mut repository = repository.lock().await;
    validate_uab_extra_modules(&repository, &app_layers)?;
    validate_uab_extra_modules(&repository, &other_layers)?;
    let exact_main_module_installed = repository.layer_item(&target, "binary").is_ok();
    let local = if extra_only {
        Some(target.clone())
    } else {
        latest_local_reference(&repository, &fuzzy)?
    };
    if extra_only {
        let installed = local.as_ref().ok_or_else(|| {
            OperationError::new(
                INSTALL_MODULE_REQUIRES_APP,
                "no matched binary module found",
            )
        })?;
        if installed.version != target.version {
            return Err(OperationError::new(
                INSTALL_MODULE_REQUIRES_APP,
                "no matched binary module found",
            ));
        }
        if selected_layers
            .iter()
            .all(|layer| repository.layer_item(installed, &layer.info.module).is_ok())
        {
            return Err(OperationError::new(
                INSTALL_MODULE_EXISTS,
                "package already installed",
            ));
        }
    } else if exact_main_module_installed {
        return Err(OperationError::new(
            INSTALL_ALREADY_INSTALLED,
            "package already installed",
        ));
    }
    if selected_info.kind == "app"
        && local.as_ref().is_some_and(|installed| {
            target.version.partial_cmp(&installed.version) == Some(Ordering::Less)
        })
        && !options.force
    {
        return Err(OperationError::new(
            INSTALL_NEED_DOWNGRADE,
            "latest version already installed",
        ));
    }
    if selected_info.kind == "app"
        && local.as_ref().is_some_and(|installed| {
            target.version.partial_cmp(&installed.version) == Some(Ordering::Greater)
        })
        && !options.skip_interaction
    {
        let mut additional = VariantMap::new();
        additional.insert(
            "LocalRef".to_string(),
            owned_string(
                local
                    .as_ref()
                    .expect("upgrade has local reference")
                    .to_string(),
            ),
        );
        additional.insert("RemoteRef".to_string(), owned_string(target.to_string()));
        if !context
            .request_interaction(InteractionMessageType::Upgrade as i32, additional)
            .await
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?
        {
            return Ok(TaskCompletion::canceled("action canceled"));
        }
    }

    context
        .update_progress(10.0, "installing uab")
        .await
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let work = repository.root().join("tmp").join(format!(
        "install-uab-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(work.parent().expect("temporary UAB path has a parent"))
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let mut imported_layers = Vec::new();
    let result = async {
        uab.unpack_bundle(&work)
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        let sign_overlay = work.join(".sign-overlay");
        let overlays = if uab
            .extract_sign_data(&sign_overlay)
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?
        {
            vec![sign_overlay]
        } else {
            Vec::new()
        };
        context
            .update_progress(15.0, "checking uab dependencies")
            .await
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        if executable_mode {
            let app_info = &app_layers[0].info;
            ensure_dependency(&mut repository, &app_info.base, &app_info.channel, &context).await?;
            if let Some(runtime) = app_info.runtime.as_deref() {
                let runtime_fuzzy = dependency_fuzzy(runtime, &app_info.channel, INSTALL_FAILED)?;
                if latest_local_reference(&repository, &runtime_fuzzy)?.is_none() {
                    import_uab_layers(
                        &mut repository,
                        &work,
                        &other_layers,
                        Some(&metadata.uuid),
                        &overlays,
                        &mut imported_layers,
                    )
                    .await?;
                }
            }
            context
                .update_progress(35.0, "importing application layers")
                .await
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
            import_uab_layers(
                &mut repository,
                &work,
                &app_layers,
                None,
                &overlays,
                &mut imported_layers,
            )
            .await?;
        } else if !app_layers.is_empty() {
            let app_info = &app_layers[0].info;
            ensure_dependency(&mut repository, &app_info.base, &app_info.channel, &context).await?;
            if let Some(runtime) = app_info.runtime.as_deref() {
                ensure_dependency(&mut repository, runtime, &app_info.channel, &context).await?;
            }
            import_uab_layers(
                &mut repository,
                &work,
                &app_layers,
                None,
                &overlays,
                &mut imported_layers,
            )
            .await?;
        } else {
            import_uab_layers(
                &mut repository,
                &work,
                &other_layers,
                None,
                &overlays,
                &mut imported_layers,
            )
            .await?;
        }

        merge_modules_best_effort(&mut repository);
        execute_post_install(&repository, &target, INSTALL_FAILED)?;
        if selected_info.kind == "app" && !extra_only {
            repository
                .export_reference(&target)
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
            if let Some(installed) = &local {
                repository
                    .unexport_reference(installed)
                    .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
                try_uninstall_reference(&mut repository, installed, INSTALL_FAILED).await?;
                prune_after_change(&mut repository, !options.no_auto_prune.unwrap_or(false)).await;
            }
        }
        Ok(())
    }
    .await;
    let _ = fs::remove_dir_all(&work);
    if result.is_err() {
        let _ = repository.unexport_reference(&target);
        rollback_uab_layers(&mut repository, &imported_layers).await;
    }
    result?;
    let message = "install uab successfully";
    Ok(TaskCompletion::new(message, common_result(0, message)))
}

fn validate_uab_group(layers: &[UabLayer]) -> Result<(), OperationError> {
    let Some(first) = layers.first() else {
        return Ok(());
    };
    let host = Architecture::current()
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    for layer in layers {
        let architecture = layer
            .info
            .arch
            .first()
            .ok_or_else(|| OperationError::new(INSTALL_FAILED, "UAB layer has no architecture"))?
            .parse::<Architecture>()
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        if architecture != host {
            return Err(OperationError::new(
                INSTALL_ARCH_MISMATCH,
                format!("uab arch: {architecture} not match host architecture"),
            ));
        }
        if layer.info.id != first.info.id {
            return Err(OperationError::new(
                INSTALL_FAILED,
                "more than one layers with different id",
            ));
        }
        if layer.info.version != first.info.version {
            return Err(OperationError::new(
                INSTALL_FAILED,
                "modules have different version",
            ));
        }
    }
    Ok(())
}

fn validate_uab_extra_modules(
    repository: &LocalRepository,
    layers: &[UabLayer],
) -> Result<(), OperationError> {
    if layers.is_empty()
        || layers
            .iter()
            .any(|layer| matches!(layer.info.module.as_str(), "binary" | "runtime"))
    {
        return Ok(());
    }
    let reference = reference_from_info(&layers[0].info)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    if repository.layer_item(&reference, "binary").is_ok()
        || repository.layer_item(&reference, "runtime").is_ok()
    {
        return Ok(());
    }
    Err(OperationError::new(
        INSTALL_MODULE_REQUIRES_APP,
        "no matched binary module found",
    ))
}

fn validate_uab_layout(
    layers: &[UabLayer],
    executable_mode: bool,
) -> Result<(Vec<UabLayer>, Vec<UabLayer>), OperationError> {
    let (app_layers, other_layers): (Vec<_>, Vec<_>) = layers
        .iter()
        .cloned()
        .partition(|layer| layer.info.kind == "app");
    if executable_mode {
        let app_info = &app_layers
            .first()
            .ok_or_else(|| OperationError::new(INSTALL_FAILED, "no app layers found"))?
            .info;
        if let Some(runtime) = app_info.runtime.as_deref() {
            let bundled_runtime = other_layers
                .first()
                .ok_or_else(|| OperationError::new(INSTALL_FAILED, "runtime layer not found"))?;
            let runtime_fuzzy = runtime
                .parse::<FuzzyReference>()
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
            let runtime_reference = reference_from_info(&bundled_runtime.info)
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
            if runtime_fuzzy.id != runtime_reference.id
                || app_info.channel != runtime_reference.channel
            {
                return Err(OperationError::new(
                    INSTALL_FAILED,
                    "runtime layer not matched",
                ));
            }
            if runtime_fuzzy
                .version
                .as_ref()
                .is_some_and(|version| !runtime_reference.version.semantic_match(version))
            {
                return Err(OperationError::new(
                    INSTALL_FAILED,
                    "runtime layer version not matched",
                ));
            }
        }
    } else {
        if app_layers.is_empty() && other_layers.is_empty() {
            return Err(OperationError::new(INSTALL_FAILED, "no layers found"));
        }
        if !app_layers.is_empty() && !other_layers.is_empty() {
            return Err(OperationError::new(
                INSTALL_FAILED,
                "layers from multiple packages found",
            ));
        }
    }
    validate_uab_group(&app_layers)?;
    validate_uab_group(&other_layers)?;
    Ok((app_layers, other_layers))
}

struct ImportedUabLayer {
    reference: Reference,
    module: String,
    sub_ref: Option<String>,
}

async fn import_uab_layers(
    repository: &mut LocalRepository,
    bundle: &Path,
    layers: &[UabLayer],
    sub_ref: Option<&str>,
    overlays: &[std::path::PathBuf],
    imported_layers: &mut Vec<ImportedUabLayer>,
) -> Result<(), OperationError> {
    for layer in layers {
        let directory = bundle
            .join("layers")
            .join(&layer.info.id)
            .join(&layer.info.module);
        if !directory.is_dir() {
            return Err(OperationError::new(
                INSTALL_FAILED,
                format!("layer directory {} doesn't exist", directory.display()),
            ));
        }
        let disk_info: PackageInfoV2 = serde_json::from_slice(
            &fs::read(directory.join("info.json"))
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?,
        )
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        if disk_info != layer.info {
            return Err(OperationError::new(
                INSTALL_FAILED,
                format!(
                    "layer metadata mismatch for {}/{}",
                    layer.info.id, layer.info.module
                ),
            ));
        }
        let reference = reference_from_info(&disk_info)
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        repository
            .import_layer_dir(&directory, overlays, sub_ref)
            .await
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        imported_layers.push(ImportedUabLayer {
            reference,
            module: disk_info.module,
            sub_ref: sub_ref.map(str::to_string),
        });
    }
    Ok(())
}

async fn rollback_uab_layers(
    repository: &mut LocalRepository,
    imported_layers: &[ImportedUabLayer],
) {
    for layer in imported_layers.iter().rev() {
        if let Err(error) = repository
            .remove_layer_with_sub_ref(&layer.reference, &layer.module, layer.sub_ref.as_deref())
            .await
        {
            eprintln!(
                "warning: failed to roll back UAB layer {}/{}: {error}",
                layer.reference, layer.module
            );
        }
        if let Err(error) =
            InstallHooks::load().and_then(|hooks| hooks.post_uninstall(&layer.reference.id))
        {
            eprintln!(
                "warning: failed to run rollback uninstall hook for {}: {error:#}",
                layer.reference
            );
        }
    }
    merge_modules_best_effort(repository);
}

async fn install_layer_inner(
    repository: SharedRepository,
    mut file: File,
    options: CommonOptions,
    context: TaskContext,
) -> Result<TaskCompletion, OperationError> {
    let layer_info = read_layer_info_from(&mut file)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let package_info: PackageInfoV2 = serde_json::from_value(layer_info.info)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let architecture = package_info
        .arch
        .first()
        .ok_or_else(|| OperationError::new(INSTALL_FAILED, "layer has no architecture"))?
        .parse::<Architecture>()
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let host_architecture = Architecture::current()
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    if architecture != host_architecture {
        return Err(OperationError::new(
            INSTALL_ARCH_MISMATCH,
            format!("app arch:{architecture} not match host architecture:{host_architecture}"),
        ));
    }
    let target = reference_from_info(&package_info)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let fuzzy = FuzzyReference::new(None, &target.id, None, Some(host_architecture))
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;

    let mut repository = repository.lock().await;
    let local = latest_local_reference(&repository, &fuzzy)?;
    if let Some(installed) = &local
        && repository
            .layer_item(installed, &package_info.module)
            .is_ok()
    {
        if installed.version == target.version {
            return Err(OperationError::new(
                INSTALL_ALREADY_INSTALLED,
                format!("{installed} is already installed"),
            ));
        }
        if target.version.partial_cmp(&installed.version) == Some(Ordering::Less) && !options.force
        {
            return Err(OperationError::new(
                INSTALL_NEED_DOWNGRADE,
                "The latest version has been installed. Use --force to replace it",
            ));
        }
        if target.version.partial_cmp(&installed.version) == Some(Ordering::Greater)
            && !options.skip_interaction
        {
            let mut additional = VariantMap::new();
            additional.insert("LocalRef".to_string(), owned_string(installed.to_string()));
            additional.insert("RemoteRef".to_string(), owned_string(target.to_string()));
            let accepted = context
                .request_interaction(InteractionMessageType::Upgrade as i32, additional)
                .await
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
            if !accepted {
                return Ok(TaskCompletion::canceled("action canceled"));
            }
        }
    }

    context
        .update_progress(10.0, "installing layer")
        .await
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let work = repository.root().join("tmp").join(format!(
        "install-layer-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(work.parent().expect("temporary layer path has a parent"))
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let result = async {
        unpack_layer_file(&mut file, &work)
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        context
            .update_progress(30.0, "installing application dependencies")
            .await
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        if package_info.kind == "app"
            && matches!(package_info.module.as_str(), "binary" | "runtime")
        {
            ensure_dependency(
                &mut repository,
                &package_info.base,
                &package_info.channel,
                &context,
            )
            .await?;
            if let Some(runtime) = package_info.runtime.as_deref() {
                ensure_dependency(
                    &mut repository,
                    runtime,
                    &package_info.channel,
                    &context,
                )
                .await?;
            }
        }

        context
            .update_progress(60.0, "importing layer")
            .await
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        repository
            .import_layer_dir(&work, &[], None)
            .await
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        if !matches!(package_info.module.as_str(), "binary" | "runtime")
            || package_info.kind != "app"
        {
            return Ok(());
        }

        execute_post_install(&repository, &target, INSTALL_FAILED)?;
        let mut replaced = false;
        if let Some(installed) = &local {
            if repository.module_list(installed).contains(&package_info.module) {
                let switched = async {
                    repository
                        .export_reference(&target)
                        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
                    repository
                        .unexport_reference(installed)
                        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
                    try_uninstall_reference(&mut repository, installed, INSTALL_FAILED).await?;
                    Ok::<(), OperationError>(())
                }
                .await;
                match switched {
                    Ok(()) => {
                        replaced = true;
                        merge_modules_best_effort(&mut repository);
                    }
                    Err(error) => eprintln!(
                        "warning: failed to remove old reference {installed} after install {target}: {}",
                        error.message
                    ),
                }
            }
        } else {
            repository
                .export_reference(&target)
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        }
        if replaced {
            prune_after_change(
                &mut repository,
                !options.no_auto_prune.unwrap_or(false),
            )
            .await;
        }
        Ok(())
    }
    .await;
    let _ = fs::remove_dir_all(&work);
    result?;

    let message = "install layer successfully";
    Ok(TaskCompletion::new(message, common_result(0, message)))
}

async fn install_inner(
    repository: SharedRepository,
    parameters: PackageManagerInstallParameters,
    context: TaskContext,
) -> Result<TaskCompletion, OperationError> {
    let architecture = Architecture::current()
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let mut fuzzy = FuzzyReference::new(
        parameters.package.channel.clone(),
        &parameters.package.id,
        parameters.package.version.clone(),
        Some(architecture),
    )
    .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let mut modules = parameters
        .package
        .modules
        .clone()
        .unwrap_or_else(|| vec!["binary".to_string()]);
    modules.retain(|module| !module.is_empty());
    modules.sort();
    modules.dedup();
    if modules.is_empty() {
        return Err(OperationError::new(
            INSTALL_MODULE_NOT_FOUND,
            "no modules found",
        ));
    }
    let extra_only = modules
        .iter()
        .all(|module| !matches!(module.as_str(), "binary" | "runtime"));

    let mut repository = repository.lock().await;
    context
        .update_state_message(format!("Installing {} - Preparing...", fuzzy.id))
        .await
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let mut local = latest_local_reference(&repository, &fuzzy)?;
    if extra_only {
        let installed = local.as_ref().ok_or_else(|| {
            OperationError::new(
                INSTALL_MODULE_REQUIRES_APP,
                "no matched binary module found",
            )
        })?;
        modules.retain(|module| repository.layer_item(installed, module).is_err());
        if modules.is_empty() {
            return Err(OperationError::new(
                INSTALL_MODULE_EXISTS,
                "no modules to install",
            ));
        }
        fuzzy.version = Some(installed.version.to_string());
    } else if local.as_ref().is_some_and(|installed| {
        fuzzy.version.as_deref() == Some(installed.version.to_string().as_str())
    }) {
        return Err(OperationError::new(
            INSTALL_ALREADY_INSTALLED,
            "package already installed",
        ));
    }

    let selection = select_remote(
        repository.config(),
        &fuzzy,
        parameters.repo.as_deref(),
        INSTALL_NOT_FOUND,
    )
    .await?;
    let target = reference_from_info(&selection.package)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    if local
        .as_ref()
        .is_some_and(|installed| installed.version == target.version)
        && !extra_only
    {
        return Err(OperationError::new(
            INSTALL_ALREADY_INSTALLED,
            "package already installed",
        ));
    }
    if local.as_ref().is_some_and(|installed| {
        target.version.partial_cmp(&installed.version) == Some(Ordering::Less)
    }) && !parameters.options.force
    {
        return Err(OperationError::new(
            INSTALL_NEED_DOWNGRADE,
            "latest version already installed",
        ));
    }
    let upgrading = local.as_ref().is_some_and(|installed| {
        target.version.partial_cmp(&installed.version) == Some(Ordering::Greater)
    });
    if upgrading && !parameters.options.skip_interaction {
        let mut additional = VariantMap::new();
        additional.insert(
            "LocalRef".to_string(),
            owned_string(
                local
                    .as_ref()
                    .expect("upgrade has local reference")
                    .to_string(),
            ),
        );
        additional.insert("RemoteRef".to_string(), owned_string(target.to_string()));
        let accepted = context
            .request_interaction(InteractionMessageType::Upgrade as i32, additional)
            .await
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        if !accepted {
            return Ok(TaskCompletion::canceled("action canceled"));
        }
    }

    let remote_modules = selection.packages.reference_modules(&target);
    let mut requested_modules = modules;
    if !extra_only && let Some(installed) = &local {
        requested_modules.extend(repository.module_list(installed));
    }
    let install_modules = available_install_modules(&requested_modules, &remote_modules);
    if install_modules.is_empty() {
        return Err(OperationError::new(
            INSTALL_MODULE_NOT_FOUND,
            "no modules found",
        ));
    }
    let mut planned = Vec::new();
    for (index, module) in install_modules.iter().enumerate() {
        let metadata = repository
            .fetch_remote_metadata(&target, &selection.repo, module, index == 0)
            .await
            .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
        planned.push(PlannedRemoteModule {
            reference: target.clone(),
            repo: selection.repo.clone(),
            module: module.clone(),
            metadata,
        });
    }
    let package_info = planned
        .first()
        .and_then(|plan| plan.metadata.package_info.clone())
        .ok_or_else(|| OperationError::new(INSTALL_FAILED, "commit does not contain info.json"))?;

    if package_info.kind == "app" {
        gather_missing_install_dependency(
            &repository,
            &package_info.base,
            &package_info.channel,
            &mut planned,
        )
        .await?;
        if let Some(runtime) = package_info.runtime.as_deref() {
            gather_missing_install_dependency(
                &repository,
                runtime,
                &package_info.channel,
                &mut planned,
            )
            .await?;
        }
    }

    let mut download = DownloadProgress::default();
    for plan in &planned {
        match repository.get_ref_statistics(&plan.metadata).await {
            Ok(statistics) => {
                download.total_size = download.total_size.saturating_add(statistics.archived);
                download.needed_size = download
                    .needed_size
                    .saturating_add(statistics.needed_archived);
            }
            Err(error) => eprintln!(
                "warning: failed to get statistics for {}/{}: {error}",
                plan.reference, plan.module
            ),
        }
    }

    let install_result = async {
        for plan in &planned {
            let task_message = format!("Installing {}/{}", plan.reference, plan.module);
            context
                .update_state_message(&task_message)
                .await
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
            install_remote_module(
                &mut repository,
                &plan.reference,
                &plan.repo,
                &plan.module,
                INSTALL_FAILED,
                &context,
                &mut download,
                &task_message,
            )
            .await?;
        }

        if package_info.kind == "app" && !extra_only {
            repository
                .export_reference(&target)
                .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
            if let Some(installed) = local.take() {
                repository
                    .unexport_reference(&installed)
                    .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
                try_uninstall_reference(&mut repository, &installed, INSTALL_FAILED).await?;
                prune_after_change(
                    &mut repository,
                    !parameters.options.no_auto_prune.unwrap_or(false),
                )
                .await;
            }
        }
        Ok(())
    }
    .await;
    if let Err(error) = install_result {
        let _ = repository.unexport_reference(&target);
        let _ = try_uninstall_reference(&mut repository, &target, INSTALL_FAILED).await;
        return Err(error);
    }
    merge_modules_best_effort(&mut repository);
    let message = format!(
        "Install {} (from repo: {}) success",
        target,
        selection.repo.effective_name()
    );
    Ok(TaskCompletion::new(&message, common_result(0, &message)))
}

pub async fn uninstall(
    repository: SharedRepository,
    parameters: PackageManagerUninstallParameters,
    context: TaskContext,
) -> Result<TaskCompletion, String> {
    match uninstall_inner(repository, parameters, context).await {
        Ok(completion) => Ok(completion),
        Err(error) => Ok(TaskCompletion::failed(error.code, error.message)),
    }
}

async fn uninstall_inner(
    repository: SharedRepository,
    parameters: PackageManagerUninstallParameters,
    context: TaskContext,
) -> Result<TaskCompletion, OperationError> {
    let mut repository = repository.lock().await;
    let candidates = matching_main_layers(&repository, &parameters.package)?;
    if candidates.is_empty() {
        return Err(OperationError::new(
            UNINSTALL_NOT_FOUND,
            "the package is not installed",
        ));
    }
    if candidates.len() > 1 {
        let references = candidates
            .iter()
            .map(|(reference, _)| reference.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(OperationError::new(UNINSTALL_MULTIPLE_VERSIONS, references));
    }
    let (reference, info) = candidates.into_iter().next().expect("candidate is present");
    if matches!(info.kind.as_str(), "base" | "runtime") && !parameters.options.force {
        return Err(OperationError::new(
            UNINSTALL_BASE_OR_RUNTIME,
            "base or runtime package cannot be uninstalled",
        ));
    }
    if reference_is_busy(&reference)? {
        return Err(OperationError::new(UNINSTALL_RUNNING, "ref is busy"));
    }
    let module = parameters
        .package
        .module
        .as_deref()
        .unwrap_or("binary")
        .to_string();
    let may_have_unused_dependencies =
        info.kind == "app" && matches!(module.as_str(), "binary" | "runtime");
    context
        .update_state_message(format!("Uninstalling {reference}"))
        .await
        .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?;
    if matches!(module.as_str(), "binary" | "runtime") {
        if info.kind == "app" {
            repository
                .unexport_reference(&reference)
                .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?;
        }
        remove_all_modules(&mut repository, &reference, UNINSTALL_FAILED).await?;
    } else {
        if !repository
            .remove_layer(&reference, &module)
            .await
            .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?
        {
            return Err(OperationError::new(
                UNINSTALL_NOT_FOUND,
                "the package module is not installed",
            ));
        }
        execute_post_uninstall(&reference);
    }
    prune_after_change(
        &mut repository,
        may_have_unused_dependencies && !parameters.options.no_auto_prune.unwrap_or(false),
    )
    .await;
    merge_modules_best_effort(&mut repository);
    let message = format!("Uninstall {reference} {module} success");
    Ok(TaskCompletion::new(&message, common_result(0, &message)))
}

pub async fn update(
    repository: SharedRepository,
    parameters: PackageManagerUpdateParameters,
    context: TaskContext,
) -> Result<TaskCompletion, String> {
    match update_inner(repository, parameters, context).await {
        Ok(completion) => Ok(completion),
        Err(error) => Ok(TaskCompletion::failed(error.code, error.message)),
    }
}

pub async fn prune(repository: SharedRepository) -> Result<Vec<PackageInfoV2>, String> {
    let mut repository = repository.lock().await;
    prune_unused(&mut repository)
        .await
        .map_err(|error| error.message)
}

#[derive(Debug)]
struct PruneTarget {
    info: PackageInfoV2,
    reference: Reference,
    references: usize,
}

async fn prune_unused(
    repository: &mut LocalRepository,
) -> Result<Vec<PackageInfoV2>, OperationError> {
    let main_layers = repository
        .list_layer_items()
        .into_iter()
        .filter(|item| matches!(item.info.module.as_str(), "binary" | "runtime"))
        .collect::<Vec<_>>();
    let mut targets = BTreeMap::<String, PruneTarget>::new();
    for item in &main_layers {
        let reference = reference_from_info(&item.info)
            .map_err(|error| OperationError::new(-1, error.to_string()))?;
        targets
            .entry(reference.to_string())
            .or_insert_with(|| PruneTarget {
                info: item.info.clone(),
                reference,
                references: usize::from(item.info.kind == "app"),
            });
    }

    for item in main_layers.iter().filter(|item| item.info.kind == "app") {
        if let Some(runtime) = item.info.runtime.as_deref()
            && let Some(reference) =
                resolve_prune_dependency(repository, runtime, &item.info.channel)
        {
            touch_prune_target(repository, &mut targets, &reference);
            scan_prune_extensions(repository, &mut targets, &reference, &item.info.channel);
        }
        if !item.info.base.is_empty()
            && let Some(reference) =
                resolve_prune_dependency(repository, &item.info.base, &item.info.channel)
        {
            touch_prune_target(repository, &mut targets, &reference);
            scan_prune_extensions(repository, &mut targets, &reference, &item.info.channel);
        }
        scan_extension_values(
            repository,
            &mut targets,
            item.info.extensions.as_deref(),
            &item.info.channel,
        );
    }

    let mut removed = Vec::new();
    let mut reserved = Vec::<RepositoryCacheLayersItem>::new();
    for target in targets.into_values() {
        if target.references == 0 {
            remove_all_modules(repository, &target.reference, -1).await?;
            removed.push(target.info);
            continue;
        }
        for module in repository.module_list(&target.reference) {
            match repository.layer_item(&target.reference, &module) {
                Ok(item) => reserved.push(item),
                Err(error) => eprintln!(
                    "warning: failed to reserve {}/{} while pruning: {error}",
                    target.reference, module
                ),
            }
        }
    }
    merge_modules_best_effort(repository);
    repository
        .clean(&reserved)
        .await
        .map_err(|error| OperationError::new(-1, error.to_string()))?;
    removed.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.version.cmp(&right.version))
    });
    Ok(removed)
}

async fn prune_after_change(repository: &mut LocalRepository, dependency_aware: bool) {
    let result = if dependency_aware {
        prune_unused(repository)
            .await
            .map(|_| ())
            .map_err(|error| error.message)
    } else {
        repository
            .prune_objects()
            .await
            .map_err(|error| error.to_string())
    };
    if let Err(error) = result {
        eprintln!("warning: failed to prune repository: {error}");
    }
}

fn merge_modules_best_effort(repository: &mut LocalRepository) {
    if let Err(error) = repository.merge_modules() {
        eprintln!("warning: failed to merge modules: {error}");
    }
}

fn resolve_prune_dependency(
    repository: &LocalRepository,
    raw: &str,
    channel: &str,
) -> Option<Reference> {
    let mut fuzzy = raw.parse::<FuzzyReference>().ok()?;
    if fuzzy.channel.is_none() && !channel.is_empty() {
        fuzzy.channel = Some(channel.to_string());
    }
    if fuzzy.architecture.is_none() {
        fuzzy.architecture = Architecture::current().ok();
    }
    repository.resolve_local(&fuzzy, true).ok()
}

fn touch_prune_target(
    repository: &LocalRepository,
    targets: &mut BTreeMap<String, PruneTarget>,
    reference: &Reference,
) {
    let key = reference.to_string();
    if let Some(target) = targets.get_mut(&key) {
        target.references += 1;
        return;
    }
    if let Ok(item) = repository.layer_item(reference, "binary") {
        targets.insert(
            key,
            PruneTarget {
                info: item.info,
                reference: reference.clone(),
                references: 1,
            },
        );
    }
}

fn scan_prune_extensions(
    repository: &LocalRepository,
    targets: &mut BTreeMap<String, PruneTarget>,
    reference: &Reference,
    channel: &str,
) {
    if let Ok(item) = repository.layer_item(reference, "binary") {
        scan_extension_values(
            repository,
            targets,
            item.info.extensions.as_deref(),
            channel,
        );
    }
}

fn scan_extension_values(
    repository: &LocalRepository,
    targets: &mut BTreeMap<String, PruneTarget>,
    extensions: Option<&[ExtensionDefine]>,
    channel: &str,
) {
    for extension in extensions.into_iter().flatten() {
        let Some(name) = enabled_extension_name(&extension.name) else {
            continue;
        };
        let version = &extension.version;
        let Ok(fuzzy) = FuzzyReference::new(
            (!channel.is_empty()).then(|| channel.to_string()),
            name,
            (!version.is_empty()).then(|| version.to_string()),
            Architecture::current().ok(),
        ) else {
            continue;
        };
        if let Ok(reference) = repository.resolve_local(&fuzzy, true) {
            touch_prune_target(repository, targets, &reference);
        }
    }
}

fn enabled_extension_name(name: &str) -> Option<String> {
    if name != "org.deepin.driver.display.nvidia" {
        return Some(name.to_string());
    }
    let driver_version = fs::read_to_string("/sys/module/nvidia/version")
        .ok()?
        .trim()
        .replace('.', "-");
    if driver_version.is_empty() {
        return None;
    }
    Some(format!("{name}.{driver_version}"))
}

async fn update_inner(
    repository: SharedRepository,
    parameters: PackageManagerUpdateParameters,
    context: TaskContext,
) -> Result<TaskCompletion, OperationError> {
    let mut repository = repository.lock().await;
    let installed_apps = installed_apps(&repository)?;
    let apps = if parameters.packages.is_empty() {
        installed_apps
    } else {
        installed_apps
            .into_iter()
            .filter(|(reference, _)| {
                parameters.packages.iter().any(|requested| {
                    requested.id == reference.id
                        && requested
                            .channel
                            .as_ref()
                            .is_none_or(|channel| channel == &reference.channel)
                })
            })
            .collect()
    };
    if apps.is_empty() {
        return Err(OperationError::new(
            UPDATE_LOCAL_NOT_FOUND,
            "No apps to upgrade",
        ));
    }
    context
        .update_state_message("Updating applications")
        .await
        .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
    let mut succeeded = 0usize;
    let mut repository_changed = false;
    for (reference, info) in apps {
        if context.is_canceled() {
            return Ok(TaskCompletion::canceled("task was cancelled"));
        }
        match update_app(
            &mut repository,
            &reference,
            &info,
            parameters.deps_only,
            &context,
        )
        .await
        {
            Ok(changed) => {
                succeeded += 1;
                repository_changed |= changed;
            }
            Err(error) => {
                context
                    .send_message(format!(
                        "failed to update app {}: {}",
                        reference.id, error.message
                    ))
                    .await
                    .map_err(|bus| OperationError::new(UPDATE_FAILED, bus.to_string()))?;
            }
        }
    }
    if succeeded == 0 {
        return Err(OperationError::new(
            UPDATE_FAILED,
            "all apps failed to upgrade",
        ));
    }
    merge_modules_best_effort(&mut repository);
    if repository_changed {
        prune_after_change(&mut repository, !parameters.no_auto_prune.unwrap_or(false)).await;
    }
    let message = "Update applications success";
    Ok(TaskCompletion::new(message, common_result(0, message)))
}

async fn update_app(
    repository: &mut LocalRepository,
    local: &Reference,
    old_info: &PackageInfoV2,
    dependencies_only: bool,
    context: &TaskContext,
) -> Result<bool, OperationError> {
    let checking_message = format!("Checking for updates {}", local.id);
    context
        .reset_progress(&checking_message)
        .await
        .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
    let mut planned = Vec::new();
    let mut new_reference = None;
    let mut app_info = old_info.clone();
    if !dependencies_only {
        let fuzzy = FuzzyReference::new(
            Some(local.channel.clone()),
            &local.id,
            None,
            Some(local.architecture),
        )
        .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
        let gathered =
            gather_reference_update(repository, &fuzzy, Some(local.clone()), true, &mut planned)
                .await?;
        if let Some(info) = gathered.package_info {
            app_info = info;
        }
        new_reference = gathered.remote;
    }

    gather_update_dependency(
        repository,
        &app_info.base,
        &app_info.channel,
        false,
        &mut planned,
    )
    .await?;
    if let Some(runtime) = app_info.runtime.as_deref() {
        gather_update_dependency(repository, runtime, &app_info.channel, false, &mut planned)
            .await?;
    }
    gather_update_extensions(
        repository,
        app_info.extensions.as_deref(),
        &app_info.channel,
        &mut planned,
    )
    .await?;

    let mut download = DownloadProgress::default();
    for plan in &planned {
        match repository.get_ref_statistics(&plan.metadata).await {
            Ok(statistics) => {
                download.total_size = download.total_size.saturating_add(statistics.archived);
                download.needed_size = download
                    .needed_size
                    .saturating_add(statistics.needed_archived);
            }
            Err(error) => eprintln!(
                "warning: failed to get statistics for {}/{}: {error}",
                plan.reference, plan.module
            ),
        }
    }

    let result = async {
        for plan in &planned {
            let task_message = format!("Updating {}/{}", plan.reference, plan.module);
            context
                .update_state_message(&task_message)
                .await
                .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
            install_remote_module(
                repository,
                &plan.reference,
                &plan.repo,
                &plan.module,
                UPDATE_FAILED,
                context,
                &mut download,
                &task_message,
            )
            .await?;
        }

        if let Some(remote) = &new_reference {
            repository
                .export_reference(remote)
                .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
            repository
                .unexport_reference(local)
                .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
            try_uninstall_reference(repository, local, UPDATE_FAILED).await?;
        }
        Ok(!planned.is_empty())
    }
    .await;

    if result.is_err()
        && let Some(remote) = &new_reference
    {
        let _ = repository.unexport_reference(remote);
        let _ = try_uninstall_reference(repository, remote, UPDATE_FAILED).await;
    }
    result
}

async fn gather_missing_install_dependency(
    repository: &LocalRepository,
    raw: &str,
    channel: &str,
    planned: &mut Vec<PlannedRemoteModule>,
) -> Result<(), OperationError> {
    if raw.is_empty() {
        return Ok(());
    }
    let fuzzy = dependency_fuzzy(raw, channel, INSTALL_FAILED)?;
    if latest_local_reference(repository, &fuzzy)?.is_some() {
        return Ok(());
    }
    let selection = select_remote(repository.config(), &fuzzy, None, INSTALL_NOT_FOUND).await?;
    let reference = reference_from_info(&selection.package)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let metadata = repository
        .fetch_remote_metadata(&reference, &selection.repo, "binary", false)
        .await
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    planned.push(PlannedRemoteModule {
        reference,
        repo: selection.repo,
        module: "binary".to_string(),
        metadata,
    });
    Ok(())
}

async fn ensure_dependency(
    repository: &mut LocalRepository,
    raw: &str,
    channel: &str,
    context: &TaskContext,
) -> Result<(), OperationError> {
    if raw.is_empty() {
        return Ok(());
    }
    let fuzzy = dependency_fuzzy(raw, channel, INSTALL_FAILED)?;
    if latest_local_reference(repository, &fuzzy)?.is_some() {
        return Ok(());
    }
    let selection = select_remote(repository.config(), &fuzzy, None, INSTALL_NOT_FOUND).await?;
    let reference = reference_from_info(&selection.package)
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let metadata = repository
        .fetch_remote_metadata(&reference, &selection.repo, "binary", false)
        .await
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    let mut download = DownloadProgress::default();
    match repository.get_ref_statistics(&metadata).await {
        Ok(statistics) => {
            download.total_size = statistics.archived;
            download.needed_size = statistics.needed_archived;
        }
        Err(error) => {
            eprintln!("warning: failed to get statistics for {reference}/binary: {error}")
        }
    }
    let task_message = format!("Installing {reference}/binary");
    context
        .update_state_message(&task_message)
        .await
        .map_err(|error| OperationError::new(INSTALL_FAILED, error.to_string()))?;
    install_remote_module(
        repository,
        &reference,
        &selection.repo,
        "binary",
        INSTALL_FAILED,
        context,
        &mut download,
        &task_message,
    )
    .await
}

async fn gather_reference_update(
    repository: &LocalRepository,
    fuzzy: &FuzzyReference,
    local: Option<Reference>,
    install_if_missing: bool,
    planned: &mut Vec<PlannedRemoteModule>,
) -> Result<GatheredUpdate, OperationError> {
    let local = match local {
        Some(local) => Some(local),
        None => latest_local_reference(repository, fuzzy)?,
    };
    if local.is_none() && !install_if_missing {
        return Ok(GatheredUpdate::default());
    }

    let selection = select_remote(repository.config(), fuzzy, None, UPDATE_FAILED).await?;
    let remote = reference_from_info(&selection.package)
        .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
    if let Some(installed) = &local
        && remote.version <= installed.version
    {
        let info = repository
            .layer_item(installed, "binary")
            .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?
            .info;
        return Ok(GatheredUpdate {
            remote: None,
            package_info: Some(info),
        });
    }

    let requested_modules = local
        .as_ref()
        .map(|installed| repository.module_list(installed))
        .unwrap_or_else(|| vec!["binary".to_string()]);
    let remote_modules = selection.packages.reference_modules(&remote);
    let modules = if local.is_some() {
        available_update_modules(&requested_modules, &remote_modules)
    } else {
        available_install_modules(&requested_modules, &remote_modules)
    };
    if modules.is_empty() {
        return Err(OperationError::new(
            UPDATE_FAILED,
            format!(
                "no modules found to upgrade {}",
                local
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| fuzzy.to_string())
            ),
        ));
    }

    let first_plan = planned.len();
    for (index, module) in modules.into_iter().enumerate() {
        let metadata = repository
            .fetch_remote_metadata(&remote, &selection.repo, &module, index == 0)
            .await
            .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
        planned.push(PlannedRemoteModule {
            reference: remote.clone(),
            repo: selection.repo.clone(),
            module,
            metadata,
        });
    }
    let package_info = planned[first_plan]
        .metadata
        .package_info
        .clone()
        .ok_or_else(|| OperationError::new(UPDATE_FAILED, "commit does not contain info.json"))?;
    Ok(GatheredUpdate {
        remote: Some(remote),
        package_info: Some(package_info),
    })
}

async fn gather_update_dependency(
    repository: &LocalRepository,
    raw: &str,
    channel: &str,
    is_extension: bool,
    planned: &mut Vec<PlannedRemoteModule>,
) -> Result<(), OperationError> {
    if raw.is_empty() {
        return Ok(());
    }
    let fuzzy = dependency_fuzzy(raw, channel, UPDATE_FAILED)?;
    let gathered =
        gather_reference_update(repository, &fuzzy, None, !is_extension, planned).await?;
    if !is_extension && let Some(info) = gathered.package_info {
        gather_update_extensions(
            repository,
            info.extensions.as_deref(),
            &info.channel,
            planned,
        )
        .await?;
    }
    Ok(())
}

async fn gather_update_extensions(
    repository: &LocalRepository,
    extensions: Option<&[ExtensionDefine]>,
    channel: &str,
    planned: &mut Vec<PlannedRemoteModule>,
) -> Result<(), OperationError> {
    for extension in extensions.into_iter().flatten() {
        let Some(name) = enabled_extension_name(&extension.name) else {
            continue;
        };
        let fuzzy = dependency_fuzzy(
            &format!("{name}/{}", extension.version),
            channel,
            UPDATE_FAILED,
        )?;
        gather_reference_update(repository, &fuzzy, None, false, planned).await?;
    }
    Ok(())
}

fn dependency_fuzzy(
    raw: &str,
    channel: &str,
    error_code: i64,
) -> Result<FuzzyReference, OperationError> {
    let mut fuzzy = raw
        .parse::<FuzzyReference>()
        .map_err(|error| OperationError::new(error_code, error.to_string()))?;
    if fuzzy.channel.is_none() && !channel.is_empty() {
        fuzzy.channel = Some(channel.to_string());
    }
    if fuzzy.architecture.is_none() {
        fuzzy.architecture = Some(
            Architecture::current()
                .map_err(|error| OperationError::new(error_code, error.to_string()))?,
        );
    }
    Ok(fuzzy)
}

async fn select_remote(
    config: &RepoConfigV2,
    fuzzy: &FuzzyReference,
    selected_alias: Option<&str>,
    not_found_code: i64,
) -> Result<RemoteSelection, OperationError> {
    let groups = if let Some(alias) = selected_alias {
        vec![vec![
            config
                .repos
                .iter()
                .find(|repo| repo.effective_name() == alias)
                .cloned()
                .ok_or_else(|| {
                    OperationError::new(INSTALL_FAILED, format!("repo {alias} not found"))
                })?,
        ]]
    } else {
        priority_grouped_repos(config)
    };
    let mut any_request_succeeded = false;
    let mut errors = Vec::new();
    let mut packages = RemotePackages::default();
    for group in groups {
        for repo in group {
            let client = match RemoteRepositoryClient::new(&repo.url) {
                Ok(client) => client,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            match client.search_packages(fuzzy, &repo, true).await {
                Ok(found) => {
                    any_request_succeeded = true;
                    if !found.is_empty() {
                        packages.add_packages(repo, found);
                    }
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        if !packages.is_empty() {
            break;
        }
    }
    if packages.is_empty() {
        if !any_request_succeeded && !errors.is_empty() {
            return Err(OperationError::new(
                NETWORK_ERROR,
                format!("failed to search remote packages: {}", errors.join("; ")),
            ));
        }
        return Err(OperationError::new(not_found_code, "package not found"));
    }
    let (repo, package) = packages
        .latest_package()
        .map_err(|error| OperationError::new(not_found_code, error.to_string()))?;
    Ok(RemoteSelection {
        repo,
        package,
        packages,
    })
}

fn latest_local_reference(
    repository: &LocalRepository,
    fuzzy: &FuzzyReference,
) -> Result<Option<Reference>, OperationError> {
    let mut references = repository
        .list_layer_items()
        .into_iter()
        .filter(|item| matches!(item.info.module.as_str(), "binary" | "runtime"))
        .filter(|item| item.info.id == fuzzy.id)
        .filter(|item| {
            fuzzy
                .channel
                .as_ref()
                .is_none_or(|channel| channel == &item.info.channel)
        })
        .filter(|item| {
            fuzzy.architecture.is_none_or(|architecture| {
                item.info.arch.first() == Some(&architecture.to_string())
            })
        })
        .filter_map(|item| reference_from_info(&item.info).ok())
        .filter(|reference| {
            fuzzy
                .version
                .as_ref()
                .is_none_or(|version| reference.version.semantic_match(version))
        })
        .collect::<Vec<_>>();
    references.sort_by(|left, right| {
        right
            .version
            .partial_cmp(&left.version)
            .unwrap_or(Ordering::Equal)
    });
    references.dedup_by(|left, right| left.to_string() == right.to_string());
    Ok(references.into_iter().next())
}

fn available_install_modules(requested: &[String], remote: &[String]) -> Vec<String> {
    let mut modules = Vec::new();
    for module in requested {
        let selected = if remote.contains(module) {
            Some(module.clone())
        } else if module == "binary" && remote.iter().any(|remote| remote == "runtime") {
            Some("runtime".to_string())
        } else {
            None
        };
        if let Some(selected) = selected
            && !modules.contains(&selected)
        {
            modules.push(selected);
        }
    }
    modules
}

fn available_update_modules(installed: &[String], remote: &[String]) -> Vec<String> {
    let mut modules = Vec::new();
    for module in installed {
        let selected = if remote.contains(module) {
            Some(module.clone())
        } else if module == "runtime" && remote.iter().any(|remote| remote == "binary") {
            Some("binary".to_string())
        } else {
            None
        };
        if let Some(selected) = selected
            && !modules.contains(&selected)
        {
            modules.push(selected);
        }
    }
    modules
}

#[allow(clippy::too_many_arguments)]
async fn install_remote_module(
    repository: &mut LocalRepository,
    reference: &Reference,
    remote: &Repo,
    module: &str,
    error_code: i64,
    context: &TaskContext,
    download: &mut DownloadProgress,
    task_message: &str,
) -> Result<(), OperationError> {
    if repository
        .restore_deleted_layer(reference, module)
        .map_err(|error| OperationError::new(error_code, error.to_string()))?
    {
        return Ok(());
    }
    let (sender, receiver) = async_channel::unbounded();
    let pull = async {
        let progress_sender = sender.clone();
        let result = repository
            .pull_with_progress(reference, remote, module, move |bytes| {
                let _ = progress_sender.try_send(bytes);
            })
            .await;
        sender.close();
        result
    };
    let report = async {
        while let Ok(bytes) = receiver.recv().await {
            download.fetched_size = download.fetched_size.saturating_add(bytes);
            if download.total_size > 0 && download.needed_size > 0 {
                let progress =
                    (download.fetched_size as f64 * 100.0 / download.needed_size as f64).min(100.0);
                context
                    .update_progress(progress, task_message)
                    .await
                    .map_err(|error| OperationError::new(error_code, error.to_string()))?;
            }
        }
        Ok::<(), OperationError>(())
    };
    let (pulled, reported) = futures_lite::future::zip(pull, report).await;
    reported?;
    pulled.map_err(|error| OperationError::new(error_code, error.to_string()))?;
    if let Err(error) = execute_post_install(repository, reference, error_code) {
        eprintln!(
            "warning: failed to execute post-install hook for {reference}: {}",
            error.message
        );
    }
    Ok(())
}

async fn try_uninstall_reference(
    repository: &mut LocalRepository,
    reference: &Reference,
    error_code: i64,
) -> Result<bool, OperationError> {
    let busy = reference_is_busy(reference)
        .map_err(|error| OperationError::new(error_code, error.message))?;
    if !busy {
        remove_all_modules(repository, reference, error_code).await?;
        return Ok(true);
    }

    let mut marked = Vec::new();
    for module in repository.module_list(reference) {
        match repository.mark_layer_deleted(reference, &module) {
            Ok(true) => marked.push(module),
            Ok(false) => {}
            Err(error) => {
                for module in marked {
                    let _ = repository.restore_deleted_layer(reference, &module);
                }
                return Err(OperationError::new(error_code, error.to_string()));
            }
        }
    }
    Ok(false)
}

pub async fn deferred_uninstall(repository: SharedRepository) -> Result<usize, String> {
    let _repo_lock = RepoLock::try_exclusive()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "failed to lock repository: Resource temporarily unavailable".to_string())?;
    let mut repository = repository.lock().await;
    let mut groups = BTreeMap::<String, (Reference, Vec<_>)>::new();
    for item in repository.list_deleted_layer_items() {
        let reference = match reference_from_info(&item.info) {
            Ok(reference) => reference,
            Err(error) => {
                eprintln!("warning: invalid deferred layer {}: {error}", item.commit);
                continue;
            }
        };
        groups
            .entry(reference.to_string())
            .or_insert_with(|| (reference, Vec::new()))
            .1
            .push(item);
    }

    let mut removed = 0usize;
    for (_, (reference, items)) in groups {
        if reference_is_busy_unlocked(&reference).map_err(|error| error.message)? {
            continue;
        }
        for item in items {
            match repository.remove_layer_item(&item).await {
                Ok(true) => removed += 1,
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "warning: failed to remove deferred layer {}: {error}",
                        item.commit
                    );
                }
            }
        }
    }
    if removed != 0 {
        if let Err(error) = repository.merge_modules() {
            eprintln!("warning: failed to merge modules after deferred uninstall: {error}");
        }
        if let Err(error) = repository.prune_objects().await {
            eprintln!("warning: failed to prune after deferred uninstall: {error}");
        }
    }
    Ok(removed)
}

async fn remove_all_modules(
    repository: &mut LocalRepository,
    reference: &Reference,
    _error_code: i64,
) -> Result<(), OperationError> {
    let modules = repository.module_list(reference);
    for module in modules {
        match repository.remove_layer(reference, &module).await {
            Ok(true) => execute_post_uninstall(reference),
            Ok(false) => eprintln!("warning: failed to uninstall {reference}/{module}: not found"),
            Err(error) => {
                eprintln!("warning: failed to uninstall {reference}/{module}: {error}")
            }
        }
    }
    Ok(())
}

fn execute_post_uninstall(reference: &Reference) {
    if let Err(error) = InstallHooks::load().and_then(|hooks| hooks.post_uninstall(&reference.id)) {
        eprintln!("warning: failed to execute post-uninstall hook for {reference}: {error:#}");
    }
}

fn execute_post_install(
    repository: &LocalRepository,
    reference: &Reference,
    error_code: i64,
) -> Result<(), OperationError> {
    let module = if repository.layer_item(reference, "binary").is_ok() {
        "binary".to_string()
    } else if repository.layer_item(reference, "runtime").is_ok() {
        "runtime".to_string()
    } else {
        repository
            .module_list(reference)
            .into_iter()
            .next()
            .ok_or_else(|| {
                OperationError::new(
                    error_code,
                    format!("installed layer not found: {reference}"),
                )
            })?
    };
    let path = repository
        .layer_path(reference, &module)
        .map_err(|error| OperationError::new(error_code, error.to_string()))?;
    InstallHooks::load()
        .and_then(|hooks| hooks.post_install(&reference.id, &path))
        .map_err(|error| {
            OperationError::new(error_code, format!("post-install hook failed: {error:#}"))
        })
}

fn matching_main_layers(
    repository: &LocalRepository,
    package: &PackageManagerPackage,
) -> Result<Vec<(Reference, PackageInfoV2)>, OperationError> {
    repository
        .list_layer_items()
        .into_iter()
        .filter(|item| matches!(item.info.module.as_str(), "binary" | "runtime"))
        .filter(|item| item.info.id == package.id)
        .filter(|item| {
            package
                .channel
                .as_ref()
                .is_none_or(|channel| channel == &item.info.channel)
        })
        .filter(|item| {
            package
                .version
                .as_ref()
                .is_none_or(|version| version == &item.info.version)
        })
        .map(|item| {
            reference_from_info(&item.info)
                .map(|reference| (reference, item.info))
                .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))
        })
        .collect()
}

fn installed_apps(
    repository: &LocalRepository,
) -> Result<Vec<(Reference, PackageInfoV2)>, OperationError> {
    let mut seen = BTreeSet::new();
    let mut apps = Vec::new();
    for item in repository.list_layer_items() {
        if item.info.kind != "app" || !matches!(item.info.module.as_str(), "binary" | "runtime") {
            continue;
        }
        let reference = reference_from_info(&item.info)
            .map_err(|error| OperationError::new(UPDATE_FAILED, error.to_string()))?;
        if seen.insert(reference.to_string()) {
            apps.push((reference, item.info));
        }
    }
    Ok(apps)
}

fn reference_is_busy(reference: &Reference) -> Result<bool, OperationError> {
    let _repo_lock = RepoLock::try_exclusive()
        .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?
        .ok_or_else(|| {
            OperationError::new(
                UNINSTALL_FAILED,
                "failed to lock repository: Resource temporarily unavailable",
            )
        })?;
    reference_is_busy_unlocked(reference)
}

fn reference_is_busy_unlocked(reference: &Reference) -> Result<bool, OperationError> {
    let root = linyaps_core::runtime_paths::process_state_base();
    let users = match fs::read_dir(&root) {
        Ok(users) => users,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(OperationError::new(UNINSTALL_FAILED, error.to_string())),
    };
    let expected = reference.to_string();
    for user in users {
        let user =
            user.map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?;
        if !user
            .file_type()
            .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?
            .is_dir()
        {
            continue;
        }
        for state in fs::read_dir(user.path())
            .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?
        {
            let state =
                state.map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?;
            if !state
                .file_type()
                .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?
                .is_file()
                || !Path::new("/proc").join(state.file_name()).exists()
            {
                continue;
            }
            let info = serde_json::from_slice::<ContainerProcessStateInfo>(
                &fs::read(state.path())
                    .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?,
            )
            .map_err(|error| OperationError::new(UNINSTALL_FAILED, error.to_string()))?;
            if info.app == expected
                || info.base == expected
                || info.runtime.as_deref() == Some(&expected)
                || info
                    .extensions
                    .as_ref()
                    .is_some_and(|extensions| extensions.contains(&expected))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
pub(crate) static TEST_REPO_LOCK_ENV_MUTEX: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use tempfile::tempdir;

    use super::*;

    struct ProcessStateEnv {
        process_state: Option<OsString>,
        repo_lock: Option<OsString>,
    }

    impl ProcessStateEnv {
        fn set(path: &Path, lock_path: &Path) -> Self {
            let process_state = std::env::var_os("LINGLONG_PROCESS_STATE_ROOT");
            let repo_lock = std::env::var_os(linyaps_core::repo_lock::REPO_LOCK_ENV);
            unsafe { std::env::set_var("LINGLONG_PROCESS_STATE_ROOT", path) };
            unsafe { std::env::set_var(linyaps_core::repo_lock::REPO_LOCK_ENV, lock_path) };
            Self {
                process_state,
                repo_lock,
            }
        }
    }

    impl Drop for ProcessStateEnv {
        fn drop(&mut self) {
            if let Some(previous) = &self.process_state {
                unsafe { std::env::set_var("LINGLONG_PROCESS_STATE_ROOT", previous) };
            } else {
                unsafe { std::env::remove_var("LINGLONG_PROCESS_STATE_ROOT") };
            }
            if let Some(previous) = &self.repo_lock {
                unsafe { std::env::set_var(linyaps_core::repo_lock::REPO_LOCK_ENV, previous) };
            } else {
                unsafe { std::env::remove_var(linyaps_core::repo_lock::REPO_LOCK_ENV) };
            }
        }
    }

    fn test_config() -> RepoConfigV2 {
        RepoConfigV2 {
            default_repo: "stable".to_string(),
            repos: vec![Repo {
                alias: None,
                mirror_enabled: None,
                name: "stable".to_string(),
                priority: 0,
                url: "https://example.invalid/repo".to_string(),
            }],
            version: 2,
        }
    }

    fn test_info(id: &str, kind: &str, version: &str, module: &str) -> PackageInfoV2 {
        PackageInfoV2 {
            arch: vec![Architecture::current().unwrap().to_string()],
            base: String::new(),
            channel: "main".to_string(),
            command: None,
            compatible_version: None,
            description: None,
            extension_implementation: None,
            extensions: None,
            id: id.to_string(),
            kind: kind.to_string(),
            module: module.to_string(),
            name: id.to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 0,
            uuid: None,
            version: version.to_string(),
        }
    }

    fn uab_layer(id: &str, kind: &str, version: &str, module: &str) -> UabLayer {
        UabLayer {
            info: test_info(id, kind, version, module),
            minified: false,
        }
    }

    fn write_layer(directory: &Path, info: &PackageInfoV2, payload: &str) {
        fs::create_dir_all(directory).unwrap();
        fs::write(
            directory.join("info.json"),
            serde_json::to_vec(info).unwrap(),
        )
        .unwrap();
        fs::write(directory.join("payload"), payload).unwrap();
    }

    #[test]
    fn install_module_fallback_matches_upstream() {
        assert_eq!(
            available_install_modules(
                &["binary".to_string(), "develop".to_string()],
                &["runtime".to_string(), "develop".to_string()]
            ),
            vec!["runtime", "develop"]
        );
    }

    #[test]
    fn update_module_fallback_is_the_reverse_direction() {
        assert_eq!(
            available_update_modules(
                &["runtime".to_string(), "develop".to_string()],
                &["binary".to_string()]
            ),
            vec!["binary"]
        );
    }

    #[test]
    fn dependency_inherits_application_channel() {
        let fuzzy = dependency_fuzzy("org.deepin.Runtime/23.1.0", "main", UPDATE_FAILED).unwrap();
        assert_eq!(fuzzy.channel.as_deref(), Some("main"));
        assert_eq!(fuzzy.id, "org.deepin.Runtime");
    }

    #[test]
    fn uab_layout_constraints_match_upstream_modes() {
        assert!(validate_uab_layout(&[], false).is_err());
        assert!(validate_uab_layout(&[], true).is_err());

        let app = uab_layer("app.id", "app", "1.0.0", "binary");
        assert!(validate_uab_layout(std::slice::from_ref(&app), true).is_ok());
        assert!(validate_uab_layout(std::slice::from_ref(&app), false).is_ok());

        let runtime = uab_layer("runtime.id", "runtime", "1.0.1", "runtime");
        assert!(validate_uab_layout(&[app.clone(), runtime.clone()], false).is_err());

        let mut app_with_runtime = app.clone();
        app_with_runtime.info.runtime = Some("runtime.id/1.0".to_string());
        assert!(validate_uab_layout(std::slice::from_ref(&app_with_runtime), true).is_err());
        assert!(validate_uab_layout(&[app_with_runtime.clone(), runtime.clone()], true).is_ok());

        let mut wrong_channel = runtime.clone();
        wrong_channel.info.channel = "testing".to_string();
        assert!(validate_uab_layout(&[app_with_runtime.clone(), wrong_channel], true).is_err());
        let mut wrong_version = runtime;
        wrong_version.info.version = "2.0.0".to_string();
        let error = validate_uab_layout(&[app_with_runtime, wrong_version], true).unwrap_err();
        assert_eq!(error.message, "runtime layer version not matched");
    }

    #[test]
    fn uab_group_rejects_arch_id_and_version_mismatches() {
        let first = uab_layer("runtime.id", "runtime", "1.0.0", "binary");
        let mut mismatch = first.clone();
        mismatch.info.id = "other.id".to_string();
        assert!(validate_uab_group(&[first.clone(), mismatch]).is_err());

        let mut mismatch = first.clone();
        mismatch.info.version = "2.0.0".to_string();
        assert!(validate_uab_group(&[first.clone(), mismatch]).is_err());

        let mut mismatch = first.clone();
        mismatch.info.arch = vec![
            if Architecture::current().unwrap() == Architecture::X86_64 {
                Architecture::Arm64
            } else {
                Architecture::X86_64
            }
            .to_string(),
        ];
        let error = validate_uab_group(&[mismatch]).unwrap_err();
        assert_eq!(error.code, INSTALL_ARCH_MISMATCH);
    }

    #[tokio::test]
    async fn uab_extra_modules_require_an_exact_local_main_module() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repository");
        let binary = temporary.path().join("binary");
        fs::create_dir_all(&root).unwrap();
        let binary_info = test_info("app.id", "app", "1.0.0", "binary");
        write_layer(&binary, &binary_info, "binary");
        let mut repository = LocalRepository::create(&root, test_config()).await.unwrap();

        let extra = uab_layer("app.id", "app", "1.0.0", "lang_zh");
        assert!(validate_uab_extra_modules(&repository, std::slice::from_ref(&extra)).is_err());

        repository
            .import_layer_dir(&binary, &[], None)
            .await
            .unwrap();
        assert!(validate_uab_extra_modules(&repository, std::slice::from_ref(&extra)).is_ok());

        let mut wrong_channel = extra.clone();
        wrong_channel.info.channel = "testing".to_string();
        assert!(
            validate_uab_extra_modules(&repository, std::slice::from_ref(&wrong_channel)).is_err()
        );

        let wrong_version = uab_layer("app.id", "app", "2.0.0", "lang_zh");
        assert!(
            validate_uab_extra_modules(&repository, std::slice::from_ref(&wrong_version)).is_err()
        );
    }

    #[tokio::test]
    async fn object_only_prune_keeps_unused_packages() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repository");
        let app = temporary.path().join("app");
        let runtime = temporary.path().join("runtime");
        fs::create_dir_all(&root).unwrap();
        let app_info = test_info("app.id", "app", "1.0.0", "binary");
        let runtime_info = test_info("runtime.id", "runtime", "1.0.0", "binary");
        write_layer(&app, &app_info, "old");
        write_layer(&runtime, &runtime_info, "runtime");
        let mut repository = LocalRepository::create(&root, test_config()).await.unwrap();
        let old = repository.import_layer_dir(&app, &[], None).await.unwrap();
        repository
            .import_layer_dir(&runtime, &[], None)
            .await
            .unwrap();
        fs::write(app.join("payload"), "new").unwrap();
        repository.import_layer_dir(&app, &[], None).await.unwrap();
        let old_object = root
            .join("repo/objects")
            .join(&old.commit[..2])
            .join(format!("{}.commit", &old.commit[2..]));

        prune_after_change(&mut repository, false).await;

        assert!(!old_object.exists());
        assert!(
            repository
                .resolve_local(&"runtime.id".parse::<FuzzyReference>().unwrap(), true)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn deferred_uninstall_waits_until_reference_is_idle() {
        let _lock = TEST_REPO_LOCK_ENV_MUTEX.lock().await;
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repository");
        let source = temporary.path().join("app");
        let process_root = temporary.path().join("processes");
        let repo_lock = temporary.path().join("repository.lock");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(process_root.join("1000")).unwrap();
        fs::write(&repo_lock, []).unwrap();
        let _environment = ProcessStateEnv::set(&process_root, &repo_lock);
        let info = test_info("app.id", "app", "1.0.0", "binary");
        write_layer(&source, &info, "app");
        let mut repository = LocalRepository::create(&root, test_config()).await.unwrap();
        repository
            .import_layer_dir(&source, &[], None)
            .await
            .unwrap();
        let reference = reference_from_info(&info).unwrap();
        assert!(repository.mark_layer_deleted(&reference, "binary").unwrap());
        let state = process_root
            .join("1000")
            .join(std::process::id().to_string());
        fs::write(
            &state,
            serde_json::to_vec(&ContainerProcessStateInfo {
                app: reference.to_string(),
                base: String::new(),
                container_id: "container".to_string(),
                extensions: None,
                runtime: None,
            })
            .unwrap(),
        )
        .unwrap();
        let repository = Arc::new(Mutex::new(repository));

        assert_eq!(deferred_uninstall(repository.clone()).await.unwrap(), 0);
        assert_eq!(repository.lock().await.list_deleted_layer_items().len(), 1);

        fs::remove_file(state).unwrap();
        assert_eq!(deferred_uninstall(repository.clone()).await.unwrap(), 1);
        assert!(
            repository
                .lock()
                .await
                .list_deleted_layer_items()
                .is_empty()
        );
    }
}
