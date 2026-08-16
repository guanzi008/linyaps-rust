use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use linyaps_api::{BuilderProject, ExtensionImplementation, PackageInfoV2};
use linyaps_core::{Architecture, FuzzyReference, Reference, Version, VersionV1};

const EXAMPLE_TEMPLATE: &str = include_str!("../templates/example.yaml");

pub fn create_project(name: &str, current_directory: &Path) -> Result<PathBuf> {
    if name.is_empty() {
        bail!(
            "{}",
            linyaps_i18n::gettext("Input parameter is empty, please input valid parameter instead",)
        );
    }
    let directory = current_directory.join(name);
    if directory.exists() {
        bail!("{name} project dir already exists");
    }
    fs::create_dir_all(&directory)
        .with_context(|| format!("create project dir failed: {}", directory.display()))?;
    let project_file = directory.join("linglong.yaml");
    fs::write(&project_file, EXAMPLE_TEMPLATE.replace("@ID@", name))
        .with_context(|| format!("Failed to write config file {}", project_file.display()))?;
    Ok(directory)
}

pub fn locate_project_file(current_directory: &Path, requested: Option<&Path>) -> Result<PathBuf> {
    let current_directory = current_directory
        .canonicalize()
        .context("invalid current directory")?;
    if let Some(requested) = requested {
        let path = requested
            .canonicalize()
            .with_context(|| format!("invalid file path {}", requested.display()))?;
        if !path.starts_with(&current_directory) {
            bail!(
                "the project file {} is not under the current working directory {}",
                path.display(),
                current_directory.display()
            );
        }
        return Ok(path);
    }
    let architecture = Architecture::current()?.to_string();
    for name in [
        format!("linglong.{architecture}.yaml"),
        "linglong.yaml".to_string(),
    ] {
        let path = current_directory.join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    bail!("project yaml file not found")
}

pub fn load_project(path: &Path) -> Result<BuilderProject> {
    eprintln!("Using project file {}", path.display());
    let project: BuilderProject = serde_yml::from_str(
        &fs::read_to_string(path)
            .with_context(|| format!("parse project config {}", path.display()))?,
    )
    .with_context(|| format!("parse project config {}", path.display()))?;
    validate_project(&project)?;
    Ok(project)
}

pub fn validate_project(project: &BuilderProject) -> Result<()> {
    let version = VersionV1::parse(&project.package.version).map_err(|_| {
        anyhow::anyhow!(
            "Please ensure the package.version number has three parts formatted as 'MAJOR.MINOR.PATCH.TWEAK'"
        )
    })?;
    if version.tweak.is_none() {
        bail!(
            "Please ensure the package.version number has three parts formatted as 'MAJOR.MINOR.PATCH.TWEAK'"
        );
    }
    if project
        .modules
        .as_ref()
        .is_some_and(|modules| modules.iter().any(|module| module.name == "binary"))
    {
        bail!(
            "configuration of binary modules is not allowed. see https://linglong.space/guide/ll-builder/modules.html"
        );
    }
    if project.package.kind == "app"
        && project
            .command
            .as_ref()
            .is_none_or(|command| command.is_empty())
    {
        bail!("'command' field is missing, app should have command as the default startup command");
    }
    if project.base.is_none() && project.runtime.is_none() {
        bail!("at least one of 'base' or 'runtime' must be specified");
    }
    validate_dependency(project.base.as_deref(), "base")?;
    validate_dependency(project.runtime.as_deref(), "runtime")?;
    Ok(())
}

pub fn current_reference(project: &BuilderProject) -> Result<Reference> {
    let architecture = project
        .package
        .architecture
        .as_deref()
        .map(str::parse)
        .transpose()?
        .map_or_else(Architecture::current, Ok)?;
    Reference::new(
        project.package.channel.as_deref().unwrap_or("main"),
        &project.package.id,
        Version::parse(&project.package.version)?,
        architecture,
    )
    .map_err(Into::into)
}

pub fn package_info(project: &BuilderProject, module: &str, size: i64) -> Result<PackageInfoV2> {
    let reference = current_reference(project)?;
    let extension_implementation =
        (project.package.kind == "extension").then(|| ExtensionImplementation {
            device_nodes: project.package.device_nodes.clone(),
            env: project.package.env.clone(),
            libs: project.package.libs.clone(),
        });
    Ok(PackageInfoV2 {
        arch: vec![reference.architecture.to_string()],
        base: project.base.clone().unwrap_or_default(),
        channel: reference.channel,
        command: project.command.clone(),
        compatible_version: None,
        description: Some(project.package.description.clone()),
        extension_implementation,
        extensions: None,
        id: project.package.id.clone(),
        kind: project.package.kind.clone(),
        module: module.to_string(),
        name: project.package.name.clone(),
        permissions: project.permissions.clone(),
        runtime: project.runtime.clone(),
        schema_version: "1.0".to_string(),
        size,
        uuid: None,
        version: project.package.version.clone(),
    })
}

fn validate_dependency(dependency: Option<&str>, field: &str) -> Result<()> {
    let Some(dependency) = dependency else {
        return Ok(());
    };
    let reference = dependency
        .parse::<FuzzyReference>()
        .with_context(|| format!("failed to parse {field} field"))?;
    let version = reference
        .version
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("{field} version is missing"))?;
    if !valid_dependency_version(version) {
        bail!("{field} version is not valid");
    }
    Ok(())
}

fn valid_dependency_version(version: &str) -> bool {
    let parts = version.split('.').collect::<Vec<_>>();
    (parts.len() == 2 || parts.len() == 3)
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part == &"0" || !part.starts_with('0'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> BuilderProject {
        serde_yml::from_str(
            r#"version: "1"
package:
  id: org.example.App
  name: Example
  version: 1.2.3.4
  kind: app
  description: Example
command: [example]
base: org.deepin.base/23.1.0
build: echo build
"#,
        )
        .unwrap()
    }

    #[test]
    fn validates_upstream_project_rules() {
        validate_project(&project()).unwrap();
        let mut invalid = project();
        invalid.package.version = "1.2.3".to_string();
        assert!(validate_project(&invalid).is_err());
        let mut invalid = project();
        invalid.command = None;
        assert!(validate_project(&invalid).is_err());
        let mut invalid = project();
        invalid.command = Some(Vec::new());
        assert!(validate_project(&invalid).is_err());
    }

    #[test]
    fn dependency_versions_match_upstream_pattern() {
        for version in ["1.0", "1.0.0", "0.0.0"] {
            assert!(valid_dependency_version(version));
        }
        for version in ["1", "1.0.0.0", "01.0", "1.0-beta"] {
            assert!(!valid_dependency_version(version));
        }
    }
}
