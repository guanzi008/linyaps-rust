use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use linyaps_api::RepoConfigV2;
use linyaps_core::{Reference, RepoOperation, RepoOperationResult, apply_repo_operation};
use linyaps_repository::{
    ErofsCompression, LocalRepository, reference_from_info, unpack_layer,
    write_layer_file_with_compression,
};

use crate::project::{current_reference, package_info};
use linyaps_api::BuilderProject;

pub async fn import_path(repository: &mut LocalRepository, path: &Path) -> Result<()> {
    if path.is_dir() {
        repository
            .import_layer_dir(path, &[], None)
            .await
            .with_context(|| format!("Import layer directory failed: {}", path.display()))?;
        repository.merge_modules()?;
        return Ok(());
    }
    if !path.is_file() {
        bail!("Layer file path doesn't exist: {}", path.display());
    }
    let temporary = repository.root().join("tmp").join(format!(
        "builder-import-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let result = (|| -> Result<()> {
        unpack_layer(path, &temporary)
            .with_context(|| format!("Import layer failed: {}", path.display()))?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = clear_path(&temporary);
        return Err(error);
    }
    let result = repository
        .import_layer_dir(&temporary, &[], None)
        .await
        .with_context(|| format!("Import layer failed: {}", path.display()));
    let _ = clear_path(&temporary);
    result?;
    repository.merge_modules()?;
    Ok(())
}

pub fn extract_layer(layer: &Path, destination: &Path) -> Result<()> {
    if destination.exists() {
        bail!("{} already exists", destination.display());
    }
    unpack_layer(layer, destination).with_context(|| {
        format!(
            "Extract layer failed: {} to {}",
            layer.display(),
            destination.display()
        )
    })?;
    Ok(())
}

pub fn list(repository: &LocalRepository) -> Result<()> {
    let references = repository
        .list_layer_items()
        .iter()
        .filter_map(|item| reference_from_info(&item.info).ok())
        .map(|reference| reference.to_string())
        .collect::<BTreeSet<_>>();
    for reference in references {
        println!("{reference}");
    }
    Ok(())
}

pub async fn remove(repository: &mut LocalRepository, references: &[String]) -> Result<()> {
    for raw in references {
        let reference = match raw.parse::<Reference>() {
            Ok(reference) => reference,
            Err(error) => {
                eprintln!("{raw}: {error}");
                continue;
            }
        };
        for module in repository.module_list(&reference) {
            if let Err(error) = repository.remove_layer(&reference, &module).await {
                eprintln!("{raw}: {error}");
            }
        }
    }
    repository.merge_modules()?;
    Ok(())
}

pub fn export_project_layers(
    repository: &LocalRepository,
    project: &BuilderProject,
    current_directory: &Path,
    no_develop: bool,
    compressor: Option<&str>,
) -> Result<Vec<PathBuf>> {
    let compression = compressor.unwrap_or("lzma").parse::<ErofsCompression>()?;
    let reference = current_reference(project)?;
    let modules = repository.module_list(&reference);
    if modules.is_empty() {
        bail!("no {} found", reference);
    }
    let mut outputs = Vec::new();
    for module in modules {
        if no_develop && module == "develop" {
            continue;
        }
        let layer = repository.layer_path(&reference, &module)?;
        let info_path = layer.join("info.json");
        let info = if info_path.is_file() {
            serde_json::from_slice(&fs::read(&info_path)?)?
        } else {
            package_info(project, &module, directory_size(&layer)?)?
        };
        let output = current_directory.join(format!(
            "{}_{}_{}_{}.layer",
            reference.id, reference.version, reference.architecture, module
        ));
        write_layer_file_with_compression(&layer, &info, &output, compression)
            .with_context(|| format!("export layer {reference}/{module} failed"))?;
        outputs.push(output);
    }
    Ok(outputs)
}

pub fn apply_repository_operation(
    repository: &mut LocalRepository,
    operation: RepoOperation,
) -> Result<()> {
    let mut config = repository.config().clone();
    match apply_repo_operation(&mut config, operation)? {
        RepoOperationResult::Unchanged => Ok(()),
        RepoOperationResult::Changed => repository.update_config(config).map_err(Into::into),
        RepoOperationResult::Show(config) => {
            print!("{}", format_repository_config(&config));
            Ok(())
        }
    }
}

pub fn clean(current_directory: &Path) -> Result<()> {
    let internal = current_directory.join("linglong");
    if internal.exists() {
        clear_path(&internal)
            .with_context(|| format!("failed to remove {}", internal.display()))?;
    }
    Ok(())
}

pub fn directory_size(path: &Path) -> Result<i64> {
    fn visit(path: &Path, size: &mut u64) -> std::io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(&entry?.path(), size)?;
            }
        } else if metadata.is_file() {
            *size = size.saturating_add(metadata.len());
        }
        Ok(())
    }
    let mut size = 0;
    visit(path, &mut size)?;
    i64::try_from(size).context("directory is too large")
}

fn format_repository_config(config: &RepoConfigV2) -> String {
    const MAX_URL_LENGTH: usize = 100;
    let name_width = config
        .repos
        .iter()
        .map(|repo| repo.name.len())
        .max()
        .unwrap_or(0)
        + 2;
    let url_width = config
        .repos
        .iter()
        .map(|repo| repo.url.len())
        .max()
        .unwrap_or(0)
        .min(MAX_URL_LENGTH)
        + 2;
    let alias_width = config
        .repos
        .iter()
        .map(|repo| repo.effective_name().len())
        .max()
        .unwrap_or(0)
        + 2;
    let mut output = format!("Default: {}\n", config.default_repo);
    output.push_str(&format!(
        "\x1b[38;5;214m{}{}{}{}\x1b[0m\n",
        display_column(&linyaps_i18n::gettext("Name"), name_width),
        display_column(&linyaps_i18n::gettext("Url"), url_width),
        display_column(&linyaps_i18n::gettext("Alias"), alias_width),
        display_column(&linyaps_i18n::gettext("Priority"), 10),
    ));
    let mut repos = config.repos.clone();
    repos.sort_by_key(|repo| Reverse(repo.priority));
    for repo in repos {
        let alias = repo.effective_name().to_string();
        let url = if repo.url.len() > MAX_URL_LENGTH {
            format!("{}...", &repo.url[..97])
        } else {
            repo.url
        };
        output.push_str(&format!(
            "{:<name_width$}{:<url_width$}{:<alias_width$}{:<10}\n",
            repo.name, url, alias, repo.priority
        ));
    }
    output
}

fn display_column(value: &str, width: usize) -> String {
    let padding = width.saturating_sub(display_width(value));
    format!("{value}{}", " ".repeat(padding))
}

fn display_width(value: &str) -> usize {
    value.chars().map(character_display_width).sum()
}

fn character_display_width(character: char) -> usize {
    let value = character as u32;
    if character.is_control()
        || matches!(
            value,
            0x0300..=0x036f
                | 0x1ab0..=0x1aff
                | 0x1dc0..=0x1dff
                | 0x20d0..=0x20ff
                | 0xfe20..=0xfe2f
        )
    {
        return 0;
    }
    if matches!(
        value,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1faff
            | 0x20000..=0x3fffd
    ) {
        2
    } else {
        1
    }
}

fn clear_path(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}
