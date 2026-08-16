use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use linyaps_api::{BuilderProject, PackageInfoV2, UabLayer, UabMetaInfo, UabSections};
use linyaps_core::{Architecture, FuzzyReference, Reference};
use linyaps_repository::{
    ErofsCompression, LocalRepository, append_elf_sections, build_erofs_image_with_compression,
};
use sha2::{Digest, Sha256};

use crate::build::resolve_dependency;
use crate::project::current_reference;
use crate::repo_ops::directory_size;
use crate::source::{clear_path, copy_tree};
use crate::uab_packaging::{
    append_base_dependencies, bundled_library_blacklist, copy_filtered_layer, load_needed_files,
};

pub struct ExportOptions {
    pub compressor: Option<String>,
    pub header: Option<PathBuf>,
    pub icon: Option<PathBuf>,
    pub loader: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub reference: Option<String>,
    pub modules: Vec<String>,
}

pub async fn export_uab(
    repository: &mut LocalRepository,
    project: Option<&BuilderProject>,
    current_directory: &Path,
    options: ExportOptions,
) -> Result<PathBuf> {
    let compressor = options.compressor.as_deref().unwrap_or("lz4");
    let compression = compressor.parse::<ErofsCompression>()?;
    let distributed = options.reference.is_some();
    let reference = if let Some(raw) = options.reference.as_deref() {
        let mut fuzzy = raw.parse::<FuzzyReference>()?;
        if fuzzy.architecture.is_none() {
            fuzzy.architecture = Some(Architecture::current()?);
        }
        repository.resolve_local(&fuzzy, true)?
    } else {
        let project = project.context("not under project")?;
        if project.package.kind != "app" {
            bail!(
                "can't export {} kind UAB in executable mode, if you want to export UAB in distributed mode, please use --ref option instead",
                project.package.kind
            );
        }
        current_reference(project)?
    };
    let working = current_directory.join(".uabBuild");
    clear_path(&working)?;
    fs::create_dir_all(&working)?;
    let bundle_tree = working.join("bundle");
    fs::create_dir_all(&bundle_tree)?;
    let uuid = random_uuid();
    let target_tools = if options.header.is_none() || (!distributed && options.loader.is_none()) {
        resolve_builder_utils(repository, reference.architecture).await
    } else {
        None
    };
    let result = if distributed {
        prepare_distributed(repository, &reference, &bundle_tree, &options.modules)
    } else {
        prepare_executable(
            repository,
            project.context("project is not set")?,
            &reference,
            &bundle_tree,
            current_directory,
            &uuid,
            options.loader.as_deref(),
            target_tools.as_ref(),
        )
    };
    let (layers, only_app) = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = clear_path(&working);
            return Err(error);
        }
    };
    let bundle = build_erofs_image_with_compression(&bundle_tree, compression, false)?;
    let digest = format!("{:x}", Sha256::digest(&bundle));
    let icon = options
        .icon
        .as_deref()
        .map(fs::read)
        .transpose()
        .context("failed to read UAB icon")?;
    let metadata = UabMetaInfo {
        digest,
        layers,
        only_app,
        sections: UabSections {
            bundle: "linglong.bundle".to_string(),
            icon: icon.as_ref().map(|_| "linglong.icon".to_string()),
        },
        uuid,
        version: "1".to_string(),
    };
    let metadata = serde_json::to_vec(&metadata)?;
    let header = options
        .header
        .or_else(|| target_tools.as_ref().and_then(|tools| tools.header.clone()))
        .map(Ok)
        .unwrap_or_else(|| {
            find_tool(
                "LINGLONG_UAB_HEADER",
                "uab-header",
                &[
                    "/usr/lib/linglong/builder/uab/uab-header",
                    "/usr/libexec/linglong/uab-header",
                    "/usr/lib/linglong/uab-header",
                ],
            )
        })?;
    let output = options.output.unwrap_or_else(|| {
        current_directory.join(format!(
            "{}_{}_{}_{}.uab",
            reference.id, reference.version, reference.architecture, reference.channel
        ))
    });
    let mut sections = vec![
        ("linglong.bundle", bundle.as_slice()),
        ("linglong.meta", metadata.as_slice()),
    ];
    if let Some(icon) = icon.as_deref() {
        sections.push(("linglong.icon", icon));
    }
    append_elf_sections(&header, &output, &sections)?;
    let mut permissions = fs::metadata(&output)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(&output, permissions)?;
    if env::var_os("LINGLONG_UAB_DEBUG").is_none() {
        let _ = clear_path(&working);
    }
    Ok(output)
}

fn prepare_distributed(
    repository: &LocalRepository,
    reference: &Reference,
    bundle: &Path,
    requested_modules: &[String],
) -> Result<(Vec<UabLayer>, Option<bool>)> {
    let modules = if requested_modules.is_empty() {
        vec!["binary".to_string()]
    } else {
        requested_modules.to_vec()
    };
    let mut layers = Vec::new();
    for module in modules {
        let source = repository.layer_path(reference, &module)?;
        let info: PackageInfoV2 = serde_json::from_slice(&fs::read(source.join("info.json"))?)?;
        let destination = bundle.join("layers").join(&info.id).join(&info.module);
        copy_tree(&source, &destination)?;
        layers.push(UabLayer {
            info,
            minified: false,
        });
    }
    Ok((layers, None))
}

#[allow(clippy::too_many_arguments)]
fn prepare_executable(
    repository: &LocalRepository,
    project: &BuilderProject,
    application_reference: &Reference,
    bundle: &Path,
    current_directory: &Path,
    uuid: &str,
    custom_loader: Option<&Path>,
    target_tools: Option<&UabTools>,
) -> Result<(Vec<UabLayer>, Option<bool>)> {
    let app_source = repository.merged_layer_path(application_reference)?;
    let mut app_info = repository.read_layer_info(application_reference, "binary")?;
    let base_raw = project
        .base
        .as_deref()
        .context("couldn't find base layer")?;
    let base_reference = resolve_local(repository, base_raw, application_reference.architecture)?;
    let base_source = repository.merged_layer_path(&base_reference)?;
    let runtime = project
        .runtime
        .as_deref()
        .map(|raw| resolve_local(repository, raw, application_reference.architecture))
        .transpose()?;
    let blacklist = bundled_library_blacklist(runtime.is_some());
    let needed = runtime
        .as_ref()
        .map(|_| load_needed_files(&current_directory.join("linglong/depends.yaml")))
        .transpose()?
        .unwrap_or_default();
    let excludes = project.exclude.as_deref().unwrap_or_default();
    let includes = project.include.as_deref().unwrap_or_default();
    let mut layers = Vec::new();
    let mut dependency_minified = false;

    if let Some(runtime_reference) = runtime.as_ref()
        && custom_loader.is_none()
    {
        let runtime_source = repository.merged_layer_path(runtime_reference)?;
        let mut runtime_info = repository.read_layer_info(runtime_reference, "binary")?;
        let runtime_destination = bundle
            .join("layers")
            .join(&runtime_info.id)
            .join(&runtime_info.module);
        dependency_minified = copy_filtered_layer(
            &runtime_source,
            &runtime_destination,
            excludes,
            includes,
            &blacklist,
        )?;
        append_base_dependencies(
            &base_source.join("files"),
            &runtime_destination.join("files"),
            &needed,
            &blacklist,
            application_reference.architecture.triplet(),
        )?;
        runtime_info.size = directory_size(&runtime_destination.join("files"))?;
        fs::write(
            runtime_destination.join("info.json"),
            serde_json::to_vec(&runtime_info)?,
        )?;
        layers.push(UabLayer {
            info: runtime_info,
            minified: dependency_minified,
        });
    }

    let app_destination = bundle
        .join("layers")
        .join(&app_info.id)
        .join(&app_info.module);
    let app_minified = copy_filtered_layer(
        &app_source,
        &app_destination,
        excludes,
        includes,
        &blacklist,
    )?;
    if dependency_minified || app_minified {
        app_info.uuid = Some(uuid.to_string());
    }
    fs::write(
        app_destination.join("info.json"),
        serde_json::to_vec(&app_info)?,
    )?;
    layers.push(UabLayer {
        info: app_info,
        minified: app_minified,
    });

    let loader = custom_loader
        .map(PathBuf::from)
        .or_else(|| target_tools.and_then(|tools| tools.loader.clone()))
        .map(Ok)
        .unwrap_or_else(|| {
            find_tool(
                "LINGLONG_UAB_LOADER",
                "uab-loader",
                &[
                    "/usr/lib/linglong/builder/uab/uab-loader",
                    "/usr/libexec/linglong/uab-loader",
                    "/usr/lib/linglong/uab-loader",
                ],
            )
        })?;
    fs::copy(&loader, bundle.join("loader"))?;
    let mut loader_permissions = fs::metadata(bundle.join("loader"))?.permissions();
    loader_permissions.set_mode(loader_permissions.mode() | 0o111);
    fs::set_permissions(bundle.join("loader"), loader_permissions)?;
    let extra = bundle.join("extra");
    fs::create_dir_all(&extra)?;
    if custom_loader.is_none() {
        let box_binary = target_tools
            .and_then(|tools| tools.box_binary.clone())
            .map(Ok)
            .unwrap_or_else(|| find_tool("LINGLONG_BOX", "ll-box", &["/usr/bin/ll-box"]))?;
        fs::copy(box_binary, extra.join("ll-box"))?;
        let mut permissions = fs::metadata(extra.join("ll-box"))?.permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(extra.join("ll-box"), permissions)?;
    }
    for (source, name) in [
        (
            base_source.join("files/etc/linglong-triplet-list"),
            "linglong-triplet-list",
        ),
        (
            base_source.join("files/etc/profile.d/linglong.sh"),
            "profile",
        ),
    ] {
        if source.is_file() {
            fs::copy(source, extra.join(name))?;
        }
    }
    Ok((layers, Some(true)))
}

#[derive(Clone, Debug)]
struct UabTools {
    header: Option<PathBuf>,
    loader: Option<PathBuf>,
    box_binary: Option<PathBuf>,
}

async fn resolve_builder_utils(
    repository: &mut LocalRepository,
    architecture: Architecture,
) -> Option<UabTools> {
    let reference = match resolve_dependency(
        repository,
        "cn.org.linyaps.builder.utils",
        architecture,
        true,
    )
    .await
    {
        Ok(reference) => reference,
        Err(error) => {
            eprintln!("warning: failed to get builder utils for arch {architecture}: {error:#}");
            return None;
        }
    };
    if let Err(error) = repository.merge_modules() {
        eprintln!("warning: failed to merge builder utils {reference}: {error}");
        return None;
    }
    let root = match repository.merged_layer_path(&reference) {
        Ok(root) => root.join("files"),
        Err(error) => {
            eprintln!("warning: failed to locate builder utils {reference}: {error}");
            return None;
        }
    };
    let present = |path: PathBuf| path.is_file().then_some(path);
    let header = present(root.join("lib/linglong/builder/uab/uab-header"));
    let loader = present(root.join("lib/linglong/builder/uab/uab-loader"));
    let box_binary = present(root.join("bin/ll-box"));
    if header.is_none() || loader.is_none() {
        eprintln!("warning: builder utils {reference} does not contain UAB tools");
    }
    Some(UabTools {
        header,
        loader,
        box_binary,
    })
}

fn resolve_local(
    repository: &LocalRepository,
    raw: &str,
    architecture: Architecture,
) -> Result<Reference> {
    let mut fuzzy = raw.parse::<FuzzyReference>()?;
    if fuzzy.architecture.is_none() {
        fuzzy.architecture = Some(architecture);
    }
    repository.resolve_local(&fuzzy, true).map_err(Into::into)
}

fn find_tool(variable: &str, name: &str, system_paths: &[&str]) -> Result<PathBuf> {
    if let Some(path) = env::var_os(variable).map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        bail!("{} {} is missing", name, path.display());
    }
    if let Ok(current) = env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join(name);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    for path in system_paths {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Some(path) = env::var_os("PATH") {
        for directory in env::split_paths(&path) {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!("couldn't find {name}")
}

fn random_uuid() -> String {
    fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|uuid| uuid.trim().to_string())
        .unwrap_or_else(|_| {
            format!(
                "{:08x}-0000-4000-8000-{:012x}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_nanos() as u64)
                    & 0xffffffffffff
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use linyaps_api::{Repo, RepoConfigV2};
    use linyaps_repository::{LocalRepository, UabFile};
    use tempfile::tempdir;

    #[test]
    fn filters_reject_relative_and_parent_paths() {
        assert!(crate::uab_packaging::filter_path("usr/lib").is_err());
        assert!(crate::uab_packaging::filter_path("/usr/../etc").is_err());
        assert_eq!(
            crate::uab_packaging::filter_path("/usr/lib").unwrap(),
            Path::new("usr/lib")
        );
    }

    #[tokio::test]
    async fn exports_verifiable_executable_uab() {
        let temporary = tempdir().unwrap();
        let repository_root = temporary.path().join("repository");
        fs::create_dir_all(&repository_root).unwrap();
        let mut repository = LocalRepository::create(
            &repository_root,
            RepoConfigV2 {
                default_repo: "stable".to_string(),
                repos: vec![Repo {
                    alias: None,
                    mirror_enabled: None,
                    name: "stable".to_string(),
                    priority: 0,
                    url: "https://example.invalid".to_string(),
                }],
                version: 2,
            },
        )
        .await
        .unwrap();
        let base = temporary.path().join("base");
        let app = temporary.path().join("app");
        fs::create_dir_all(base.join("files/lib/x86_64-linux-gnu")).unwrap();
        fs::create_dir_all(app.join("files/bin")).unwrap();
        fs::write(base.join("files/lib/x86_64-linux-gnu/libdemo.so"), "base").unwrap();
        fs::write(app.join("files/bin/demo"), "app").unwrap();
        let base_info = PackageInfoV2 {
            arch: vec!["x86_64".to_string()],
            base: String::new(),
            channel: "main".to_string(),
            command: None,
            compatible_version: None,
            description: Some("Base".to_string()),
            extension_implementation: None,
            extensions: None,
            id: "org.deepin.base".to_string(),
            kind: "base".to_string(),
            module: "binary".to_string(),
            name: "Base".to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 4,
            uuid: None,
            version: "23.1.0.0".to_string(),
        };
        let app_info = PackageInfoV2 {
            arch: vec!["x86_64".to_string()],
            base: "org.deepin.base/23.1.0".to_string(),
            channel: "main".to_string(),
            command: Some(vec!["/opt/apps/org.example.App/files/bin/demo".to_string()]),
            compatible_version: None,
            description: Some("App".to_string()),
            extension_implementation: None,
            extensions: None,
            id: "org.example.App".to_string(),
            kind: "app".to_string(),
            module: "binary".to_string(),
            name: "App".to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 3,
            uuid: None,
            version: "1.0.0.0".to_string(),
        };
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
        repository.import_layer_dir(&base, &[], None).await.unwrap();
        repository.import_layer_dir(&app, &[], None).await.unwrap();
        repository.merge_modules().unwrap();
        let project: BuilderProject = serde_yml::from_str(
            r#"version: "1"
package:
  id: org.example.App
  name: App
  version: 1.0.0.0
  kind: app
  description: App
command: [/opt/apps/org.example.App/files/bin/demo]
base: org.deepin.base/23.1.0
build: demo
"#,
        )
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let output = temporary.path().join("app.uab");
        export_uab(
            &mut repository,
            Some(&project),
            temporary.path(),
            ExportOptions {
                compressor: None,
                header: Some(executable.clone()),
                icon: None,
                loader: Some(executable),
                output: Some(output.clone()),
                reference: None,
                modules: Vec::new(),
            },
        )
        .await
        .unwrap();
        let uab = UabFile::open(&output).unwrap();
        uab.verify().unwrap();
        let metadata = uab.metadata().unwrap();
        assert_eq!(metadata.only_app, Some(true));
        assert_eq!(metadata.layers.len(), 1);
        let unpacked = temporary.path().join("unpacked");
        uab.unpack_bundle(&unpacked).unwrap();
        assert!(unpacked.join("loader").is_file());
        assert!(!unpacked.join("layers/org.deepin.base").exists());
        assert!(
            unpacked
                .join("layers/org.example.App/binary/files/bin/demo")
                .is_file()
        );
    }
}
