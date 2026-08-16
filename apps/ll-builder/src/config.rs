use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use linyaps_api::{BuilderConfig, Repo, RepoConfigV2};
use linyaps_core::repository::load_config;
use linyaps_repository::LocalRepository;

pub fn load_builder_config() -> Result<BuilderConfig> {
    let path = builder_config_path()?;
    if !path.exists() {
        let cache = xdg_home("XDG_CACHE_HOME", ".cache")?;
        let config = BuilderConfig {
            arch: None,
            cache: None,
            offline: None,
            repo: cache
                .join("linglong-builder")
                .to_string_lossy()
                .into_owned(),
            version: 1,
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        }
        fs::write(&path, serde_yml::to_string(&config)?).with_context(|| {
            format!(
                "failed to save default build config file {}",
                path.display()
            )
        })?;
        return Ok(config);
    }
    let config: BuilderConfig = serde_yml::from_str(
        &fs::read_to_string(&path)
            .with_context(|| format!("failed to load build config {}", path.display()))?,
    )
    .with_context(|| format!("parse build config {}", path.display()))?;
    if config.version != 1 {
        bail!("wrong configuration file version {}", config.version);
    }
    Ok(config)
}

pub async fn open_repository(config: &BuilderConfig) -> Result<LocalRepository> {
    let root = PathBuf::from(&config.repo);
    fs::create_dir_all(&root).with_context(|| {
        format!(
            "failed to create the repository of builder: {}",
            root.display()
        )
    })?;
    LocalRepository::create(root, fallback_repository_config()?)
        .await
        .context("failed to create ostree repo")
}

fn builder_config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("LINGLONG_BUILDER_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    Ok(xdg_home("XDG_CONFIG_HOME", ".config")?.join("linglong/builder/config.yaml"))
}

fn xdg_home(variable: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(path) = env::var_os(variable).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|home| home.join(fallback))
        .ok_or_else(|| anyhow::anyhow!("neither {variable} nor HOME is set"))
}

fn fallback_repository_config() -> Result<RepoConfigV2> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("LINGLONG_REPO_CONFIG") {
        candidates.push(PathBuf::from(path));
    }
    candidates.extend([
        PathBuf::from("/etc/linglong/config.yaml"),
        PathBuf::from("/usr/share/linglong/config.yaml"),
    ]);
    for path in candidates {
        if path.is_file() {
            return load_config(Path::new(&path))
                .with_context(|| format!("failed to load repository config {}", path.display()));
        }
    }
    Ok(RepoConfigV2 {
        default_repo: "stable".to_string(),
        repos: vec![Repo {
            alias: None,
            mirror_enabled: None,
            name: "stable".to_string(),
            priority: 0,
            url: "https://mirror-repo-linglong.deepin.com".to_string(),
        }],
        version: 2,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_config_matches_distribution_default() {
        let config = fallback_repository_config().unwrap();
        assert_eq!(config.default_repo, "stable");
        assert_eq!(
            config.repos[0].url,
            "https://mirror-repo-linglong.deepin.com"
        );
    }
}
