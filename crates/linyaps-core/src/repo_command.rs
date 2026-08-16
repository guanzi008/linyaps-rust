use linyaps_api::{Repo, RepoConfigV2};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoOperation {
    Add {
        name: String,
        url: String,
        alias: Option<String>,
    },
    Modify,
    Remove {
        alias: String,
    },
    Update {
        alias: String,
        url: String,
    },
    SetDefault {
        alias: String,
    },
    Show,
    SetPriority {
        alias: String,
        priority: i64,
    },
    EnableMirror {
        alias: String,
    },
    DisableMirror {
        alias: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoOperationResult {
    Unchanged,
    Changed,
    Show(RepoConfigV2),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RepoCommandError {
    #[error(
        "sub-command 'modify' already has been deprecated, please use sub-command 'add' to add a remote repository and use it as default."
    )]
    ModifyDeprecated,
    #[error("url is empty.")]
    EmptyUrl,
    #[error("url is invalid: {0}")]
    InvalidUrl(String),
    #[error("repo {0} already exist")]
    AlreadyExists(String),
    #[error("the operated repo {0} doesn't exist")]
    NotFound(String),
    #[error(
        "repo {0} is the only repo, please add another repo before removing it or update it directly."
    )]
    OnlyRepository(String),
}

pub fn apply_repo_operation(
    config: &mut RepoConfigV2,
    operation: RepoOperation,
) -> Result<RepoOperationResult, RepoCommandError> {
    match operation {
        RepoOperation::Show => Ok(RepoOperationResult::Show(config.clone())),
        RepoOperation::Modify => Err(RepoCommandError::ModifyDeprecated),
        RepoOperation::Add { name, url, alias } => {
            let url = normalized_url(url)?;
            let effective_name = alias.as_deref().unwrap_or(&name);
            if config
                .repos
                .iter()
                .any(|repo| repo.effective_name() == effective_name)
            {
                return Err(RepoCommandError::AlreadyExists(effective_name.to_string()));
            }
            config.repos.push(Repo {
                alias,
                mirror_enabled: None,
                name,
                priority: 0,
                url,
            });
            Ok(RepoOperationResult::Changed)
        }
        operation => apply_existing_repo_operation(config, operation),
    }
}

fn apply_existing_repo_operation(
    config: &mut RepoConfigV2,
    operation: RepoOperation,
) -> Result<RepoOperationResult, RepoCommandError> {
    let alias = match &operation {
        RepoOperation::Remove { alias }
        | RepoOperation::Update { alias, .. }
        | RepoOperation::SetDefault { alias }
        | RepoOperation::SetPriority { alias, .. }
        | RepoOperation::EnableMirror { alias }
        | RepoOperation::DisableMirror { alias } => alias.clone(),
        RepoOperation::Add { .. } | RepoOperation::Modify | RepoOperation::Show => unreachable!(),
    };
    let index = config
        .repos
        .iter()
        .position(|repo| repo.effective_name() == alias)
        .ok_or_else(|| RepoCommandError::NotFound(alias.clone()))?;

    match operation {
        RepoOperation::Remove { .. } => {
            if config.repos.len() == 1 {
                return Err(RepoCommandError::OnlyRepository(alias));
            }
            config.repos.remove(index);
            if config.default_repo == alias {
                let maximum = config
                    .repos
                    .iter()
                    .map(|repo| repo.priority)
                    .max()
                    .unwrap_or(0);
                config.default_repo = config
                    .repos
                    .iter()
                    .find(|repo| repo.priority == maximum)
                    .expect("a non-empty repository list has a maximum")
                    .effective_name()
                    .to_string();
            }
            Ok(RepoOperationResult::Changed)
        }
        RepoOperation::Update { url, .. } => {
            config.repos[index].url = normalized_url(url)?;
            Ok(RepoOperationResult::Changed)
        }
        RepoOperation::EnableMirror { .. } => {
            config.repos[index].mirror_enabled = Some(true);
            Ok(RepoOperationResult::Changed)
        }
        RepoOperation::DisableMirror { .. } => {
            config.repos[index].mirror_enabled = Some(false);
            Ok(RepoOperationResult::Changed)
        }
        RepoOperation::SetDefault { .. } => {
            if config.default_repo == alias {
                return Ok(RepoOperationResult::Unchanged);
            }
            config.default_repo = alias;
            let maximum = config
                .repos
                .iter()
                .map(|repo| repo.priority)
                .max()
                .unwrap_or(0);
            config.repos[index].priority = maximum + 100;
            Ok(RepoOperationResult::Changed)
        }
        RepoOperation::SetPriority { priority, .. } => {
            config.repos[index].priority = priority;
            Ok(RepoOperationResult::Changed)
        }
        RepoOperation::Add { .. } | RepoOperation::Modify | RepoOperation::Show => unreachable!(),
    }
}

fn normalized_url(mut url: String) -> Result<String, RepoCommandError> {
    if url.is_empty() {
        return Err(RepoCommandError::EmptyUrl);
    }
    if !url.starts_with("http") {
        return Err(RepoCommandError::InvalidUrl(url));
    }
    if url.ends_with('/') {
        url.pop();
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RepoConfigV2 {
        RepoConfigV2 {
            default_repo: "stable".to_string(),
            repos: vec![
                Repo {
                    alias: Some("stable".to_string()),
                    mirror_enabled: Some(false),
                    name: "stable".to_string(),
                    priority: 10,
                    url: "https://stable.example".to_string(),
                },
                Repo {
                    alias: Some("beta".to_string()),
                    mirror_enabled: Some(true),
                    name: "beta".to_string(),
                    priority: 20,
                    url: "https://beta.example".to_string(),
                },
            ],
            version: 2,
        }
    }

    #[test]
    fn adds_repo_and_trims_one_trailing_slash() {
        let mut config = config();
        let result = apply_repo_operation(
            &mut config,
            RepoOperation::Add {
                name: "community".to_string(),
                url: "https://community.example/".to_string(),
                alias: Some("comm".to_string()),
            },
        )
        .unwrap();
        assert_eq!(result, RepoOperationResult::Changed);
        assert_eq!(config.repos[2].effective_name(), "comm");
        assert_eq!(config.repos[2].url, "https://community.example");
        assert_eq!(config.repos[2].priority, 0);
    }

    #[test]
    fn rejects_duplicate_alias() {
        let mut config = config();
        let error = apply_repo_operation(
            &mut config,
            RepoOperation::Add {
                name: "stable2".to_string(),
                url: "https://stable2.example".to_string(),
                alias: Some("stable".to_string()),
            },
        )
        .unwrap_err();
        assert_eq!(error, RepoCommandError::AlreadyExists("stable".to_string()));
        assert_eq!(config.repos.len(), 2);
    }

    #[test]
    fn updates_url() {
        let mut config = config();
        apply_repo_operation(
            &mut config,
            RepoOperation::Update {
                alias: "beta".to_string(),
                url: "https://new-beta.example/".to_string(),
            },
        )
        .unwrap();
        assert_eq!(config.repos[1].url, "https://new-beta.example");
    }

    #[test]
    fn removing_default_selects_first_highest_priority_repo() {
        let mut config = config();
        apply_repo_operation(
            &mut config,
            RepoOperation::Remove {
                alias: "stable".to_string(),
            },
        )
        .unwrap();
        assert_eq!(config.repos.len(), 1);
        assert_eq!(config.default_repo, "beta");
    }

    #[test]
    fn setting_default_raises_priority() {
        let mut config = config();
        apply_repo_operation(
            &mut config,
            RepoOperation::SetDefault {
                alias: "beta".to_string(),
            },
        )
        .unwrap();
        assert_eq!(config.default_repo, "beta");
        assert_eq!(config.repos[1].priority, 120);
    }

    #[test]
    fn sets_priority_and_mirror_state() {
        let mut config = config();
        apply_repo_operation(
            &mut config,
            RepoOperation::SetPriority {
                alias: "stable".to_string(),
                priority: 42,
            },
        )
        .unwrap();
        apply_repo_operation(
            &mut config,
            RepoOperation::EnableMirror {
                alias: "stable".to_string(),
            },
        )
        .unwrap();
        apply_repo_operation(
            &mut config,
            RepoOperation::DisableMirror {
                alias: "beta".to_string(),
            },
        )
        .unwrap();
        assert_eq!(config.repos[0].priority, 42);
        assert_eq!(config.repos[0].mirror_enabled, Some(true));
        assert_eq!(config.repos[1].mirror_enabled, Some(false));
    }

    #[test]
    fn show_and_deprecated_modify_do_not_change_config() {
        let mut config = config();
        assert_eq!(
            apply_repo_operation(&mut config, RepoOperation::Show).unwrap(),
            RepoOperationResult::Show(config.clone())
        );
        assert_eq!(
            apply_repo_operation(&mut config, RepoOperation::Modify).unwrap_err(),
            RepoCommandError::ModifyDeprecated
        );
    }
}
