use std::fs;
use std::path::{Path, PathBuf};

use linyaps_api::{
    RepoConfigV2, RepositoryCache, RepositoryCacheLayersItem, RepositoryCacheMergedItem,
};
use thiserror::Error;

pub const CACHE_VERSION: &str = "2";

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("failed to read repository cache: {0}")]
    Read(#[from] std::io::Error),
    #[error("failed to parse repository cache: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("cache version mismatch: cache version {actual}, expected version {expected}")]
    VersionMismatch {
        actual: String,
        expected: &'static str,
    },
    #[error("repository cache item already exists")]
    AlreadyExists,
    #[error("repository cache item does not exist")]
    NotFound,
}

#[derive(Clone, Debug)]
pub struct RepositoryCacheStore {
    path: PathBuf,
    data: RepositoryCache,
}

impl RepositoryCacheStore {
    pub fn empty(path: impl Into<PathBuf>, config: RepoConfigV2) -> Self {
        Self {
            path: path.into(),
            data: RepositoryCache {
                config,
                layers: Vec::new(),
                ll_version: env!("CARGO_PKG_VERSION").to_string(),
                merged: None,
                version: CACHE_VERSION.to_string(),
            },
        }
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Self, CacheError> {
        let path = path.into();
        let data: RepositoryCache = serde_json::from_slice(&fs::read(&path)?)?;
        if data.version != CACHE_VERSION {
            return Err(CacheError::VersionMismatch {
                actual: data.version,
                expected: CACHE_VERSION,
            });
        }
        Ok(Self { path, data })
    }

    pub fn data(&self) -> &RepositoryCache {
        &self.data
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn replace_config(&mut self, config: RepoConfigV2) {
        self.data.config = config;
    }

    pub fn replace_merged(
        &mut self,
        merged: Vec<RepositoryCacheMergedItem>,
    ) -> Result<(), CacheError> {
        let previous = self.data.merged.replace(merged);
        if let Err(error) = self.write_to_disk() {
            self.data.merged = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn write_to_disk(&self) -> Result<(), CacheError> {
        fs::write(&self.path, serde_json::to_vec(&self.data)?)?;
        Ok(())
    }

    pub fn add_layer(&mut self, item: RepositoryCacheLayersItem) -> Result<(), CacheError> {
        if self
            .data
            .layers
            .iter()
            .any(|current| same_layer(current, &item))
        {
            return Err(CacheError::AlreadyExists);
        }
        self.data.layers.push(item);
        self.write_to_disk()
    }

    pub fn upsert_layer(&mut self, item: RepositoryCacheLayersItem) -> Result<(), CacheError> {
        let previous = self.data.layers.clone();
        self.data.layers.retain(|current| {
            !same_layer_identity(current, &item)
                || (current.deleted == Some(true) && current.commit != item.commit)
        });
        self.data.layers.push(item);
        if let Err(error) = self.write_to_disk() {
            self.data.layers = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn delete_layer(&mut self, item: &RepositoryCacheLayersItem) -> Result<(), CacheError> {
        let index = self
            .data
            .layers
            .iter()
            .position(|current| same_layer(current, item))
            .ok_or(CacheError::NotFound)?;
        let removed = self.data.layers.remove(index);
        if let Err(error) = self.write_to_disk() {
            self.data.layers.insert(index, removed);
            return Err(error);
        }
        Ok(())
    }

    pub fn set_layer_deleted(
        &mut self,
        item: &RepositoryCacheLayersItem,
        deleted: bool,
    ) -> Result<(), CacheError> {
        let index = self
            .data
            .layers
            .iter()
            .position(|current| same_layer(current, item))
            .ok_or(CacheError::NotFound)?;
        let previous = self.data.layers[index].deleted;
        self.data.layers[index].deleted = deleted.then_some(true);
        if let Err(error) = self.write_to_disk() {
            self.data.layers[index].deleted = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn existing_layers(&self) -> Vec<&RepositoryCacheLayersItem> {
        self.data
            .layers
            .iter()
            .filter(|item| item.deleted != Some(true))
            .collect()
    }

    pub fn deleted_layers(&self) -> Vec<&RepositoryCacheLayersItem> {
        self.data
            .layers
            .iter()
            .filter(|item| item.deleted == Some(true))
            .collect()
    }
}

fn same_layer(left: &RepositoryCacheLayersItem, right: &RepositoryCacheLayersItem) -> bool {
    left.commit == right.commit
        && left.repo == right.repo
        && left.info.channel == right.info.channel
        && left.info.id == right.info.id
        && left.info.version == right.info.version
        && left.info.arch.first() == right.info.arch.first()
        && left.info.module == right.info.module
}

fn same_layer_identity(
    left: &RepositoryCacheLayersItem,
    right: &RepositoryCacheLayersItem,
) -> bool {
    left.info.channel == right.info.channel
        && left.info.id == right.info.id
        && left.info.version == right.info.version
        && left.info.arch.first() == right.info.arch.first()
        && left.info.module == right.info.module
}

#[cfg(test)]
mod tests {
    use linyaps_api::{PackageInfoV2, RepoConfigV2};
    use tempfile::tempdir;

    use super::*;

    fn info(id: &str) -> PackageInfoV2 {
        PackageInfoV2 {
            arch: vec!["x86_64".to_string()],
            base: String::new(),
            channel: "main".to_string(),
            command: None,
            compatible_version: None,
            description: None,
            extension_implementation: None,
            extensions: None,
            id: id.to_string(),
            kind: "app".to_string(),
            module: "binary".to_string(),
            name: id.to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 0,
            uuid: None,
            version: "1.0.0.0".to_string(),
        }
    }

    fn config() -> RepoConfigV2 {
        RepoConfigV2 {
            default_repo: String::new(),
            repos: Vec::new(),
            version: 2,
        }
    }

    #[test]
    fn persists_add_delete_and_deleted_filter() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("states.json");
        let mut store = RepositoryCacheStore::empty(&path, config());
        store.write_to_disk().unwrap();
        let live = RepositoryCacheLayersItem {
            commit: "live".to_string(),
            deleted: None,
            info: info("app.live"),
            repo: "local".to_string(),
        };
        let deleted = RepositoryCacheLayersItem {
            commit: "deleted".to_string(),
            deleted: Some(true),
            info: info("app.deleted"),
            repo: "local".to_string(),
        };
        store.add_layer(live.clone()).unwrap();
        store.add_layer(deleted).unwrap();

        let mut loaded = RepositoryCacheStore::load(&path).unwrap();
        assert_eq!(loaded.existing_layers().len(), 1);
        assert_eq!(loaded.existing_layers()[0].commit, "live");
        loaded.delete_layer(&live).unwrap();
        assert!(
            RepositoryCacheStore::load(path)
                .unwrap()
                .data()
                .layers
                .iter()
                .all(|item| item.commit != "live")
        );
    }

    #[test]
    fn rejects_cache_version_mismatch() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("states.json");
        fs::write(
            &path,
            r#"{"config":{"defaultRepo":"","repos":[],"version":2},"layers":[],"ll-version":"test","version":"1"}"#,
        )
        .unwrap();
        assert!(matches!(
            RepositoryCacheStore::load(path),
            Err(CacheError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn upsert_preserves_deferred_layer_with_a_different_commit() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("states.json");
        let mut store = RepositoryCacheStore::empty(&path, config());
        store.write_to_disk().unwrap();
        let old = RepositoryCacheLayersItem {
            commit: "old".to_string(),
            deleted: Some(true),
            info: info("app.demo"),
            repo: "old-remote".to_string(),
        };
        let mut new = old.clone();
        new.commit = "new".to_string();
        new.deleted = None;
        new.repo = "new-remote".to_string();
        store.add_layer(old.clone()).unwrap();
        store.upsert_layer(new.clone()).unwrap();

        assert_eq!(store.data().layers, vec![old.clone(), new]);
        assert_eq!(store.deleted_layers(), vec![&old]);

        store.set_layer_deleted(&old, false).unwrap();
        assert!(store.deleted_layers().is_empty());
    }
}
