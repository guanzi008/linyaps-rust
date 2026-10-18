use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Component;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use linyaps_api::{BuilderProject, PackageInfoV2, UabLayer, UabMetaInfo, UabSections};
use linyaps_core::{Architecture, FuzzyReference, Reference};
use linyaps_repository::{
    ErofsCompression, LocalRepository, build_erofs_image_with_compression, write_uab_sections,
};
use sha2::{Digest, Sha256};

use crate::build::resolve_dependency;
use crate::project::current_reference;
use crate::source::{clear_path, copy_tree};

pub struct ExportOptions {
    pub compressor: Option<String>,
    pub header: Option<PathBuf>,
    pub icon: Option<PathBuf>,
    pub loader: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub reference: Option<String>,
    pub modules: Vec<String>,
    pub executable: bool,
}

pub async fn export_uab(
    repository: &mut LocalRepository,
    project: Option<&BuilderProject>,
    current_directory: &Path,
    options: ExportOptions,
) -> Result<PathBuf> {
    let compressor = options.compressor.as_deref().unwrap_or("lz4");
    let compression = compressor.parse::<ErofsCompression>()?;
    let reference = if let Some(raw) = options.reference.as_deref() {
        let mut fuzzy = raw.parse::<FuzzyReference>()?;
        if fuzzy.architecture.is_none() {
            fuzzy.architecture = Some(Architecture::current()?);
        }
        repository.resolve_local(&fuzzy, true)?
    } else {
        let project = project.context("not under project")?;
        current_reference(project)?
    };
    if !options.executable && (options.icon.is_some() || options.loader.is_some()) {
        bail!("--icon and --loader require --uabx");
    }
    let modules = if options.modules.is_empty() {
        vec!["binary".to_string()]
    } else {
        options.modules.clone()
    };
    let working = current_directory.join(".uabBuild");
    clear_path(&working)?;
    fs::create_dir_all(&working)?;
    let bundle_tree = working.join("bundle");
    fs::create_dir_all(&bundle_tree)?;
    let uuid = random_uuid();
    let target_tools = if options.header.is_none() {
        Some(resolve_builder_utils(repository, reference.architecture).await?)
    } else {
        None
    };
    let result = if options.executable {
        prepare_executable(
            repository,
            &reference,
            &bundle_tree,
            &modules,
            options.loader.as_deref(),
        )
    } else {
        prepare_distributed(repository, &reference, &bundle_tree, &modules)
    };
    let (layers, only_app) = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = clear_path(&working);
            return Err(error);
        }
    };
    let bundle = build_erofs_image_with_compression(&bundle_tree, compression, false)?;
    let digest = linyaps_core::hex_encode(Sha256::digest(&bundle));
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
    let header = options.header.unwrap_or_else(|| {
        target_tools
            .expect("builder utils are resolved when no custom header is provided")
            .header
    });
    let output = options.output.unwrap_or_else(|| {
        current_directory.join(format!(
            "{}_{}_{}_{}.{}",
            reference.id,
            reference.version,
            reference.architecture,
            reference.channel,
            if options.executable { "uabx" } else { "uab" }
        ))
    });
    let mut sections = vec![
        ("linglong.bundle", bundle.as_slice()),
        ("linglong.meta", metadata.as_slice()),
    ];
    if let Some(icon) = icon.as_deref() {
        sections.push(("linglong.icon", icon));
    }
    write_uab_sections(&header, &output, &sections)?;
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

fn prepare_executable(
    repository: &LocalRepository,
    application_reference: &Reference,
    bundle: &Path,
    modules: &[String],
    custom_loader: Option<&Path>,
) -> Result<(Vec<UabLayer>, Option<bool>)> {
    if !modules.iter().any(|module| module == "binary") {
        bail!("binary module is required in UABX mode");
    }
    let app_info = repository.read_layer_info(application_reference, "binary")?;
    if app_info.kind != "app" {
        bail!("only app packages can be exported in UABX mode");
    }
    let app_destination = bundle
        .join("layers")
        .join(&app_info.id)
        .join(&app_info.module);
    for module in modules {
        let source = repository.layer_path(application_reference, module)?;
        overlay_tree(&source, &app_destination)?;
    }
    fs::write(
        app_destination.join("info.json"),
        serde_json::to_vec(&app_info)?,
    )?;
    ensure_exec_entry(
        &app_destination.join("files/entry.sh"),
        app_info.command.as_deref(),
        &Path::new("/opt/apps").join(&app_info.id).join("files"),
    )?;

    let loader = bundle.join("loader");
    if let Some(custom_loader) = custom_loader {
        fs::copy(custom_loader, &loader)?;
    } else {
        fs::write(&loader, default_exec_loader())?;
    }
    let mut loader_permissions = fs::metadata(bundle.join("loader"))?.permissions();
    loader_permissions.set_mode(loader_permissions.mode() | 0o111);
    fs::set_permissions(bundle.join("loader"), loader_permissions)?;
    Ok((
        vec![UabLayer {
            info: app_info,
            minified: false,
        }],
        Some(true),
    ))
}

fn overlay_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        clear_path(destination)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        symlink(fs::read_link(source)?, destination)?;
        return Ok(());
    }
    if metadata.is_dir() {
        if fs::symlink_metadata(destination).is_ok_and(|metadata| !metadata.is_dir()) {
            clear_path(destination)?;
        }
        fs::create_dir_all(destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            overlay_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }
    if metadata.is_file() {
        clear_path(destination)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        return Ok(());
    }
    bail!("unsupported layer entry {}", source.display())
}

fn ensure_exec_entry(entry: &Path, command: Option<&[String]>, prefix: &Path) -> Result<()> {
    match fs::metadata(entry) {
        Ok(metadata) => {
            if !metadata.is_file() {
                bail!("{} is not a regular file", entry.display());
            }
            if metadata.permissions().mode() & 0o111 == 0 {
                bail!("{} is not executable", entry.display());
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let command = command
        .filter(|command| !command.is_empty())
        .context("package command is required for executable bundle")?;
    let script = generate_exec_entry(command, prefix)?;
    if let Some(parent) = entry.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(entry, script)?;
    let mut permissions = fs::metadata(entry)?.permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    fs::set_permissions(entry, permissions)?;
    Ok(())
}

fn generate_exec_entry(command: &[String], prefix: &Path) -> Result<String> {
    let executable = command
        .first()
        .filter(|value| !value.is_empty())
        .context("package command is empty")?;
    let executable = lexical_normalize(Path::new(executable));
    if executable.as_os_str().is_empty()
        || executable == Path::new(".")
        || executable == Path::new("..")
        || executable
            .components()
            .next()
            .is_some_and(|component| component == Component::ParentDir)
    {
        bail!("invalid package command: {}", command[0]);
    }

    let mut script = "#!/bin/sh\nset -eu\n: \"${LINGLONG_UAB_APPROOT:?LINGLONG_UAB_APPROOT is not set}\"\ncd \"$LINGLONG_UAB_APPROOT\"\nexec ".to_string();
    if executable.is_relative() {
        script.push_str("\"$LINGLONG_UAB_APPROOT/bin\"/");
        script.push_str(&quote_shell(&executable.to_string_lossy()));
    } else if let Ok(relative) = executable.strip_prefix(lexical_normalize(prefix)) {
        script.push_str("\"$LINGLONG_UAB_APPROOT\"");
        if !relative.as_os_str().is_empty() {
            script.push('/');
            script.push_str(&quote_shell(&relative.to_string_lossy()));
        }
    } else {
        script.push_str(&quote_shell(&executable.to_string_lossy()));
    }
    for argument in &command[1..] {
        script.push(' ');
        script.push_str(&quote_shell(argument));
    }
    script.push_str(" \"$@\"\n");
    Ok(script)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if output.file_name().is_some() => {
                output.pop();
            }
            component => output.push(component.as_os_str()),
        }
    }
    output
}

fn quote_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn default_exec_loader() -> &'static str {
    "#!/bin/sh\nset -eu\n: \"${LINGLONG_UAB_APPROOT:?LINGLONG_UAB_APPROOT is not set}\"\nexec \"$LINGLONG_UAB_APPROOT/entry.sh\" \"$@\"\n"
}

#[derive(Clone, Debug)]
struct UabTools {
    header: PathBuf,
}

async fn resolve_builder_utils(
    repository: &mut LocalRepository,
    architecture: Architecture,
) -> Result<UabTools> {
    let reference = resolve_dependency(
        repository,
        "cn.org.linyaps.builder.utils",
        architecture,
        true,
    )
    .await
    .with_context(|| {
        format!("failed to get builder utils for target architecture {architecture}")
    })?;
    let minimum = linyaps_core::Version::parse("0.0.4.0").expect("valid minimum version");
    if reference.version < minimum {
        bail!(
            "builder utils {} is too old; version 0.0.4.0 or newer is required",
            reference.version,
        );
    }
    repository
        .merge_modules()
        .with_context(|| format!("failed to merge builder utils {reference}"))?;
    let root = repository
        .merged_layer_path(&reference)
        .with_context(|| format!("failed to locate builder utils {reference}"))?
        .join("files");
    let header = root.join("lib/linglong/builder/uab/uab-header");
    if !header.is_file() {
        bail!("builder utils did not provide uab-header for target architecture {architecture}");
    }
    Ok(UabTools { header })
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
                    region: None,
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
        repository.import_layer_dir(&base, &[]).await.unwrap();
        repository.import_layer_dir(&app, &[]).await.unwrap();
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
        let header = temporary.path().join("uab-header");
        linyaps_repository::append_elf_sections(
            &executable,
            &header,
            &[(
                linyaps_repository::UAB_SIGNATURE_SECTION,
                linyaps_repository::UAB_SIGNATURE_NOTE.as_slice(),
            )],
        )
        .unwrap();
        let output = temporary.path().join("app.uabx");
        export_uab(
            &mut repository,
            Some(&project),
            temporary.path(),
            ExportOptions {
                compressor: None,
                header: Some(header),
                icon: None,
                loader: Some(executable),
                output: Some(output.clone()),
                reference: None,
                modules: Vec::new(),
                executable: true,
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
