use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::mem::MaybeUninit;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use linyaps_api::{BuilderConfig, BuilderProject, BuilderProjectModule, PackageInfoV2, Repo};
use linyaps_core::repository::priority_grouped_repos;
use linyaps_core::{Architecture, FuzzyReference, Reference};
use linyaps_repository::{
    LocalRepository, RemotePackages, RemoteRepositoryClient, reference_from_info,
};

use crate::container::{self, ContainerExtension, ContainerRequest};
use crate::postprocess::{
    check_runtime_dependencies, rewrite_application_configuration, strip_debug_symbols,
    validate_exported_configuration,
};
use crate::project::{current_reference, package_info};
use crate::repo_ops::directory_size;
use crate::source::{clear_path, copy_tree, fetch_sources};

pub struct BuildOptions {
    pub command: Vec<String>,
    pub offline: bool,
    pub full_develop_module: bool,
    pub skip_fetch_source: bool,
    pub skip_pull_depend: bool,
    pub skip_run_container: bool,
    pub skip_commit_output: bool,
    pub skip_output_check: bool,
    pub skip_strip_symbols: bool,
    pub isolate_network: bool,
}

pub struct RunOptions {
    pub command: Vec<String>,
    pub modules: Vec<String>,
    pub debug: bool,
    pub workdir: Option<PathBuf>,
    pub extensions: Vec<String>,
}

struct ResolvedDependencies {
    base: Reference,
    base_files: PathBuf,
    runtime: Option<Reference>,
    runtime_files: Option<PathBuf>,
}

pub async fn build(
    repository: &mut LocalRepository,
    config: &BuilderConfig,
    project: &mut BuilderProject,
    project_file: &Path,
    current_directory: &Path,
    options: BuildOptions,
) -> Result<()> {
    let deprecated_install = current_directory.join(format!("{}.install", project.package.id));
    if deprecated_install.exists() {
        bail!(
            "$appid.install is deprecated, please use modules instead. see https://linglong.space/guide/ll-builder/modules.html"
        );
    }
    let internal = current_directory.join("linglong");
    fs::create_dir_all(&internal)?;
    let offline = options.offline || config.offline.unwrap_or(false);
    if !offline
        && !options.skip_fetch_source
        && let Some(sources) = &project.sources
    {
        eprintln!("[Processing Sources]");
        fetch_sources(sources, &internal, config).await?;
    }
    eprintln!("[Processing Dependency]");
    let dependencies =
        resolve_dependencies(repository, project, !(offline || options.skip_pull_depend)).await?;

    if options.skip_run_container {
        return Ok(());
    }

    let output_root = internal.join("output");
    let build_output = output_root.join("_build");
    let ran_default_build = options.command.is_empty();
    clear_path(&output_root)?;
    fs::create_dir_all(&build_output)?;
    let entry = internal.join("entry.sh");
    write_entry_script(&entry, &project.build, options.skip_strip_symbols)?;

    let prepared_base = internal.join("buildext/build-base");
    let prepared_runtime = internal.join("buildext/build-runtime");
    let mut active_base = dependencies.base_files.as_path();
    let mut active_runtime = dependencies.runtime_files.as_deref();
    if let Some(packages) = apt_packages(project, AptStage::BuildDepends) {
        let script = internal.join("buildext.sh");
        write_apt_script(&script, packages, AptStage::BuildDepends)?;
        eprintln!("[Processing buildext.apt.buildDepends]");
        let code = container::run_with_writeback(
            ContainerRequest {
                base: &dependencies.base_files,
                runtime: dependencies.runtime_files.as_deref(),
                project_directory: current_directory,
                internal_directory: &internal,
                application: None,
                output: None,
                extensions: Vec::new(),
                arguments: vec![
                    "/bin/bash".to_string(),
                    "/project/linglong/buildext.sh".to_string(),
                ],
                working_directory: "/project".to_string(),
                isolate_network: false,
                writable_root: true,
            },
            container::ContainerWriteback {
                root: &prepared_base,
                runtime: dependencies
                    .runtime_files
                    .as_ref()
                    .map(|_| prepared_runtime.as_path()),
            },
        )?;
        if code != 0 {
            bail!(
                "failed to process buildext.apt.buildDepends: container exited with status {code}"
            );
        }
        active_base = &prepared_base;
        active_runtime = dependencies
            .runtime_files
            .as_ref()
            .map(|_| prepared_runtime.as_path());
    }

    let arguments = if options.command.is_empty() {
        vec![
            "/bin/bash".to_string(),
            "/project/linglong/entry.sh".to_string(),
        ]
    } else {
        options.command.clone()
    };
    let prefix = install_prefix(project)?;
    eprintln!("[Start Build]");
    let code = container::run(ContainerRequest {
        base: active_base,
        runtime: active_runtime,
        project_directory: current_directory,
        internal_directory: &internal,
        application: None,
        output: Some((&build_output, &prefix)),
        extensions: Vec::new(),
        arguments,
        working_directory: "/project".to_string(),
        isolate_network: options.isolate_network,
        writable_root: true,
    })?;
    if code != 0 {
        bail!("Build failed: container exited with status {code}");
    }
    if options.skip_commit_output {
        return Ok(());
    }
    if !build_output.is_dir() {
        bail!("build output does not exist: {}", build_output.display());
    }
    process_runtime_dependencies(
        project,
        &dependencies,
        current_directory,
        &internal,
        &build_output,
    )?;
    if ran_default_build && !options.skip_strip_symbols {
        strip_debug_symbols(&build_output, &install_prefix(project)?)?;
    }
    rewrite_application_configuration(project, &build_output)?;
    eprintln!("[Install Files]");
    let modules = split_modules(
        project,
        &build_output,
        &output_root,
        options.full_develop_module,
    )?;
    generate_entries(project, &output_root)?;
    commit_modules(repository, project, project_file, &output_root, &modules).await?;
    let check_result = check_output(
        project,
        repository,
        &dependencies,
        &internal.join("depends.yaml"),
    );
    if let Err(error) = check_result {
        if options.skip_output_check {
            eprintln!("warning: runtime check ignored: {error:#}");
        } else {
            return Err(error);
        }
    }
    eprintln!("Successfully build {}", project.package.id);
    let _ = dependencies.base;
    let _ = dependencies.runtime;
    Ok(())
}

pub async fn run_built(
    repository: &mut LocalRepository,
    project: &mut BuilderProject,
    current_directory: &Path,
    options: RunOptions,
) -> Result<i32> {
    if project.package.kind != "app" {
        bail!("only app can run");
    }
    let dependencies = resolve_dependencies(repository, project, false).await?;
    let reference = current_reference(project)?;
    let mut modules = vec!["binary".to_string()];
    if options.debug {
        modules.push("develop".to_string());
    }
    for module in options.modules {
        if !modules.contains(&module) {
            modules.push(module);
        }
    }
    let internal = current_directory.join("linglong");
    let application = merge_selected_modules(repository, &reference, &modules, &internal)?;
    let mut extensions = Vec::new();
    for requested in &options.extensions {
        let extension =
            resolve_dependency(repository, requested, reference.architecture, false).await?;
        let info = repository.read_layer_info(&extension, "binary")?;
        if info.kind != "extension" {
            bail!("{extension} is not an extension");
        }
        let implementation = info.extension_implementation.as_ref();
        let environment = implementation
            .and_then(|value| value.env.clone())
            .unwrap_or_default();
        let devices = implementation
            .and_then(|value| value.device_nodes.as_deref())
            .map(|values| {
                values
                    .iter()
                    .map(|value| {
                        let source = value.host_path.as_deref().unwrap_or(&value.path);
                        (PathBuf::from(source), PathBuf::from(&value.path))
                    })
                    .collect()
            })
            .unwrap_or_default();
        repository.merge_modules()?;
        extensions.push(ContainerExtension {
            source: repository.merged_layer_path(&extension)?.join("files"),
            id: extension.id,
            environment,
            devices,
        });
    }
    let arguments = if options.command.is_empty() {
        project
            .command
            .clone()
            .filter(|command| !command.is_empty())
            .unwrap_or_else(|| vec!["/bin/bash".to_string()])
    } else {
        options.command
    };
    container::run(ContainerRequest {
        base: &dependencies.base_files,
        runtime: dependencies.runtime_files.as_deref(),
        project_directory: current_directory,
        internal_directory: &internal,
        application: Some((&application, &project.package.id)),
        output: None,
        extensions,
        arguments,
        working_directory: options
            .workdir
            .unwrap_or_else(|| PathBuf::from("/project"))
            .to_string_lossy()
            .into_owned(),
        isolate_network: false,
        writable_root: false,
    })
}

async fn resolve_dependencies(
    repository: &mut LocalRepository,
    project: &mut BuilderProject,
    pull: bool,
) -> Result<ResolvedDependencies> {
    let architecture = project
        .package
        .architecture
        .as_deref()
        .map(str::parse)
        .transpose()?
        .map_or_else(Architecture::current, Ok)?;
    let runtime = if let Some(raw) = project.runtime.as_deref() {
        Some(resolve_dependency(repository, raw, architecture, pull).await?)
    } else {
        None
    };
    if project.base.is_none() {
        let runtime = runtime
            .as_ref()
            .context("at least one of base or runtime must be specified")?;
        let info = repository.read_layer_info(runtime, "binary")?;
        if info.base.is_empty() {
            bail!("base required by runtime is missing");
        }
        project.base = Some(info.base);
    }
    let base_raw = project.base.as_deref().context("base is missing")?;
    let base = resolve_dependency(repository, base_raw, architecture, pull).await?;
    if let Some(runtime) = &runtime {
        let runtime_info = repository.read_layer_info(runtime, "binary")?;
        if !runtime_info.base.is_empty() {
            let required = runtime_info.base.parse::<FuzzyReference>()?;
            if !base.semantic_match(&required) {
                bail!(
                    "base is not compatible with runtime. \n - Current base: {}\n - Current runtime: {}\n - base required by runtime: {}",
                    base,
                    runtime,
                    runtime_info.base
                );
            }
        }
    }
    repository.merge_modules()?;
    let base_files = repository.merged_layer_path(&base)?.join("files");
    let runtime_files = runtime
        .as_ref()
        .map(|reference| {
            repository
                .merged_layer_path(reference)
                .map(|path| path.join("files"))
        })
        .transpose()?;
    Ok(ResolvedDependencies {
        base,
        base_files,
        runtime,
        runtime_files,
    })
}

pub(crate) async fn resolve_dependency(
    repository: &mut LocalRepository,
    raw: &str,
    architecture: Architecture,
    pull: bool,
) -> Result<Reference> {
    let mut fuzzy = raw.parse::<FuzzyReference>()?;
    if fuzzy.architecture.is_none() {
        fuzzy.architecture = Some(architecture);
    }
    let local = repository.resolve_local(&fuzzy, true).ok();
    if !pull {
        return local.ok_or_else(|| anyhow::anyhow!("dependency not found locally: {raw}"));
    }
    let mut selected: Option<(Repo, PackageInfoV2)> = None;
    for group in priority_grouped_repos(repository.config()) {
        let mut packages = RemotePackages::default();
        for remote in group {
            let client = match RemoteRepositoryClient::new(&remote.url) {
                Ok(client) => client,
                Err(error) => {
                    eprintln!(
                        "warning: failed to use repository {}: {error}",
                        remote.effective_name()
                    );
                    continue;
                }
            };
            match client.search_packages(&fuzzy, &remote, true).await {
                Ok(found) => {
                    let found = found
                        .into_iter()
                        .filter(|package| matches!(package.module.as_str(), "binary" | "runtime"))
                        .collect::<Vec<_>>();
                    packages.add_packages(remote, found);
                }
                Err(error) => eprintln!(
                    "warning: failed to search repository {}: {error}",
                    remote.effective_name()
                ),
            }
        }
        if !packages.is_empty() {
            selected = packages.latest_package().ok();
            if selected.is_some() {
                break;
            }
        }
    }
    let Some((remote, package)) = selected else {
        return local.ok_or_else(|| anyhow::anyhow!("dependency not found: {raw}"));
    };
    let reference = reference_from_info(&package)?;
    match repository.pull(&reference, &remote, "binary").await {
        Ok(_) => {
            if let Err(error) = repository.pull(&reference, &remote, "develop").await {
                eprintln!("warning: failed to pull develop module of {reference}: {error}");
            }
            Ok(reference)
        }
        Err(error) => {
            if let Some(local) = local {
                eprintln!(
                    "warning: failed to pull {reference}, use local version {local}: {error}"
                );
                Ok(local)
            } else {
                Err(error.into())
            }
        }
    }
}

fn write_entry_script(path: &Path, script: &str, skip_strip_symbols: bool) -> Result<()> {
    let mut content = "#!/bin/bash\nset -e\n\n# This file is generated by `build` in linglong.yaml\n# DO NOT EDIT IT\n".to_string();
    if !skip_strip_symbols {
        content.push_str("\n# enable strip symbols\nexport CFLAGS=\"-g $CFLAGS\"\nexport CXXFLAGS=\"-g $CXXFLAGS\"\n");
    }
    content.push_str(script);
    content.push('\n');
    fs::write(path, content)?;
    let mut permissions = fs::metadata(path)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum AptStage {
    BuildDepends,
    Depends,
}

fn apt_packages(project: &BuilderProject, stage: AptStage) -> Option<&[String]> {
    let apt = project.buildext.as_ref()?.apt.as_ref()?;
    let packages = match stage {
        AptStage::BuildDepends => apt.build_depends.as_deref(),
        AptStage::Depends => apt.depends.as_deref(),
    }?;
    (!packages.is_empty()).then_some(packages)
}

fn write_apt_script(path: &Path, packages: &[String], stage: AptStage) -> Result<()> {
    let packages = packages
        .iter()
        .map(|package| format!(" {package}"))
        .collect::<String>();
    let content = match stage {
        AptStage::BuildDepends => format!(
            "echo 'APT::Sandbox::User \"root\";' > /etc/apt/apt.conf.d/99linglong-builder.conf\napt update\napt -y install{packages}\n"
        ),
        AptStage::Depends => format!(
            "apt -o APT::Sandbox::User=root update || echo \"$?\"\napt -o APT::Sandbox::User=root -y install{packages} || echo \"$?\"\n"
        ),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn process_runtime_dependencies(
    project: &BuilderProject,
    dependencies: &ResolvedDependencies,
    current_directory: &Path,
    internal: &Path,
    build_output: &Path,
) -> Result<()> {
    let Some(packages) = apt_packages(project, AptStage::Depends) else {
        return Ok(());
    };
    let script = internal.join("buildext.sh");
    write_apt_script(&script, packages, AptStage::Depends)?;
    let prepared_base = internal.join("buildext/depends-base");
    let prepared_runtime = internal.join("buildext/depends-runtime");
    let code = container::run_with_writeback(
        ContainerRequest {
            base: &dependencies.base_files,
            runtime: dependencies.runtime_files.as_deref(),
            project_directory: current_directory,
            internal_directory: internal,
            application: None,
            output: None,
            extensions: Vec::new(),
            arguments: vec![
                "/bin/bash".to_string(),
                "/project/linglong/buildext.sh".to_string(),
            ],
            working_directory: "/project".to_string(),
            isolate_network: false,
            writable_root: true,
        },
        container::ContainerWriteback {
            root: &prepared_base,
            runtime: dependencies
                .runtime_files
                .as_ref()
                .map(|_| prepared_runtime.as_path()),
        },
    )?;
    if code != 0 {
        bail!("failed to process buildext.apt.depends: container exited with status {code}");
    }
    merge_dependency_changes(
        &prepared_base.join("usr"),
        &dependencies.base_files.join("usr"),
        build_output,
    )?;
    if matches!(project.package.kind.as_str(), "app" | "extension")
        && let Some(runtime) = dependencies.runtime_files.as_deref()
    {
        merge_dependency_changes(&prepared_runtime, runtime, build_output)?;
    }
    Ok(())
}

fn merge_dependency_changes(modified: &Path, original: &Path, output: &Path) -> Result<()> {
    for target in ["bin", "sbin", "lib"] {
        copy_changed_entries(
            &modified.join(target),
            &original.join(target),
            &output.join(target),
            Path::new(target),
        )?;
    }
    Ok(())
}

fn copy_changed_entries(
    modified: &Path,
    original: &Path,
    output: &Path,
    relative: &Path,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(modified) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if relative.starts_with("lib/systemd") || relative.starts_with("share/systemd") {
        return Ok(());
    }
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        for entry in fs::read_dir(modified)? {
            let entry = entry?;
            let name = entry.file_name();
            copy_changed_entries(
                &entry.path(),
                &original.join(&name),
                &output.join(&name),
                &relative.join(name),
            )?;
        }
        return Ok(());
    }
    if entries_equal(modified, original)? {
        return Ok(());
    }
    clear_path(output)?;
    copy_tree(modified, output)
}

fn entries_equal(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata = fs::symlink_metadata(left)?;
    let right_metadata = match fs::symlink_metadata(right) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if left_metadata.file_type() != right_metadata.file_type()
        || left_metadata.permissions().mode() != right_metadata.permissions().mode()
    {
        return Ok(false);
    }
    if left_metadata.file_type().is_symlink() {
        return Ok(fs::read_link(left)? == fs::read_link(right)?);
    }
    if left_metadata.is_file() {
        return Ok(
            left_metadata.len() == right_metadata.len() && fs::read(left)? == fs::read(right)?
        );
    }
    Ok(true)
}

fn install_prefix(project: &BuilderProject) -> Result<String> {
    match project.package.kind.as_str() {
        "app" => Ok(format!("/opt/apps/{}/files", project.package.id)),
        "runtime" => Ok("/runtime".to_string()),
        "extension" => Ok(format!("/opt/extensions/{}", project.package.id)),
        kind => bail!("unsupported package kind: {kind}"),
    }
}

fn split_modules(
    project: &BuilderProject,
    build_output: &Path,
    output_root: &Path,
    full_develop: bool,
) -> Result<Vec<String>> {
    let mut modules = project.modules.clone().unwrap_or_default();
    let has_develop = modules.iter().any(|module| module.name == "develop");
    let mut package_modules = modules
        .iter()
        .map(|module| module.name.clone())
        .collect::<Vec<_>>();
    if full_develop && !has_develop {
        let destination = output_root.join("develop/files");
        copy_tree(build_output, &destination)?;
        package_modules.push("develop".to_string());
    } else if !has_develop {
        modules.push(BuilderProjectModule {
            files: vec![
                "^/include/.+".to_string(),
                "^/lib/debug/.+".to_string(),
                "^/lib/.+\\.a$".to_string(),
            ],
            name: "develop".to_string(),
        });
        package_modules.push("develop".to_string());
    }
    modules.push(BuilderProjectModule {
        files: vec!["/".to_string()],
        name: "binary".to_string(),
    });
    package_modules.push("binary".to_string());
    for module in modules {
        let destination = output_root.join(&module.name).join("files");
        fs::create_dir_all(&destination)?;
        let rules = module
            .files
            .iter()
            .map(|rule| ModuleRule::new(rule))
            .collect::<Result<Vec<_>>>()?;
        move_matching(build_output, &destination, &rules)?;
    }
    package_modules.sort();
    package_modules.dedup();
    Ok(package_modules)
}

fn move_matching(source: &Path, destination: &Path, rules: &[ModuleRule]) -> Result<()> {
    let mut entries = Vec::new();
    collect_leaves(source, source, &mut entries)?;
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.components().count()));
    for relative in entries {
        let path = format!("/{}", relative.to_string_lossy());
        if !rules.iter().any(|rule| rule.matches(&path)) {
            continue;
        }
        let from = source.join(&relative);
        if fs::symlink_metadata(&from).is_err() {
            continue;
        }
        let to = destination.join(&relative);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(_) => {
                copy_tree(&from, &to)?;
                clear_path(&from)?;
            }
        }
    }
    remove_empty_directories(source)?;
    Ok(())
}

fn collect_leaves(root: &Path, path: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        if path != root {
            output.push(path.strip_prefix(root)?.to_path_buf());
        }
        return Ok(());
    }
    let entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    if entries.is_empty() && path != root {
        output.push(path.strip_prefix(root)?.to_path_buf());
    } else {
        for entry in entries {
            collect_leaves(root, &entry.path(), output)?;
        }
    }
    Ok(())
}

fn remove_empty_directories(path: &Path) -> Result<bool> {
    if !fs::symlink_metadata(path)?.is_dir() {
        return Ok(false);
    }
    for entry in fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()? {
        let child = entry.path();
        if fs::symlink_metadata(&child)?.is_dir() {
            let _ = remove_empty_directories(&child)?;
        }
    }
    if fs::read_dir(path)?.next().is_none() {
        fs::remove_dir(path)?;
        return Ok(true);
    }
    Ok(false)
}

struct ModuleRule {
    original: String,
    regex: Option<PosixRegex>,
}

impl ModuleRule {
    fn new(rule: &str) -> Result<Self> {
        Ok(Self {
            original: rule.to_string(),
            regex: (rule != "/").then(|| PosixRegex::new(rule)).transpose()?,
        })
    }

    fn matches(&self, path: &str) -> bool {
        if self.original == "/" {
            return true;
        }
        let literal = self.original.trim_end_matches('/');
        if literal.starts_with('/')
            && (path == literal
                || path
                    .strip_prefix(literal)
                    .is_some_and(|rest| rest.starts_with('/')))
        {
            return true;
        }
        self.regex
            .as_ref()
            .is_some_and(|regex| regex.is_match(path))
    }
}

struct PosixRegex {
    regex: libc::regex_t,
}

impl PosixRegex {
    fn new(pattern: &str) -> Result<Self> {
        let pattern = CString::new(pattern).context("module rule contains NUL")?;
        let mut regex = MaybeUninit::<libc::regex_t>::uninit();
        let code = unsafe {
            libc::regcomp(
                regex.as_mut_ptr(),
                pattern.as_ptr(),
                libc::REG_EXTENDED | libc::REG_NOSUB,
            )
        };
        if code != 0 {
            let mut buffer = vec![0_u8; 1024];
            unsafe {
                libc::regerror(
                    code,
                    regex.as_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                );
            }
            let message =
                String::from_utf8_lossy(buffer.split(|byte| *byte == 0).next().unwrap_or_default());
            bail!("invalid module rule: {message}");
        }
        Ok(Self {
            regex: unsafe { regex.assume_init() },
        })
    }

    fn is_match(&self, value: &str) -> bool {
        let Ok(value) = CString::new(value) else {
            return false;
        };
        unsafe { libc::regexec(&self.regex, value.as_ptr(), 0, std::ptr::null_mut(), 0) == 0 }
    }
}

impl Drop for PosixRegex {
    fn drop(&mut self) {
        unsafe { libc::regfree(&mut self.regex) };
    }
}

fn generate_entries(project: &BuilderProject, output_root: &Path) -> Result<()> {
    if project.package.kind != "app" {
        return Ok(());
    }
    let files = output_root.join("binary/files");
    let entries = output_root.join("binary/entries");
    fs::create_dir_all(&entries)?;
    for relative in [
        "share/applications",
        "share/mime",
        "share/icons",
        "share/dbus-1",
        "share/gnome-shell",
        "share/appdata",
        "share/metainfo",
        "share/plugins",
        "share/templates",
    ] {
        let source = files.join(relative);
        if !source.exists() {
            continue;
        }
        let target = if relative == "share/appdata" {
            entries.join("share/metainfo")
        } else {
            entries.join(relative)
        };
        copy_tree(&source, &target)?;
    }
    let systemd = files.join("lib/systemd/user");
    if systemd.is_dir() {
        copy_tree(&systemd, &entries.join("lib/systemd/user"))?;
    }
    Ok(())
}

async fn commit_modules(
    repository: &mut LocalRepository,
    project: &BuilderProject,
    project_file: &Path,
    output_root: &Path,
    modules: &[String],
) -> Result<()> {
    let reference = current_reference(project)?;
    for module in repository.module_list(&reference) {
        repository.remove_layer(&reference, &module).await?;
    }
    eprintln!("[Commit Contents]");
    for module in modules {
        let directory = output_root.join(module);
        let size = directory_size(&directory)?;
        let info = package_info(project, module, size)?;
        fs::write(directory.join("info.json"), serde_json::to_vec(&info)?)?;
        fs::copy(project_file, directory.join("linglong.yaml"))?;
        repository.import_layer_dir(&directory, &[], None).await?;
        eprintln!("{} {} {} complete", info.id, info.version, module);
    }
    repository.merge_modules()?;
    Ok(())
}

fn check_output(
    project: &BuilderProject,
    repository: &LocalRepository,
    dependencies: &ResolvedDependencies,
    depends_file: &Path,
) -> Result<()> {
    let reference = current_reference(project)?;
    let binary = repository.layer_path(&reference, "binary")?;
    if !binary.join("info.json").is_file() || !binary.join("files").is_dir() {
        bail!("runtime check failed: invalid binary module");
    }
    for invalid in validate_exported_configuration(project, &binary.join("files"))? {
        eprintln!(
            "warning: exported configuration file should start with {}: {}",
            project.package.id,
            invalid.display()
        );
    }
    if project.package.kind == "app"
        && current_reference(project)?.architecture == Architecture::current()?
    {
        check_runtime_dependencies(
            project,
            &binary.join("files"),
            &dependencies.base_files,
            dependencies.runtime_files.as_deref(),
            depends_file,
        )?;
    }
    Ok(())
}

fn merge_selected_modules(
    repository: &LocalRepository,
    reference: &Reference,
    modules: &[String],
    internal: &Path,
) -> Result<PathBuf> {
    let root = internal.join("run-layer");
    clear_path(&root)?;
    fs::create_dir_all(&root)?;
    let selected = modules.iter().cloned().collect::<BTreeSet<_>>();
    for module in selected {
        let layer = repository.layer_path(reference, &module)?;
        copy_tree(&layer, &root)?;
    }
    let files = root.join("files");
    if !files.is_dir() {
        bail!("application layer has no files directory");
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn module_rules_split_default_develop_files() {
        let temporary = tempdir().unwrap();
        let build = temporary.path().join("_build");
        let output = temporary.path().join("output");
        fs::create_dir_all(build.join("include/demo")).unwrap();
        fs::create_dir_all(build.join("lib/debug")).unwrap();
        fs::create_dir_all(build.join("bin")).unwrap();
        fs::write(build.join("include/demo/api.h"), "header").unwrap();
        fs::write(build.join("lib/libdemo.a"), "archive").unwrap();
        fs::write(build.join("lib/debug/demo"), "debug").unwrap();
        fs::write(build.join("bin/demo"), "binary").unwrap();
        let project: BuilderProject = serde_yml::from_str(
            r#"version: "1"
package:
  id: org.example.App
  name: Demo
  version: 1.0.0.0
  kind: app
  description: Demo
command: [demo]
base: org.deepin.base/23.1.0
build: demo
"#,
        )
        .unwrap();
        let modules = split_modules(&project, &build, &output, false).unwrap();
        assert_eq!(modules, ["binary", "develop"]);
        assert!(output.join("develop/files/include/demo/api.h").is_file());
        assert!(output.join("develop/files/lib/libdemo.a").is_file());
        assert!(output.join("binary/files/bin/demo").is_file());
    }

    #[test]
    fn posix_rules_support_custom_regular_expressions() {
        let rule = ModuleRule::new(r"^/share/.+\.qm$").unwrap();
        assert!(rule.matches("/share/i18n/demo.qm"));
        assert!(!rule.matches("/share/i18n/demo.mo"));
    }

    #[test]
    fn buildext_apt_scripts_match_upstream_protocol() {
        let project: BuilderProject = serde_yml::from_str(
            r#"version: "1"
package:
  id: org.example.App
  name: Demo
  version: 1.0.0.0
  kind: app
  description: Demo
command: [demo]
base: org.deepin.base/23.1.0
build: demo
buildext:
  apt:
    build_depends: [cmake, ninja-build]
    depends: [libdemo1]
"#,
        )
        .unwrap();
        let temporary = tempdir().unwrap();
        let build_script = temporary.path().join("build.sh");
        write_apt_script(
            &build_script,
            apt_packages(&project, AptStage::BuildDepends).unwrap(),
            AptStage::BuildDepends,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(build_script).unwrap(),
            "echo 'APT::Sandbox::User \"root\";' > /etc/apt/apt.conf.d/99linglong-builder.conf\napt update\napt -y install cmake ninja-build\n"
        );
        let depends_script = temporary.path().join("depends.sh");
        write_apt_script(
            &depends_script,
            apt_packages(&project, AptStage::Depends).unwrap(),
            AptStage::Depends,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(depends_script).unwrap(),
            "apt -o APT::Sandbox::User=root update || echo \"$?\"\napt -o APT::Sandbox::User=root -y install libdemo1 || echo \"$?\"\n"
        );
    }

    #[test]
    fn merges_only_changed_dependency_runtime_files() {
        let temporary = tempdir().unwrap();
        let original = temporary.path().join("original/usr");
        let modified = temporary.path().join("modified/usr");
        let output = temporary.path().join("output");
        for root in [&original, &modified] {
            fs::create_dir_all(root.join("lib/systemd")).unwrap();
            fs::write(root.join("lib/same.so"), "same").unwrap();
            fs::write(root.join("lib/changed.so"), "old").unwrap();
            fs::write(root.join("lib/systemd/ignored.service"), "ignored").unwrap();
        }
        fs::write(modified.join("lib/changed.so"), "new").unwrap();
        fs::write(modified.join("lib/added.so"), "added").unwrap();
        fs::create_dir_all(output.join("lib")).unwrap();
        fs::write(output.join("lib/changed.so"), "application copy").unwrap();

        merge_dependency_changes(&modified, &original, &output).unwrap();

        assert_eq!(fs::read(output.join("lib/changed.so")).unwrap(), b"new");
        assert_eq!(fs::read(output.join("lib/added.so")).unwrap(), b"added");
        assert!(!output.join("lib/same.so").exists());
        assert!(!output.join("lib/systemd/ignored.service").exists());
    }
}
