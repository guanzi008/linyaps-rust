use std::cmp::Reverse;
use std::fs;
use std::path::Path;

use linyaps_api::{Repo, RepoConfig, RepoConfigV2};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RepositoryConfigError {
    #[error("failed to read repository configuration: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse repository configuration: {0}")]
    Parse(#[from] serde_yml::Error),
    #[error("default repo not found in repos")]
    MissingDefault,
    #[error("repository list is empty")]
    Empty,
}

pub fn load_config(path: &Path) -> Result<RepoConfigV2, RepositoryConfigError> {
    let content = fs::read_to_string(path)?;
    serde_yml::from_str::<RepoConfigV2>(&content).or_else(|_| {
        serde_yml::from_str::<RepoConfig>(&content)
            .map(convert_to_v2)
            .map_err(RepositoryConfigError::from)
    })
}

pub fn save_config(config: &RepoConfigV2, path: &Path) -> Result<(), RepositoryConfigError> {
    if !config
        .repos
        .iter()
        .any(|repo| repo.effective_name() == config.default_repo)
    {
        return Err(RepositoryConfigError::MissingDefault);
    }
    fs::write(path, serde_yml::to_string(config)?)?;
    Ok(())
}

pub fn convert_to_v2(config: RepoConfig) -> RepoConfigV2 {
    let mut repos = Vec::with_capacity(config.repos.len());
    let mut priority = 0;
    if let Some(url) = config.repos.get(&config.default_repo) {
        repos.push(Repo {
            alias: None,
            mirror_enabled: None,
            name: config.default_repo.clone(),
            priority,
            url: url.clone(),
        });
        priority -= 100;
    }
    for (name, url) in config.repos {
        if name == config.default_repo {
            continue;
        }
        repos.push(Repo {
            alias: None,
            mirror_enabled: None,
            name,
            priority,
            url,
        });
        priority -= 100;
    }
    RepoConfigV2 {
        default_repo: config.default_repo,
        repos,
        version: 2,
    }
}

pub fn default_repo(config: &RepoConfigV2) -> Result<&Repo, RepositoryConfigError> {
    config
        .repos
        .iter()
        .find(|repo| repo.effective_name() == config.default_repo)
        .ok_or(RepositoryConfigError::MissingDefault)
}

pub fn minimum_priority(config: &RepoConfigV2) -> Result<i64, RepositoryConfigError> {
    config
        .repos
        .iter()
        .map(|repo| repo.priority)
        .min()
        .ok_or(RepositoryConfigError::Empty)
}

pub fn maximum_priority(config: &RepoConfigV2) -> Result<i64, RepositoryConfigError> {
    config
        .repos
        .iter()
        .map(|repo| repo.priority)
        .max()
        .ok_or(RepositoryConfigError::Empty)
}

pub fn priority_sorted_repos(config: &RepoConfigV2) -> Vec<Repo> {
    let mut repos = config.repos.clone();
    repos.sort_by_key(|repo| Reverse(repo.priority));
    repos
}

pub fn priority_grouped_repos(config: &RepoConfigV2) -> Vec<Vec<Repo>> {
    let mut groups: Vec<Vec<Repo>> = Vec::new();
    for repo in priority_sorted_repos(config) {
        if groups
            .last()
            .is_none_or(|group| group[0].priority != repo.priority)
        {
            groups.push(Vec::new());
        }
        groups
            .last_mut()
            .expect("group was just created")
            .push(repo);
    }
    groups
}
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn repo(name: &str, priority: i64) -> Repo {
        Repo {
            alias: None,
            mirror_enabled: Some(false),
            name: name.to_string(),
            priority,
            url: format!("http://example.com/{name}"),
        }
    }

    #[test]
    fn converts_v1_with_default_first() {
        let config = RepoConfig {
            default_repo: "repo2".to_string(),
            repos: BTreeMap::from([
                ("repo1".to_string(), "http://example.com/repo1".to_string()),
                ("repo2".to_string(), "http://example.com/repo2".to_string()),
            ]),
            version: 1,
        };
        let converted = convert_to_v2(config);
        assert_eq!(converted.version, 2);
        assert_eq!(converted.repos[0].name, "repo2");
        assert_eq!(converted.repos[0].priority, 0);
        assert_eq!(converted.repos[1].priority, -100);
    }

    #[test]
    fn sorts_and_groups_stably() {
        let config = RepoConfigV2 {
            default_repo: "repo3".to_string(),
            repos: vec![
                repo("repo2", 100),
                repo("repo4", 200),
                repo("repo3", 300),
                repo("repo1", 200),
            ],
            version: 2,
        };
        assert_eq!(minimum_priority(&config).unwrap(), 100);
        assert_eq!(maximum_priority(&config).unwrap(), 300);
        let groups = priority_grouped_repos(&config);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[1][0].name, "repo4");
        assert_eq!(groups[1][1].name, "repo1");
    }
}
