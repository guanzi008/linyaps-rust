use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::os::fd::AsFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use futures_lite::AsyncReadExt;
use linyaps_api::{
    PackageInfoV2, RepoConfigV2, RepositoryCacheLayersItem, RepositoryCacheMergedItem,
};
use linyaps_core::repository::{RepositoryConfigError, load_config, save_config};
use linyaps_core::{Architecture, FuzzyReference, Reference, Version};
use ostrya::{
    CheckoutMode, CheckoutOptions, Checksum, CommitModifier, CommitModifierFlags, CommitOptions,
    CreateOptions, FileKind, FileMeta, LockKind, MutableTree, ObjectType, Repo, RepoMode,
    Transaction, TreeEntry,
};
use ostrya_core::sizes::unpack_entry;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::remote::ResolvedRemoteLayer;
use crate::{CacheError, RemoteError, RemoteRepositoryClient, RepositoryCacheStore};

const LOCAL_REMOTE: &str = "local";

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("repository root does not exist: {0}")]
    MissingRoot(PathBuf),
    #[error("layer source is not a directory: {0}")]
    InvalidLayerSource(PathBuf),
    #[error("layer has no architecture")]
    MissingArchitecture,
    #[error("commit does not contain info.json")]
    MissingPackageInfo,
    #[error("invalid remote commit metadata: {0}")]
    InvalidRemoteMetadata(String),
    #[error("invalid module name")]
    InvalidModule,
    #[error("package not found: {0}")]
    PackageNotFound(String),
    #[error("compatible layer not found: {0}")]
    CompatibleLayerNotFound(String),
    #[error("layer not found: {0}/{1}")]
    LayerNotFound(String, String),
    #[error("layer directory does not exist: {0}")]
    MissingLayerDirectory(PathBuf),
    #[error("failed to access repository files: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Config(#[from] RepositoryConfigError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error(transparent)]
    Ostree(#[from] ostrya::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Reference(#[from] linyaps_core::reference::ReferenceError),
    #[error(transparent)]
    Version(#[from] linyaps_core::version::VersionError),
    #[error(transparent)]
    Architecture(#[from] linyaps_core::architecture::ArchitectureError),
    #[error(transparent)]
    Remote(#[from] RemoteError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportedLayer {
    pub commit: String,
    pub path: PathBuf,
    pub info: PackageInfoV2,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteRefMetadata {
    pub commit: String,
    pub selected_ref: String,
    pub package_info: Option<PackageInfoV2>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefStatistics {
    pub archived: u64,
    pub unpacked: u64,
    pub objects: u64,
    pub needed_archived: u64,
    pub needed_unpacked: u64,
    pub needed_objects: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteFetchMode {
    CommitOnly,
    PackageInfo,
    Full,
}

#[derive(Debug)]
pub struct LocalRepository {
    root: PathBuf,
    config: RepoConfigV2,
    repo: Repo,
    cache: RepositoryCacheStore,
}

impl LocalRepository {
    pub async fn create(
        root: impl Into<PathBuf>,
        fallback_config: RepoConfigV2,
    ) -> Result<Self, RepositoryError> {
        let root = root.into();
        require_root(&root)?;
        let config_path = root.join("config.yaml");
        let config = if config_path.exists() {
            load_config(&config_path)?
        } else {
            save_config(&fallback_config, &config_path)?;
            fallback_config
        };
        let repo_path = root.join("repo");
        let repo = Repo::create(&repo_path, CreateOptions::new(RepoMode::BareUserOnly)).await?;
        let cache_path = root.join("states.json");
        let cache = match RepositoryCacheStore::load(&cache_path) {
            Ok(cache) => cache,
            Err(_) => rebuild_cache(&cache_path, config.clone(), &repo, &repo_path).await?,
        };
        Ok(Self {
            root,
            config,
            repo,
            cache,
        })
    }

    pub async fn open(root: impl Into<PathBuf>) -> Result<Self, RepositoryError> {
        let root = root.into();
        require_root(&root)?;
        let config = load_config(&root.join("config.yaml"))?;
        let repo = Repo::open(&root.join("repo")).await?;
        let cache = RepositoryCacheStore::load(root.join("states.json"))?;
        Ok(Self {
            root,
            config,
            repo,
            cache,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &RepoConfigV2 {
        &self.config
    }

    pub fn cache(&self) -> &RepositoryCacheStore {
        &self.cache
    }

    pub fn list_layer_items(&self) -> Vec<RepositoryCacheLayersItem> {
        self.cache.existing_layers().into_iter().cloned().collect()
    }

    pub fn list_deleted_layer_items(&self) -> Vec<RepositoryCacheLayersItem> {
        self.cache.deleted_layers().into_iter().cloned().collect()
    }

    pub fn resolve_local(
        &self,
        fuzzy: &FuzzyReference,
        semantic_matching: bool,
    ) -> Result<Reference, RepositoryError> {
        let mut candidates = self
            .cache
            .existing_layers()
            .into_iter()
            .filter(|item| {
                item.info.id == fuzzy.id
                    && fuzzy
                        .channel
                        .as_ref()
                        .is_none_or(|channel| channel == &item.info.channel)
                    && matches!(item.info.module.as_str(), "binary" | "runtime")
                    && fuzzy.architecture.is_none_or(|architecture| {
                        item.info.arch.first() == Some(&architecture.to_string())
                    })
            })
            .map(|item| Ok((Version::parse(&item.info.version)?, item)))
            .collect::<Result<Vec<_>, RepositoryError>>()?;
        if candidates.is_empty() {
            return Err(RepositoryError::PackageNotFound(fuzzy.to_string()));
        }
        candidates.sort_by(|(left, _), (right, _)| {
            right.partial_cmp(left).unwrap_or(std::cmp::Ordering::Equal)
        });

        let requested_version = fuzzy.version.as_deref().map(Version::parse).transpose()?;
        let item = candidates
            .into_iter()
            .find(|(version, _)| {
                requested_version.as_ref().is_none_or(|requested| {
                    (semantic_matching && version.semantic_match(&requested.to_string()))
                        || version == requested
                })
            })
            .map(|(_, item)| item)
            .ok_or_else(|| RepositoryError::CompatibleLayerNotFound(fuzzy.to_string()))?;
        reference_from_info(&item.info)
    }

    pub fn layer_item(
        &self,
        reference: &Reference,
        module: &str,
    ) -> Result<RepositoryCacheLayersItem, RepositoryError> {
        self.layer_item_with_deleted(reference, module, false)
    }

    fn layer_item_with_deleted(
        &self,
        reference: &Reference,
        module: &str,
        deleted: bool,
    ) -> Result<RepositoryCacheLayersItem, RepositoryError> {
        let module = if module == "runtime" {
            "binary"
        } else {
            module
        };
        let find = |candidate_module: &str| {
            self.cache.data().layers.iter().find(|item| {
                item.deleted == deleted.then_some(true)
                    && item.info.id == reference.id
                    && item.info.channel == reference.channel
                    && item.info.version == reference.version.to_string()
                    && item.info.arch.first() == Some(&reference.architecture.to_string())
                    && item.info.module == candidate_module
            })
        };
        find(module)
            .or_else(|| (module == "binary").then(|| find("runtime")).flatten())
            .cloned()
            .ok_or_else(|| {
                RepositoryError::LayerNotFound(reference.to_string(), module.to_string())
            })
    }

    pub fn mark_layer_deleted(
        &mut self,
        reference: &Reference,
        module: &str,
    ) -> Result<bool, RepositoryError> {
        let item = match self.layer_item_with_deleted(reference, module, false) {
            Ok(item) => item,
            Err(RepositoryError::LayerNotFound(_, _)) => return Ok(false),
            Err(error) => return Err(error),
        };
        self.cache.set_layer_deleted(&item, true)?;
        Ok(true)
    }

    pub fn restore_deleted_layer(
        &mut self,
        reference: &Reference,
        module: &str,
    ) -> Result<bool, RepositoryError> {
        let item = match self.layer_item_with_deleted(reference, module, true) {
            Ok(item) => item,
            Err(RepositoryError::LayerNotFound(_, _)) => return Ok(false),
            Err(error) => return Err(error),
        };
        self.cache.set_layer_deleted(&item, false)?;
        Ok(true)
    }

    pub fn layer_path_for_item(
        &self,
        item: &RepositoryCacheLayersItem,
    ) -> Result<PathBuf, RepositoryError> {
        let path = self.root.join("layers").join(&item.commit);
        if !path.exists() {
            return Err(RepositoryError::MissingLayerDirectory(path));
        }
        Ok(path)
    }

    pub fn layer_path(
        &self,
        reference: &Reference,
        module: &str,
    ) -> Result<PathBuf, RepositoryError> {
        let item = self.layer_item(reference, module)?;
        self.layer_path_for_item(&item)
    }

    pub fn merged_layer_path(&self, reference: &Reference) -> Result<PathBuf, RepositoryError> {
        let item = self.layer_item(reference, "binary")?;
        if let Some(merged) = self.cache.data().merged.as_ref().and_then(|items| {
            items
                .iter()
                .find(|merged| merged.binary_commit.as_deref() == Some(&item.commit))
        }) {
            let path = self.root.join("merged").join(&merged.id);
            if path.is_dir() {
                return Ok(path);
            }
        }
        self.layer_path_for_item(&item)
    }

    pub fn read_layer_info(
        &self,
        reference: &Reference,
        module: &str,
    ) -> Result<PackageInfoV2, RepositoryError> {
        let path = self.layer_path(reference, module)?.join("info.json");
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    pub fn layer_create_time(
        &self,
        item: &RepositoryCacheLayersItem,
    ) -> Result<Option<i64>, RepositoryError> {
        let metadata = fs::metadata(self.layer_path_for_item(item)?)?;
        let timestamp = metadata
            .created()
            .ok()
            .and_then(|created| created.duration_since(UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_secs()).ok());
        Ok(timestamp)
    }

    pub fn update_config(&mut self, config: RepoConfigV2) -> Result<(), RepositoryError> {
        let old_config = self.config.clone();
        save_config(&config, &self.root.join("config.yaml"))?;
        self.cache.replace_config(config.clone());
        if let Err(error) = self.cache.write_to_disk() {
            self.cache.replace_config(old_config.clone());
            let _ = save_config(&old_config, &self.root.join("config.yaml"));
            return Err(error.into());
        }
        self.config = config;
        Ok(())
    }

    pub async fn import_layer_dir(
        &mut self,
        directory: &Path,
        overlays: &[PathBuf],
        sub_ref: Option<&str>,
    ) -> Result<ImportedLayer, RepositoryError> {
        let info: PackageInfoV2 = serde_json::from_slice(&fs::read(directory.join("info.json"))?)?;
        let reference = reference_from_info(&info)?;
        let mut directories = Vec::with_capacity(overlays.len() + 1);
        directories.push(directory.to_path_buf());
        directories.extend_from_slice(overlays);
        self.import_directories(&reference, &info, &directories, sub_ref)
            .await
    }

    pub async fn import_directories(
        &mut self,
        reference: &Reference,
        info: &PackageInfoV2,
        directories: &[PathBuf],
        sub_ref: Option<&str>,
    ) -> Result<ImportedLayer, RepositoryError> {
        self.import_directories_from(reference, info, directories, sub_ref, LOCAL_REMOTE)
            .await
    }

    async fn import_directories_from(
        &mut self,
        reference: &Reference,
        info: &PackageInfoV2,
        directories: &[PathBuf],
        sub_ref: Option<&str>,
        source: &str,
    ) -> Result<ImportedLayer, RepositoryError> {
        if info.module.is_empty() {
            return Err(RepositoryError::InvalidModule);
        }
        for directory in directories {
            if !directory.is_dir() {
                return Err(RepositoryError::InvalidLayerSource(directory.clone()));
            }
        }

        let transaction = self.repo.transaction().await?;
        let mut tree = MutableTree::new();
        let mut modifier = CommitModifier::new(CommitModifierFlags::CANONICAL_PERMISSIONS);
        for directory in directories {
            let source = fs::File::open(directory)?;
            transaction
                .write_dfd_to_mtree(
                    source.as_fd(),
                    Path::new("."),
                    &mut tree,
                    Some(&mut modifier),
                )
                .await?;
        }
        let root = transaction.write_mtree(&mut tree).await?;
        let commit = transaction
            .write_commit(CommitOptions::default(), &root)
            .await?;
        let ref_name = ostree_ref(reference, &info.module, sub_ref)?;
        transaction.set_ref(&format!("{source}:{ref_name}"), Some(&commit));
        transaction.commit().await?;

        let path = self.checkout_layer(&commit).await?;
        let item = RepositoryCacheLayersItem {
            commit: commit.to_hex(),
            deleted: None,
            info: info.clone(),
            repo: source.to_string(),
        };
        self.cache.upsert_layer(item)?;
        Ok(ImportedLayer {
            commit: commit.to_hex(),
            path,
            info: info.clone(),
        })
    }

    pub async fn pull(
        &mut self,
        reference: &Reference,
        remote: &linyaps_api::Repo,
        module: &str,
    ) -> Result<ImportedLayer, RepositoryError> {
        self.pull_with_progress(reference, remote, module, |_| {})
            .await
    }

    pub async fn fetch_remote_metadata(
        &self,
        reference: &Reference,
        remote: &linyaps_api::Repo,
        module: &str,
        fetch_package_info: bool,
    ) -> Result<RemoteRefMetadata, RepositoryError> {
        let work = self.root.join("tmp").join(format!(
            "metadata-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&work)?;
        let result = async {
            let client = RemoteRepositoryClient::new(&remote.url)?;
            let resolved = client.resolve_layer(reference, remote, module).await?;
            let archive_path = work.join("archive");
            let archive =
                Repo::create(&archive_path, CreateOptions::new(RepoMode::Archive)).await?;
            let mut ignore_progress = |_: u64| {};
            self.transfer_remote_commit(
                &client,
                remote,
                &resolved,
                &archive,
                &archive_path,
                if fetch_package_info {
                    RemoteFetchMode::PackageInfo
                } else {
                    RemoteFetchMode::CommitOnly
                },
                None,
                &mut ignore_progress,
            )
            .await?;
            let package_info = if fetch_package_info {
                Some(self.read_package_info(&resolved.commit).await?)
            } else {
                None
            };
            Ok(RemoteRefMetadata {
                commit: resolved.commit.to_hex(),
                selected_ref: resolved.selected_ref,
                package_info,
            })
        }
        .await;
        let _ = clear_path(&work);
        result
    }

    pub async fn get_ref_statistics(
        &self,
        metadata: &RemoteRefMetadata,
    ) -> Result<RefStatistics, RepositoryError> {
        let checksum = Checksum::from_hex(&metadata.commit).map_err(ostrya::Error::from)?;
        let (commit, _) = self.repo.load_commit(&checksum).await?;
        let Some(values) = commit
            .metadata_value("ostree.sizes")
            .and_then(ostrya::Value::as_array)
        else {
            return Ok(RefStatistics::default());
        };

        let mut statistics = RefStatistics::default();
        for value in values {
            let bytes = value.as_bytes().ok_or_else(|| {
                RepositoryError::InvalidRemoteMetadata(
                    "ostree.sizes contains a non-byte-array entry".to_string(),
                )
            })?;
            let entry = unpack_entry(bytes).map_err(|error| {
                RepositoryError::InvalidRemoteMetadata(format!(
                    "invalid ostree.sizes entry: {error}"
                ))
            })?;
            statistics.archived = statistics.archived.saturating_add(entry.compressed);
            statistics.unpacked = statistics.unpacked.saturating_add(entry.unpacked);
            statistics.objects = statistics.objects.saturating_add(1);
            if !self
                .has_size_entry_object(&entry.checksum, entry.objtype)
                .await?
            {
                statistics.needed_archived =
                    statistics.needed_archived.saturating_add(entry.compressed);
                statistics.needed_unpacked =
                    statistics.needed_unpacked.saturating_add(entry.unpacked);
                statistics.needed_objects = statistics.needed_objects.saturating_add(1);
            }
        }
        Ok(statistics)
    }

    pub async fn pull_with_progress<F>(
        &mut self,
        reference: &Reference,
        remote: &linyaps_api::Repo,
        module: &str,
        mut progress: F,
    ) -> Result<ImportedLayer, RepositoryError>
    where
        F: FnMut(u64) + Send,
    {
        let work =
            self.root
                .join("tmp")
                .join(format!("pull-{}-{}", std::process::id(), unique_suffix()));
        fs::create_dir_all(&work)?;
        let result = async {
            let client = RemoteRepositoryClient::new(&remote.url)?;
            let resolved = client.resolve_layer(reference, remote, module).await?;
            let archive_path = work.join("archive");
            let archive =
                Repo::create(&archive_path, CreateOptions::new(RepoMode::Archive)).await?;
            let refspec = format!("{}:{}", remote.effective_name(), resolved.selected_ref);
            self.transfer_remote_commit(
                &client,
                remote,
                &resolved,
                &archive,
                &archive_path,
                RemoteFetchMode::Full,
                Some(&refspec),
                &mut progress,
            )
            .await?;
            let info = self.read_package_info(&resolved.commit).await?;
            if info.module.is_empty() {
                return Err(RepositoryError::InvalidModule);
            }
            let path = self.checkout_layer(&resolved.commit).await?;
            self.cache.upsert_layer(RepositoryCacheLayersItem {
                commit: resolved.commit.to_hex(),
                deleted: None,
                info: info.clone(),
                repo: remote.effective_name().to_string(),
            })?;
            Ok(ImportedLayer {
                commit: resolved.commit.to_hex(),
                path,
                info,
            })
        }
        .await;
        let _ = clear_path(&work);
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn transfer_remote_commit(
        &self,
        client: &RemoteRepositoryClient,
        remote: &linyaps_api::Repo,
        resolved: &ResolvedRemoteLayer,
        archive: &Repo,
        archive_path: &Path,
        mode: RemoteFetchMode,
        refspec: Option<&str>,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<(), RepositoryError> {
        let transaction = self.repo.transaction().await?;
        let commit_bytes = self
            .load_or_fetch_remote_metadata(
                client,
                remote,
                archive_path,
                &transaction,
                &resolved.commit,
                ObjectType::Commit,
                progress,
            )
            .await?;
        let commit = ostrya::Commit::parse(&commit_bytes).map_err(ostrya::Error::from)?;

        match mode {
            RemoteFetchMode::CommitOnly => {}
            RemoteFetchMode::PackageInfo => {
                self.load_or_fetch_remote_metadata(
                    client,
                    remote,
                    archive_path,
                    &transaction,
                    &commit.root_dirmeta,
                    ObjectType::DirMeta,
                    progress,
                )
                .await?;
                let tree_bytes = self
                    .load_or_fetch_remote_metadata(
                        client,
                        remote,
                        archive_path,
                        &transaction,
                        &commit.root_dirtree,
                        ObjectType::DirTree,
                        progress,
                    )
                    .await?;
                let tree = ostrya::DirTree::parse(&tree_bytes).map_err(ostrya::Error::from)?;
                let info_checksum = tree
                    .files
                    .iter()
                    .find_map(|(name, checksum)| (name == "info.json").then_some(*checksum))
                    .ok_or(RepositoryError::MissingPackageInfo)?;
                self.fetch_remote_file(
                    client,
                    remote,
                    archive,
                    archive_path,
                    &transaction,
                    &info_checksum,
                    progress,
                )
                .await?;
            }
            RemoteFetchMode::Full => {
                let mut pending = VecDeque::from([(commit.root_dirtree, commit.root_dirmeta)]);
                let mut seen_trees = BTreeSet::new();
                let mut seen_metadata = BTreeSet::new();
                let mut files = BTreeSet::new();
                while let Some((tree_checksum, metadata_checksum)) = pending.pop_front() {
                    if seen_metadata.insert(metadata_checksum) {
                        self.load_or_fetch_remote_metadata(
                            client,
                            remote,
                            archive_path,
                            &transaction,
                            &metadata_checksum,
                            ObjectType::DirMeta,
                            progress,
                        )
                        .await?;
                    }
                    if !seen_trees.insert(tree_checksum) {
                        continue;
                    }
                    let tree_bytes = self
                        .load_or_fetch_remote_metadata(
                            client,
                            remote,
                            archive_path,
                            &transaction,
                            &tree_checksum,
                            ObjectType::DirTree,
                            progress,
                        )
                        .await?;
                    let tree = ostrya::DirTree::parse(&tree_bytes).map_err(ostrya::Error::from)?;
                    files.extend(tree.files.into_iter().map(|(_, checksum)| checksum));
                    pending.extend(
                        tree.dirs
                            .into_iter()
                            .map(|(_, tree, metadata)| (tree, metadata)),
                    );
                }
                for checksum in files {
                    self.fetch_remote_file(
                        client,
                        remote,
                        archive,
                        archive_path,
                        &transaction,
                        &checksum,
                        progress,
                    )
                    .await?;
                }
            }
        }

        if let Some(refspec) = refspec {
            transaction.set_ref(refspec, Some(&resolved.commit));
        }
        transaction.commit().await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn load_or_fetch_remote_metadata(
        &self,
        client: &RemoteRepositoryClient,
        remote: &linyaps_api::Repo,
        archive_path: &Path,
        transaction: &Transaction,
        checksum: &Checksum,
        object_type: ObjectType,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<Vec<u8>, RepositoryError> {
        if self.repo.has_object(object_type, checksum).await? {
            return Ok(self.repo.load_object_bytes(object_type, checksum).await?);
        }
        let bytes = client
            .fetch_metadata_object(
                remote,
                archive_path,
                checksum,
                object_type,
                Some(&mut *progress),
            )
            .await?;
        transaction
            .write_metadata(object_type, Some(checksum), &bytes)
            .await?;
        Ok(bytes)
    }

    #[allow(clippy::too_many_arguments)]
    async fn fetch_remote_file(
        &self,
        client: &RemoteRepositoryClient,
        remote: &linyaps_api::Repo,
        archive: &Repo,
        archive_path: &Path,
        transaction: &Transaction,
        checksum: &Checksum,
        progress: &mut (dyn FnMut(u64) + Send),
    ) -> Result<(), RepositoryError> {
        if self.repo.has_object(ObjectType::File, checksum).await? {
            return Ok(());
        }
        client
            .fetch_file_object(remote, archive_path, checksum, Some(&mut *progress))
            .await?;
        let file = archive.load_file(checksum).await?;
        let metadata = FileMeta {
            uid: file.uid,
            gid: file.gid,
            mode: file.mode,
            xattrs: file.xattrs.clone(),
        };
        match &file.kind {
            FileKind::Regular { .. } => {
                transaction
                    .write_content(Some(checksum), &metadata, file.reader().await?)
                    .await?;
            }
            FileKind::Symlink { target } => {
                transaction
                    .write_symlink(target, &metadata, Some(checksum))
                    .await?;
            }
        }
        Ok(())
    }

    async fn read_package_info(
        &self,
        commit_checksum: &Checksum,
    ) -> Result<PackageInfoV2, RepositoryError> {
        let (commit, _) = self.repo.load_commit(commit_checksum).await?;
        let tree = self.repo.load_dirtree(&commit.root_dirtree).await?;
        let checksum = tree
            .files
            .iter()
            .find_map(|(name, checksum)| (name == "info.json").then_some(*checksum))
            .ok_or(RepositoryError::MissingPackageInfo)?;
        let file = self.repo.load_file(&checksum).await?;
        if file.is_symlink() {
            return Err(RepositoryError::MissingPackageInfo);
        }
        let mut reader = file.reader().await?;
        let mut content = Vec::new();
        reader.read_to_end(&mut content).await?;
        Ok(serde_json::from_slice(&content)?)
    }

    async fn has_size_entry_object(
        &self,
        checksum: &Checksum,
        object_type: Option<ObjectType>,
    ) -> Result<bool, RepositoryError> {
        if let Some(object_type) = object_type {
            return Ok(self.repo.has_object(object_type, checksum).await?);
        }
        for object_type in [
            ObjectType::File,
            ObjectType::DirTree,
            ObjectType::DirMeta,
            ObjectType::Commit,
            ObjectType::TombstoneCommit,
            ObjectType::CommitMeta,
            ObjectType::PayloadLink,
            ObjectType::FileXattrs,
            ObjectType::FileXattrsLink,
        ] {
            if self.repo.has_object(object_type, checksum).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[cfg(test)]
    async fn import_archive_commit(
        &mut self,
        reference: &Reference,
        info: &PackageInfoV2,
        source: &Repo,
        commit_checksum: &Checksum,
        remote: &str,
    ) -> Result<ImportedLayer, RepositoryError> {
        if info.module.is_empty() {
            return Err(RepositoryError::InvalidModule);
        }

        let commit_bytes = source
            .load_object_bytes(ObjectType::Commit, commit_checksum)
            .await?;
        let commit = ostrya::Commit::parse(&commit_bytes).map_err(ostrya::Error::from)?;
        let transaction = self.repo.transaction().await?;
        transaction
            .write_metadata(ObjectType::Commit, Some(commit_checksum), &commit_bytes)
            .await?;

        let mut pending = VecDeque::from([(commit.root_dirtree, commit.root_dirmeta)]);
        let mut seen_trees = BTreeSet::new();
        let mut seen_metadata = BTreeSet::new();
        let mut files = BTreeSet::new();
        while let Some((tree_checksum, metadata_checksum)) = pending.pop_front() {
            if seen_metadata.insert(metadata_checksum) {
                let bytes = source
                    .load_object_bytes(ObjectType::DirMeta, &metadata_checksum)
                    .await?;
                transaction
                    .write_metadata(ObjectType::DirMeta, Some(&metadata_checksum), &bytes)
                    .await?;
            }
            if !seen_trees.insert(tree_checksum) {
                continue;
            }
            let bytes = source
                .load_object_bytes(ObjectType::DirTree, &tree_checksum)
                .await?;
            let tree = ostrya::DirTree::parse(&bytes).map_err(ostrya::Error::from)?;
            transaction
                .write_metadata(ObjectType::DirTree, Some(&tree_checksum), &bytes)
                .await?;
            files.extend(tree.files.into_iter().map(|(_, checksum)| checksum));
            pending.extend(
                tree.dirs
                    .into_iter()
                    .map(|(_, tree, metadata)| (tree, metadata)),
            );
        }

        for checksum in files {
            let file = source.load_file(&checksum).await?;
            let metadata = FileMeta {
                uid: file.uid,
                gid: file.gid,
                mode: file.mode,
                xattrs: file.xattrs.clone(),
            };
            match &file.kind {
                FileKind::Regular { .. } => {
                    transaction
                        .write_content(Some(&checksum), &metadata, file.reader().await?)
                        .await?;
                }
                FileKind::Symlink { target } => {
                    transaction
                        .write_symlink(target, &metadata, Some(&checksum))
                        .await?;
                }
            }
        }

        let ref_name = ostree_ref(reference, &info.module, None)?;
        transaction.set_ref(&format!("{remote}:{ref_name}"), Some(commit_checksum));
        transaction.commit().await?;

        let path = self.checkout_layer(commit_checksum).await?;
        self.cache.upsert_layer(RepositoryCacheLayersItem {
            commit: commit_checksum.to_hex(),
            deleted: None,
            info: info.clone(),
            repo: remote.to_string(),
        })?;
        Ok(ImportedLayer {
            commit: commit_checksum.to_hex(),
            path,
            info: info.clone(),
        })
    }

    pub fn module_list(&self, reference: &Reference) -> Vec<String> {
        let mut modules = self
            .cache
            .existing_layers()
            .into_iter()
            .filter(|item| {
                item.info.id == reference.id
                    && item.info.channel == reference.channel
                    && item.info.version == reference.version.to_string()
                    && item.info.arch.first() == Some(&reference.architecture.to_string())
            })
            .map(|item| item.info.module.clone())
            .collect::<Vec<_>>();
        modules.sort();
        modules.dedup();
        modules
    }

    pub async fn remove_layer(
        &mut self,
        reference: &Reference,
        module: &str,
    ) -> Result<bool, RepositoryError> {
        self.remove_layer_with_sub_ref(reference, module, None)
            .await
    }

    pub async fn remove_layer_with_sub_ref(
        &mut self,
        reference: &Reference,
        module: &str,
        sub_ref: Option<&str>,
    ) -> Result<bool, RepositoryError> {
        let item = match self.layer_item(reference, module) {
            Ok(item) => item,
            Err(RepositoryError::LayerNotFound(_, _)) => return Ok(false),
            Err(error) => return Err(error),
        };
        self.remove_layer_item_with_sub_ref(&item, sub_ref).await
    }

    pub async fn remove_layer_item(
        &mut self,
        item: &RepositoryCacheLayersItem,
    ) -> Result<bool, RepositoryError> {
        self.remove_layer_item_with_sub_ref(item, None).await
    }

    pub async fn remove_layer_item_with_sub_ref(
        &mut self,
        item: &RepositoryCacheLayersItem,
        sub_ref: Option<&str>,
    ) -> Result<bool, RepositoryError> {
        let reference = reference_from_info(&item.info)?;
        let ref_name = ostree_ref(&reference, &item.info.module, sub_ref)?;
        let refspec = format!("{}:{ref_name}", item.repo);
        let actual = self.repo.resolve_rev(&refspec, true).await?;
        let unset = actual
            .as_ref()
            .is_some_and(|actual| actual.to_hex() == item.commit);
        if unset {
            self.repo.set_ref_immediate(&refspec, None).await?;
        }
        if let Err(error) = self.cache.delete_layer(item) {
            if unset {
                let _ = self.repo.set_ref_immediate(&refspec, actual.as_ref()).await;
            }
            return Err(error.into());
        }
        if !self
            .cache
            .data()
            .layers
            .iter()
            .any(|remaining| remaining.commit == item.commit)
        {
            let _ = clear_path(&self.root.join("layers").join(&item.commit));
        }
        Ok(true)
    }

    pub async fn prune_objects(&self) -> Result<(), RepositoryError> {
        let transaction = self.repo.transaction_with_lock(LockKind::Exclusive).await?;
        let result = self.prune_objects_locked().await;
        let unlock = transaction.abort().await;
        match result {
            Err(error) => Err(error),
            Ok(()) => {
                unlock?;
                Ok(())
            }
        }
    }

    pub async fn clean_unreferenced(&self) -> Result<(), RepositoryError> {
        let reserved = self
            .cache
            .existing_layers()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        self.clean(&reserved).await
    }

    pub async fn clean(
        &self,
        reserved: &[RepositoryCacheLayersItem],
    ) -> Result<(), RepositoryError> {
        let reserved = reserved
            .iter()
            .map(|item| item.commit.as_str())
            .collect::<BTreeSet<_>>();
        for (refspec, commit) in repository_refspecs(&self.root.join("repo"))? {
            if reserved.contains(commit.to_hex().as_str()) {
                continue;
            }
            let _ = clear_path(&self.root.join("layers").join(commit.to_hex()));
            self.repo.set_ref_immediate(&refspec, None).await?;
        }
        self.prune_objects().await
    }

    async fn prune_objects_locked(&self) -> Result<(), RepositoryError> {
        let mode = self.repo.mode();
        let mut reachable = BTreeSet::new();
        let mut commits = VecDeque::new();
        for (_, commit) in repository_refspecs(&self.root.join("repo"))? {
            commits.push_back(commit);
        }

        let mut seen_commits = BTreeSet::new();
        let mut seen_trees = BTreeSet::new();
        while let Some(checksum) = commits.pop_front() {
            if !seen_commits.insert(checksum.to_hex()) {
                continue;
            }
            mark_object(&mut reachable, &checksum, ObjectType::Commit, mode);
            mark_object(&mut reachable, &checksum, ObjectType::CommitMeta, mode);
            let (commit, _) = self.repo.load_commit(&checksum).await?;
            for (_, related) in &commit.related {
                if let Ok(bytes) = <[u8; 32]>::try_from(related.as_slice()) {
                    commits.push_back(Checksum::from_bytes(bytes));
                }
            }

            let mut trees = vec![(commit.root_dirtree, commit.root_dirmeta)];
            while let Some((tree_checksum, metadata_checksum)) = trees.pop() {
                mark_object(
                    &mut reachable,
                    &metadata_checksum,
                    ObjectType::DirMeta,
                    mode,
                );
                mark_object(&mut reachable, &tree_checksum, ObjectType::DirTree, mode);
                if !seen_trees.insert(tree_checksum.to_hex()) {
                    continue;
                }
                let tree = self.repo.load_dirtree(&tree_checksum).await?;
                for (_, file_checksum) in tree.files {
                    mark_object(&mut reachable, &file_checksum, ObjectType::File, mode);
                    if mode == RepoMode::BareSplitXattrs {
                        mark_object(
                            &mut reachable,
                            &file_checksum,
                            ObjectType::FileXattrsLink,
                            mode,
                        );
                    }
                }
                for (_, child_tree, child_metadata) in tree.dirs {
                    trees.push((child_tree, child_metadata));
                }
            }
        }

        let objects = self.root.join("repo/objects");
        for fanout in fs::read_dir(&objects)? {
            let fanout = fanout?;
            if !fanout.file_type()?.is_dir() {
                continue;
            }
            let prefix = fanout.file_name().to_string_lossy().into_owned();
            if prefix.len() != 2 || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                continue;
            }
            for object in fs::read_dir(fanout.path())? {
                let object = object?;
                let name = object.file_name().to_string_lossy().into_owned();
                if !is_loose_object_name(&name) {
                    continue;
                }
                let relative = format!("{prefix}/{name}");
                if !reachable.contains(&relative) {
                    fs::remove_file(object.path())?;
                }
            }
        }
        Ok(())
    }

    pub fn export_reference(&self, reference: &Reference) -> Result<(), RepositoryError> {
        let item = self.layer_item(reference, "binary")?;
        let layer = self.layer_path_for_item(&item)?;
        let source_root = layer.join("entries");
        if !source_root.is_dir() {
            return Ok(());
        }
        let destination_root = self.root.join("entries");
        fs::create_dir_all(&destination_root)?;
        for relative in export_paths(&source_root) {
            let source = source_root.join(&relative);
            if !source.exists() {
                continue;
            }
            let destination = if relative == Path::new("share/systemd/user") {
                destination_root.join("lib/systemd/user")
            } else if relative == Path::new("share/deepin-elf-verify") {
                destination_root.join(&relative).join(&item.commit)
            } else {
                destination_root.join(&relative)
            };
            export_tree(&source, &destination, 10, &item.info.id, &destination_root)?;
        }
        update_shared_info(&destination_root);
        Ok(())
    }

    pub fn unexport_reference(&self, reference: &Reference) -> Result<(), RepositoryError> {
        let layer = self.layer_path(reference, "binary")?;
        let entries = self.root.join("entries");
        if !entries.is_dir() {
            return Ok(());
        }
        remove_links_into(&entries, &layer)?;
        update_shared_info(&entries);
        remove_empty_directories(&entries)?;
        Ok(())
    }

    pub fn merge_modules(&mut self) -> Result<(), RepositoryError> {
        let mut groups =
            std::collections::BTreeMap::<String, Vec<RepositoryCacheLayersItem>>::new();
        for item in self.list_layer_items() {
            let architecture = item.info.arch.first().cloned().unwrap_or_default();
            groups
                .entry(format!(
                    "{}/{}/{}",
                    item.info.id, item.info.version, architecture
                ))
                .or_default()
                .push(item);
        }

        let merged_root = self.root.join("merged");
        fs::create_dir_all(&merged_root)?;
        let previous = self.cache.data().merged.clone().unwrap_or_default();
        let mut merged_items = Vec::new();
        for (name, mut layers) in groups {
            if layers.len() < 2 {
                continue;
            }
            layers.sort_by(|left, right| left.info.module.cmp(&right.info.module));
            let Some(binary_commit) = layers
                .iter()
                .find(|item| matches!(item.info.module.as_str(), "binary" | "runtime"))
                .map(|item| item.commit.clone())
            else {
                continue;
            };
            let commits = layers
                .iter()
                .map(|item| item.commit.clone())
                .collect::<Vec<_>>();
            let modules = layers
                .iter()
                .map(|item| item.info.module.clone())
                .collect::<Vec<_>>();
            let mut hasher = Sha256::new();
            for commit in &commits {
                hasher.update(commit.as_bytes());
            }
            let id = linyaps_core::hex_encode(hasher.finalize());
            let item = RepositoryCacheMergedItem {
                binary_commit: Some(binary_commit),
                commits: commits.clone(),
                id: id.clone(),
                modules,
                name: Some(name),
            };
            if previous
                .iter()
                .any(|current| current.id == id && current.commits == commits)
                && merged_root.join(&id).is_dir()
            {
                merged_items.push(item);
                continue;
            }

            let temporary = merged_root.join(format!("tmp_{id}"));
            clear_path(&temporary)?;
            fs::create_dir_all(&temporary)?;
            for layer in &layers {
                overlay_tree(&self.root.join("layers").join(&layer.commit), &temporary)?;
            }
            let destination = merged_root.join(&id);
            clear_path(&destination)?;
            fs::rename(temporary, destination)?;
            merged_items.push(item);
        }
        self.cache.replace_merged(merged_items.clone())?;
        for entry in fs::read_dir(&merged_root)? {
            let entry = entry?;
            if !merged_items
                .iter()
                .any(|item| entry.file_name() == std::ffi::OsStr::new(&item.id))
            {
                clear_path(&entry.path())?;
            }
        }
        Ok(())
    }

    pub async fn remove_ref(
        &self,
        reference: &Reference,
        module: &str,
        sub_ref: Option<&str>,
        expected_commit: &str,
    ) -> Result<bool, RepositoryError> {
        let ref_name = ostree_ref(reference, module, sub_ref)?;
        let refspec = format!("{LOCAL_REMOTE}:{ref_name}");
        let Some(actual) = self.repo.resolve_rev(&refspec, true).await? else {
            return Ok(false);
        };
        if actual.to_hex() != expected_commit {
            return Ok(false);
        }
        self.repo.set_ref_immediate(&refspec, None).await?;
        Ok(true)
    }

    async fn checkout_layer(&self, commit: &ostrya::Checksum) -> Result<PathBuf, RepositoryError> {
        let layers = self.root.join("layers");
        fs::create_dir_all(&layers)?;
        let destination = layers.join(commit.to_hex());
        clear_path(&destination)?;
        let layers_fd = fs::File::open(&layers)?;
        let mut options = CheckoutOptions::new(CheckoutMode::None);
        self.repo
            .checkout_at(
                &mut options,
                layers_fd.as_fd(),
                Path::new(&commit.to_hex()),
                commit,
            )
            .await?;
        Ok(destination)
    }
}

fn mark_object(
    reachable: &mut BTreeSet<String>,
    checksum: &Checksum,
    object_type: ObjectType,
    mode: RepoMode,
) {
    let hex = checksum.to_hex();
    reachable.insert(format!(
        "{}/{}.{}",
        &hex[..2],
        &hex[2..],
        object_type.extension(mode)
    ));
}

fn is_loose_object_name(name: &str) -> bool {
    let Some((checksum, extension)) = name.split_once('.') else {
        return false;
    };
    checksum.len() == 62
        && checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
        && ObjectType::from_extension(extension).is_some()
}

pub fn ostree_ref(
    reference: &Reference,
    module: &str,
    sub_ref: Option<&str>,
) -> Result<String, RepositoryError> {
    if module.is_empty() || module.contains('/') || module.contains(':') {
        return Err(RepositoryError::InvalidModule);
    }
    let mut value = format!(
        "{}/{}/{}/{}/{}",
        reference.channel, reference.id, reference.version, reference.architecture, module
    );
    if let Some(sub_ref) = sub_ref {
        if sub_ref.is_empty() || sub_ref.contains('/') || sub_ref.contains(':') {
            return Err(RepositoryError::InvalidModule);
        }
        value.push('_');
        value.push_str(sub_ref);
    }
    Ok(value)
}

pub fn reference_from_info(info: &PackageInfoV2) -> Result<Reference, RepositoryError> {
    let architecture = info
        .arch
        .first()
        .ok_or(RepositoryError::MissingArchitecture)?
        .parse::<Architecture>()?;
    Ok(Reference::new(
        &info.channel,
        &info.id,
        Version::parse(&info.version)?,
        architecture,
    )?)
}

fn export_paths(source_root: &Path) -> Vec<PathBuf> {
    let mut paths = [
        "share/applications",
        "share/mime",
        "share/icons",
        "share/dbus-1",
        "share/gnome-shell",
        "share/appdata",
        "share/metainfo",
        "share/plugins",
        "share/templates",
        "share/deepin-elf-verify",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect::<Vec<_>>();
    if source_root.join("lib/systemd/user").is_dir() {
        paths.push(PathBuf::from("lib/systemd/user"));
    } else {
        paths.push(PathBuf::from("share/systemd/user"));
    }
    paths
}

fn export_tree(
    source: &Path,
    destination: &Path,
    depth: usize,
    app_id: &str,
    entries_root: &Path,
) -> Result<(), std::io::Error> {
    if depth == 0 {
        return Ok(());
    }
    let metadata = fs::metadata(source)?;
    if metadata.is_file() {
        if destination
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".linyaps.original"))
        {
            return Ok(());
        }
        rewrite_export_file(source, destination, app_id)?;
        let destinations = desktop_destinations(destination, entries_root);
        for destination in destinations {
            let Some(parent) = destination.parent() else {
                continue;
            };
            fs::create_dir_all(parent)?;
            if fs::symlink_metadata(&destination).is_ok() {
                if fs::symlink_metadata(&destination)?.file_type().is_symlink() {
                    fs::remove_file(&destination)?;
                } else {
                    continue;
                }
            }
            let target = relative_path(parent, source);
            symlink(target, destination)?;
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        export_tree(
            &entry.path(),
            &destination.join(entry.file_name()),
            depth - 1,
            app_id,
            entries_root,
        )?;
    }
    Ok(())
}

fn desktop_destinations(destination: &Path, entries_root: &Path) -> Vec<PathBuf> {
    if destination.extension() != Some(std::ffi::OsStr::new("desktop")) {
        return vec![destination.to_path_buf()];
    }
    let default = entries_root.join("share/applications");
    if !destination.starts_with(&default) {
        return vec![destination.to_path_buf()];
    }
    let overlay_share = std::env::var_os("LINGLONG_EXPORT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("share"));
    let overlay = entries_root.join(overlay_share).join("applications");
    if overlay == default {
        return vec![destination.to_path_buf()];
    }
    let relative = destination
        .strip_prefix(&default)
        .unwrap_or_else(|_| Path::new(""));
    let overlay_destination = overlay.join(relative);
    let existing = [destination.to_path_buf(), overlay_destination.clone()]
        .into_iter()
        .filter(|path| fs::symlink_metadata(path).is_ok())
        .collect::<Vec<_>>();
    if existing.is_empty() {
        vec![overlay_destination]
    } else {
        existing
    }
}

#[derive(Clone, Copy)]
enum RewriteKind {
    Desktop,
    DbusService,
    SystemdService,
    ContextMenu,
}

fn rewrite_export_file(
    source: &Path,
    destination: &Path,
    app_id: &str,
) -> Result<(), std::io::Error> {
    let destination = destination.to_string_lossy();
    let source_display = source.to_string_lossy();
    let kind = if destination.contains("share/applications/") && destination.ends_with(".desktop") {
        Some(RewriteKind::Desktop)
    } else if destination.contains("share/dbus-1/") && destination.ends_with(".service") {
        Some(RewriteKind::DbusService)
    } else if (source_display.contains("share/systemd/user/")
        || source_display.contains("lib/systemd/user/"))
        && destination.ends_with(".service")
    {
        Some(RewriteKind::SystemdService)
    } else if destination.contains("share/applications/context-menus/")
        && destination.ends_with(".conf")
    {
        Some(RewriteKind::ContextMenu)
    } else {
        None
    };
    let Some(kind) = kind else {
        return Ok(());
    };
    let backup = PathBuf::from(format!("{}.linyaps.original", source.display()));
    if !backup.exists() {
        fs::rename(source, &backup)?;
    }
    let temporary = source.with_file_name(format!(
        "{}_{}.{}",
        source
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("entry"),
        unique_suffix(),
        source
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("tmp")
    ));
    fs::copy(&backup, &temporary)?;
    let content = fs::read_to_string(&temporary)?;
    let rewritten = rewrite_ini(&content, kind, app_id);
    fs::write(&temporary, rewritten)?;
    if matches!(kind, RewriteKind::Desktop) {
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
    }
    clear_path(source)?;
    fs::rename(temporary, source)?;
    Ok(())
}

#[derive(Debug)]
struct IniGroup {
    header: String,
    name: String,
    lines: Vec<String>,
}

fn rewrite_ini(content: &str, kind: RewriteKind, app_id: &str) -> String {
    let (preamble, mut groups) = parse_ini(content);
    match kind {
        RewriteKind::Desktop => {
            for group in &mut groups {
                let mut found_exec = false;
                for line in &mut group.lines {
                    if ini_key(line) == Some("Exec") {
                        let value = ini_value(line).unwrap_or_default();
                        *line = format!("Exec={}", desktop_exec(generated_origin(value), app_id));
                        found_exec = true;
                    }
                }
                if !found_exec {
                    group
                        .lines
                        .push(format!("Exec=/usr/bin/ll-cli run {app_id}"));
                }
                if group.name == "Desktop Entry" {
                    set_ini_key(&mut group.lines, "TryExec", "/usr/bin/ll-cli");
                    set_ini_key(&mut group.lines, "X-linglong", app_id);
                }
            }
        }
        RewriteKind::DbusService => {
            rewrite_group_execs(&mut groups, "D-BUS Service", &["Exec"], app_id, false);
        }
        RewriteKind::SystemdService => rewrite_group_execs(
            &mut groups,
            "Service",
            &[
                "ExecStart",
                "ExecStartPost",
                "ExecCondition",
                "ExecStop",
                "ExecStopPost",
                "ExecReload",
            ],
            app_id,
            false,
        ),
        RewriteKind::ContextMenu => {
            for group in &mut groups {
                for line in &mut group.lines {
                    if ini_key(line) == Some("Exec") {
                        let value = ini_value(line).unwrap_or_default();
                        *line = format!("Exec={}", desktop_exec(generated_origin(value), app_id));
                    }
                }
            }
        }
    }
    serialize_ini(&preamble, &groups)
}

fn rewrite_group_execs(
    groups: &mut [IniGroup],
    group_name: &str,
    keys: &[&str],
    app_id: &str,
    desktop: bool,
) {
    for group in groups.iter_mut().filter(|group| group.name == group_name) {
        for line in &mut group.lines {
            let Some(key) = ini_key(line) else {
                continue;
            };
            if !keys.contains(&key) {
                continue;
            }
            let origin = generated_origin(ini_value(line).unwrap_or_default());
            let command = if desktop {
                desktop_exec(origin, app_id)
            } else {
                format!("/usr/bin/ll-cli run {app_id} -- {origin}")
            };
            *line = format!("{key}={command}");
        }
    }
}

fn parse_ini(content: &str) -> (Vec<String>, Vec<IniGroup>) {
    let mut preamble = Vec::new();
    let mut groups = Vec::<IniGroup>::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
            groups.push(IniGroup {
                header: line.to_string(),
                name: trimmed[1..trimmed.len() - 1].to_string(),
                lines: Vec::new(),
            });
        } else if let Some(group) = groups.last_mut() {
            group.lines.push(line.to_string());
        } else {
            preamble.push(line.to_string());
        }
    }
    (preamble, groups)
}

fn serialize_ini(preamble: &[String], groups: &[IniGroup]) -> String {
    let mut lines = preamble.to_vec();
    for group in groups {
        lines.push(group.header.clone());
        lines.extend(group.lines.iter().cloned());
    }
    let mut content = lines.join("\n");
    content.push('\n');
    content
}

fn ini_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('#') || trimmed.starts_with(';') {
        return None;
    }
    trimmed.split_once('=').map(|(key, _)| key.trim())
}

fn ini_value(line: &str) -> Option<&str> {
    line.trim_start().split_once('=').map(|(_, value)| value)
}

fn set_ini_key(lines: &mut Vec<String>, key: &str, value: &str) {
    if let Some(line) = lines.iter_mut().find(|line| ini_key(line) == Some(key)) {
        *line = format!("{key}={value}");
    } else {
        lines.push(format!("{key}={value}"));
    }
}

fn generated_origin(command: &str) -> &str {
    if !command.contains("ll-cli") {
        return command;
    }
    if let Some((_, origin)) = command.split_once("--exec ") {
        return origin;
    }
    command.split_once("-- ").map_or("", |(_, origin)| origin)
}

fn desktop_exec(origin: &str, app_id: &str) -> String {
    if origin.is_empty() {
        return format!("/usr/bin/ll-cli run {app_id} ");
    }
    let bytes = origin.as_bytes();
    let mut index = 0;
    while let Some(offset) = bytes[index..].iter().position(|byte| *byte == b'%') {
        let percent = index + offset;
        let Some(code) = bytes.get(percent + 1).copied() else {
            break;
        };
        if code == b'%' {
            index = percent + 2;
            continue;
        }
        if matches!(code, b'f' | b'F' | b'u' | b'U') {
            let mut escaped = origin.to_string();
            escaped.insert(percent + 1, '%');
            let option = if matches!(code, b'f' | b'F') {
                "--file"
            } else {
                "--url"
            };
            return format!(
                "/usr/bin/ll-cli run {app_id} {option} %{} -- -- {escaped}",
                code as char
            );
        }
        break;
    }
    format!("/usr/bin/ll-cli run {app_id} -- {origin}")
}

fn update_shared_info(entries_root: &Path) {
    let default_applications = entries_root.join("share/applications");
    let overlay_share = std::env::var_os("LINGLONG_EXPORT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("share"));
    let overlay_applications = entries_root.join(overlay_share).join("applications");
    let mut application_dirs = Vec::new();
    if default_applications.is_dir() {
        application_dirs.push(default_applications);
    }
    if overlay_applications != entries_root.join("share/applications")
        && overlay_applications.is_dir()
    {
        application_dirs.push(overlay_applications);
    }
    if !application_dirs.is_empty() {
        let _ = Command::new("update-desktop-database")
            .args(&application_dirs)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let mime = entries_root.join("share/mime");
    if mime.is_dir() {
        let _ = Command::new("update-mime-database")
            .arg(mime)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let schemas = entries_root.join("share/glib-2.0/schemas");
    if schemas.is_dir() {
        let _ = Command::new("glib-compile-schemas")
            .arg(schemas)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn remove_links_into(directory: &Path, layer: &Path) -> Result<(), std::io::Error> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(entry.path())?;
            let absolute = normalize_path(&entry.path().parent().unwrap_or(directory).join(target));
            if absolute.starts_with(layer) {
                fs::remove_file(entry.path())?;
            }
        } else if metadata.is_dir() {
            remove_links_into(&entry.path(), layer)?;
        }
    }
    Ok(())
}

fn remove_empty_directories(directory: &Path) -> Result<bool, std::io::Error> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && remove_empty_directories(&entry.path())? {
            fs::remove_dir(entry.path())?;
        }
    }
    Ok(fs::read_dir(directory)?.next().is_none())
}

fn overlay_tree(source: &Path, destination: &Path) -> Result<(), std::io::Error> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.is_dir() {
            if fs::symlink_metadata(&destination_path).is_ok_and(|current| !current.is_dir()) {
                clear_path(&destination_path)?;
            }
            fs::create_dir_all(&destination_path)?;
            overlay_tree(&source_path, &destination_path)?;
        } else {
            clear_path(&destination_path)?;
            if metadata.file_type().is_symlink() {
                symlink(fs::read_link(&source_path)?, &destination_path)?;
            } else if fs::hard_link(&source_path, &destination_path).is_err() {
                fs::copy(&source_path, &destination_path)?;
                fs::set_permissions(&destination_path, metadata.permissions())?;
            }
        }
    }
    Ok(())
}

fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from = normalize_path(from);
    let to = normalize_path(to);
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..from_components.len() {
        relative.push("..");
    }
    for component in &to_components[common..] {
        relative.push(component.as_os_str());
    }
    relative
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn repository_refspecs(repo_root: &Path) -> Result<Vec<(String, Checksum)>, RepositoryError> {
    let mut refs = Vec::new();
    collect_refspecs(
        &repo_root.join("refs/heads"),
        &repo_root.join("refs/heads"),
        false,
        &mut refs,
    )?;
    collect_refspecs(
        &repo_root.join("refs/remotes"),
        &repo_root.join("refs/remotes"),
        true,
        &mut refs,
    )?;
    refs.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(refs)
}

fn collect_refspecs(
    base: &Path,
    directory: &Path,
    remote: bool,
    refs: &mut Vec<(String, Checksum)>,
) -> Result<(), RepositoryError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_refspecs(base, &entry.path(), remote, refs)?;
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(base)
            .expect("repository ref is below its base")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let refspec = if remote {
            let Some((remote, name)) = relative.split_once('/') else {
                continue;
            };
            format!("{remote}:{name}")
        } else {
            relative
        };
        let checksum = Checksum::from_hex(fs::read_to_string(entry.path())?.trim())
            .map_err(ostrya::Error::from)?;
        refs.push((refspec, checksum));
    }
    Ok(())
}

fn require_root(root: &Path) -> Result<(), RepositoryError> {
    if !root.is_dir() {
        return Err(RepositoryError::MissingRoot(root.to_path_buf()));
    }
    Ok(())
}

fn clear_path(path: &Path) -> Result<(), std::io::Error> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        fs::remove_file(path)?;
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

async fn rebuild_cache(
    cache_path: &Path,
    config: RepoConfigV2,
    repo: &Repo,
    repo_path: &Path,
) -> Result<RepositoryCacheStore, RepositoryError> {
    let mut cache = RepositoryCacheStore::empty(cache_path, config);
    for refspec in remote_refspecs(repo_path)? {
        let Some((remote, _)) = refspec.split_once(':') else {
            continue;
        };
        let Ok((tree, commit)) = repo.read_commit(&refspec).await else {
            continue;
        };
        let Ok(Some(TreeEntry::File { checksum, .. })) = tree.lookup(Path::new("info.json")).await
        else {
            continue;
        };
        let Ok(file) = repo.load_file(&checksum).await else {
            continue;
        };
        if file.is_symlink() {
            continue;
        }
        let Ok(mut reader) = file.reader().await else {
            continue;
        };
        let mut content = Vec::new();
        if reader.read_to_end(&mut content).await.is_err() {
            continue;
        }
        let Ok(info) = serde_json::from_slice::<PackageInfoV2>(&content) else {
            continue;
        };
        if cache
            .add_layer(RepositoryCacheLayersItem {
                commit: commit.to_hex(),
                deleted: None,
                info,
                repo: remote.to_string(),
            })
            .is_err()
        {
            continue;
        }
    }
    cache.write_to_disk()?;
    Ok(cache)
}

fn remote_refspecs(repo_path: &Path) -> Result<Vec<String>, std::io::Error> {
    let remotes = repo_path.join("refs/remotes");
    let mut refs = Vec::new();
    let Ok(entries) = fs::read_dir(remotes) else {
        return Ok(refs);
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            collect_remote_refs(
                &entry.path(),
                &entry.file_name().to_string_lossy(),
                "",
                &mut refs,
            )?;
        }
    }
    refs.sort();
    Ok(refs)
}

fn collect_remote_refs(
    directory: &Path,
    remote: &str,
    prefix: &str,
    refs: &mut Vec<String>,
) -> Result<(), std::io::Error> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type()?.is_dir() {
            collect_remote_refs(&entry.path(), remote, &relative, refs)?;
        } else {
            refs.push(format!("{remote}:{relative}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use linyaps_api::Repo;
    use tempfile::tempdir;

    use super::*;
    use crate::CACHE_VERSION;

    struct StaticRepositoryServer {
        address: SocketAddr,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl StaticRepositoryServer {
        fn start(archive: PathBuf, commit: Checksum, reference: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let worker_requests = requests.clone();
            let worker_stop = stop.clone();
            let ref_path = format!("/repos/stable/refs/heads/{reference}");
            let worker = thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let request = read_http_request(&mut stream);
                            let path = request
                                .lines()
                                .next()
                                .and_then(|line| line.split_whitespace().nth(1))
                                .unwrap_or("/")
                                .to_string();
                            worker_requests.lock().unwrap().push(path.clone());
                            if path == ref_path {
                                write_http_response(
                                    &mut stream,
                                    200,
                                    format!("{}\n", commit.to_hex()).as_bytes(),
                                );
                                continue;
                            }
                            let Some(relative) = path.strip_prefix("/repos/stable/objects/") else {
                                write_http_response(&mut stream, 404, b"not found");
                                continue;
                            };
                            if relative.split('/').any(|part| part == "..") {
                                write_http_response(&mut stream, 404, b"not found");
                                continue;
                            }
                            match fs::read(archive.join("objects").join(relative)) {
                                Ok(bytes) => write_http_response(&mut stream, 200, &bytes),
                                Err(_) => write_http_response(&mut stream, 404, b"not found"),
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                address,
                requests,
                stop,
                worker: Some(worker),
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.address)
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for StaticRepositoryServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let _ = TcpStream::connect(self.address);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap();
            }
        }
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") || count == 0 {
                break;
            }
        }
        String::from_utf8(request).unwrap()
    }

    fn write_http_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Not Found" };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    }

    fn remote_object_request(checksum: &Checksum, object_type: ObjectType) -> String {
        let checksum = checksum.to_hex();
        format!(
            "/repos/stable/objects/{}/{}.{}",
            &checksum[..2],
            &checksum[2..],
            object_type.extension(RepoMode::Archive)
        )
    }

    fn config() -> RepoConfigV2 {
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

    fn info() -> PackageInfoV2 {
        PackageInfoV2 {
            arch: vec!["x86_64".to_string()],
            base: "org.deepin.base/23.1.0".to_string(),
            channel: "main".to_string(),
            command: Some(vec!["demo".to_string()]),
            compatible_version: None,
            description: Some("demo".to_string()),
            extension_implementation: None,
            extensions: None,
            id: "org.example.demo".to_string(),
            kind: "app".to_string(),
            module: "binary".to_string(),
            name: "Demo".to_string(),
            permissions: None,
            runtime: None,
            schema_version: "1.0".to_string(),
            size: 4,
            uuid: None,
            version: "1.2.3.4".to_string(),
        }
    }

    #[test]
    fn builds_upstream_ref_format() {
        let reference = Reference::new(
            "main",
            "org.example.demo",
            Version::parse("1.2.3.4").unwrap(),
            Architecture::X86_64,
        )
        .unwrap();
        assert_eq!(
            ostree_ref(&reference, "binary", None).unwrap(),
            "main/org.example.demo/1.2.3.4/x86_64/binary"
        );
        assert_eq!(
            ostree_ref(&reference, "develop", Some("debug")).unwrap(),
            "main/org.example.demo/1.2.3.4/x86_64/develop_debug"
        );
    }

    #[test]
    fn rewrites_desktop_and_service_commands() {
        let desktop = rewrite_ini(
            "[Desktop Entry]\nName=Demo\nExec=demo %F\n\n[Desktop Action New]\nName=New\n",
            RewriteKind::Desktop,
            "org.example.demo",
        );
        assert!(
            desktop.contains("Exec=/usr/bin/ll-cli run org.example.demo --file %F -- -- demo %%F")
        );
        assert!(desktop.contains("TryExec=/usr/bin/ll-cli"));
        assert!(desktop.contains("X-linglong=org.example.demo"));
        assert!(desktop.contains("Exec=/usr/bin/ll-cli run org.example.demo\n"));

        let service = rewrite_ini(
            "[Service]\nExecStart=/usr/bin/demo\nExecStop=-/usr/bin/demo --quit\n",
            RewriteKind::SystemdService,
            "org.example.demo",
        );
        assert!(
            service.contains("ExecStart=/usr/bin/ll-cli run org.example.demo -- /usr/bin/demo")
        );
        assert!(
            service
                .contains("ExecStop=/usr/bin/ll-cli run org.example.demo -- -/usr/bin/demo --quit")
        );
    }

    #[tokio::test]
    async fn exports_rewritten_desktop_and_unexports_it() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo-root");
        let source = temporary.path().join("source");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(source.join("entries/share/applications")).unwrap();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec(&info()).unwrap(),
        )
        .unwrap();
        fs::write(
            source.join("entries/share/applications/org.example.demo.desktop"),
            "[Desktop Entry]\nName=Demo\nExec=demo %U\n",
        )
        .unwrap();
        let mut repository = LocalRepository::create(&root, config()).await.unwrap();
        repository
            .import_layer_dir(&source, &[], None)
            .await
            .unwrap();
        let reference = reference_from_info(&info()).unwrap();
        repository.export_reference(&reference).unwrap();

        let exported = root.join("entries/share/applications/org.example.demo.desktop");
        assert!(
            fs::symlink_metadata(&exported)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::read_to_string(&exported)
                .unwrap()
                .contains("Exec=/usr/bin/ll-cli run org.example.demo --url %U -- -- demo %%U")
        );
        let layer_desktop = repository
            .layer_path(&reference, "binary")
            .unwrap()
            .join("entries/share/applications/org.example.demo.desktop");
        assert!(PathBuf::from(format!("{}.linyaps.original", layer_desktop.display())).is_file());

        repository.unexport_reference(&reference).unwrap();
        assert!(fs::symlink_metadata(exported).is_err());
    }

    #[tokio::test]
    async fn creates_imports_overlays_checks_out_and_reloads() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo-root");
        let source = temporary.path().join("source");
        let overlay = temporary.path().join("overlay");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&overlay).unwrap();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec(&info()).unwrap(),
        )
        .unwrap();
        fs::write(source.join("payload"), "base").unwrap();
        fs::set_permissions(source.join("payload"), fs::Permissions::from_mode(0o666)).unwrap();
        fs::write(overlay.join("payload"), "overlay").unwrap();
        fs::write(overlay.join("extra"), "extra").unwrap();

        let mut repository = LocalRepository::create(&root, config()).await.unwrap();
        let imported = repository
            .import_layer_dir(&source, std::slice::from_ref(&overlay), None)
            .await
            .unwrap();

        assert!(root.join("config.yaml").is_file());
        assert!(root.join("repo/config").is_file());
        assert!(root.join("states.json").is_file());
        assert_eq!(
            fs::read_to_string(imported.path.join("payload")).unwrap(),
            "overlay"
        );
        assert_eq!(
            fs::read_to_string(imported.path.join("extra")).unwrap(),
            "extra"
        );
        assert_eq!(
            fs::metadata(imported.path.join("payload"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
        let cache: linyaps_api::RepositoryCache =
            serde_json::from_slice(&fs::read(root.join("states.json")).unwrap()).unwrap();
        assert_eq!(cache.version, CACHE_VERSION);
        assert_eq!(cache.layers[0].commit, imported.commit);
        assert_eq!(cache.layers[0].repo, LOCAL_REMOTE);
        assert!(
            root.join("repo/refs/remotes/local/main/org.example.demo/1.2.3.4/x86_64/binary")
                .is_file()
        );

        let loaded = LocalRepository::open(&root).await.unwrap();
        assert_eq!(loaded.cache().existing_layers().len(), 1);
        assert_eq!(loaded.list_layer_items().len(), 1);
        let fuzzy = "main:org.example.demo/1.2.3/x86_64"
            .parse::<FuzzyReference>()
            .unwrap();
        let resolved = loaded.resolve_local(&fuzzy, true).unwrap();
        assert_eq!(resolved.to_string(), "main:org.example.demo/1.2.3.4/x86_64");
        assert_eq!(
            loaded.layer_path(&resolved, "binary").unwrap(),
            imported.path
        );
        assert_eq!(loaded.read_layer_info(&resolved, "binary").unwrap(), info());
        loaded
            .layer_create_time(&loaded.list_layer_items()[0])
            .unwrap();
    }

    #[tokio::test]
    async fn archive_import_preserves_remote_commit_identity() {
        let temporary = tempdir().unwrap();
        let archive_root = temporary.path().join("archive");
        let source = temporary.path().join("source");
        let local_root = temporary.path().join("local");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&local_root).unwrap();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec(&info()).unwrap(),
        )
        .unwrap();
        fs::write(source.join("payload"), "remote payload").unwrap();
        std::os::unix::fs::symlink("payload", source.join("payload-link")).unwrap();

        let archive = ostrya::Repo::create(&archive_root, CreateOptions::new(RepoMode::Archive))
            .await
            .unwrap();
        let transaction = archive.transaction().await.unwrap();
        let mut tree = MutableTree::new();
        let mut modifier = CommitModifier::new(
            CommitModifierFlags::CANONICAL_PERMISSIONS | CommitModifierFlags::GENERATE_SIZES,
        );
        let source_directory = fs::File::open(&source).unwrap();
        transaction
            .write_dfd_to_mtree(
                source_directory.as_fd(),
                Path::new("."),
                &mut tree,
                Some(&mut modifier),
            )
            .await
            .unwrap();
        let root = transaction.write_mtree(&mut tree).await.unwrap();
        let remote_commit = transaction
            .write_commit(CommitOptions::default(), &root)
            .await
            .unwrap();
        transaction.set_ref(
            "stable:main/org.example.demo/1.2.3.4/x86_64/binary",
            Some(&remote_commit),
        );
        transaction.commit().await.unwrap();

        let mut repository = LocalRepository::create(&local_root, config())
            .await
            .unwrap();
        let reference = reference_from_info(&info()).unwrap();
        let imported = repository
            .import_archive_commit(&reference, &info(), &archive, &remote_commit, "stable")
            .await
            .unwrap();

        assert_eq!(imported.commit, remote_commit.to_hex());
        assert_eq!(
            fs::read_to_string(imported.path.join("payload")).unwrap(),
            "remote payload"
        );
        assert_eq!(
            fs::read_link(imported.path.join("payload-link")).unwrap(),
            PathBuf::from("payload")
        );
        assert_eq!(
            repository
                .repo
                .load_object_bytes(ObjectType::Commit, &remote_commit)
                .await
                .unwrap(),
            archive
                .load_object_bytes(ObjectType::Commit, &remote_commit)
                .await
                .unwrap()
        );
        assert_eq!(
            repository
                .repo
                .resolve_rev("stable:main/org.example.demo/1.2.3.4/x86_64/binary", false,)
                .await
                .unwrap(),
            Some(remote_commit)
        );
    }

    #[tokio::test]
    async fn metadata_prefetch_and_pull_download_only_missing_objects() {
        let temporary = tempdir().unwrap();
        let archive_root = temporary.path().join("remote-archive");
        let source = temporary.path().join("remote-source");
        let local_root = temporary.path().join("local");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&local_root).unwrap();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec(&info()).unwrap(),
        )
        .unwrap();
        let payload = (0_u8..=255).cycle().take(256 * 1024).collect::<Vec<_>>();
        fs::write(source.join("payload"), &payload).unwrap();

        let archive = ostrya::Repo::create(&archive_root, CreateOptions::new(RepoMode::Archive))
            .await
            .unwrap();
        let transaction = archive.transaction().await.unwrap();
        let mut tree = MutableTree::new();
        let mut modifier = CommitModifier::new(
            CommitModifierFlags::CANONICAL_PERMISSIONS | CommitModifierFlags::GENERATE_SIZES,
        );
        let source_directory = fs::File::open(&source).unwrap();
        transaction
            .write_dfd_to_mtree(
                source_directory.as_fd(),
                Path::new("."),
                &mut tree,
                Some(&mut modifier),
            )
            .await
            .unwrap();
        let root = transaction.write_mtree(&mut tree).await.unwrap();
        let remote_commit = transaction
            .write_commit(CommitOptions::default(), &root)
            .await
            .unwrap();
        transaction.commit().await.unwrap();

        let (commit, _) = archive.load_commit(&remote_commit).await.unwrap();
        let root_tree = archive.load_dirtree(&commit.root_dirtree).await.unwrap();
        let info_checksum = root_tree
            .files
            .iter()
            .find_map(|(name, checksum)| (name == "info.json").then_some(*checksum))
            .unwrap();
        let payload_checksum = root_tree
            .files
            .iter()
            .find_map(|(name, checksum)| (name == "payload").then_some(*checksum))
            .unwrap();
        let reference_name = "main/org.example.demo/1.2.3.4/x86_64/binary";
        let server =
            StaticRepositoryServer::start(archive_root.clone(), remote_commit, reference_name);
        let remote = linyaps_api::Repo {
            alias: None,
            mirror_enabled: None,
            name: "stable".to_string(),
            priority: 0,
            url: server.url(),
        };
        let reference = reference_from_info(&info()).unwrap();
        let mut repository = LocalRepository::create(&local_root, config())
            .await
            .unwrap();

        let metadata = repository
            .fetch_remote_metadata(&reference, &remote, "binary", true)
            .await
            .unwrap();
        assert_eq!(metadata.commit, remote_commit.to_hex());
        assert_eq!(metadata.selected_ref, reference_name);
        assert_eq!(metadata.package_info, Some(info()));
        let before_pull = repository.get_ref_statistics(&metadata).await.unwrap();
        assert!(before_pull.archived > 0);
        assert!(before_pull.needed_archived > 0);
        assert!(before_pull.needed_archived < before_pull.archived);
        assert!(before_pull.needed_objects < before_pull.objects);

        let requests = server.requests();
        let info_request = remote_object_request(&info_checksum, ObjectType::File);
        let payload_request = remote_object_request(&payload_checksum, ObjectType::File);
        assert_eq!(
            requests
                .iter()
                .filter(|request| **request == info_request)
                .count(),
            1
        );
        assert!(!requests.iter().any(|request| request == &payload_request));

        let mut transferred = 0_u64;
        let imported = repository
            .pull_with_progress(&reference, &remote, "binary", |bytes| {
                transferred = transferred.saturating_add(bytes);
            })
            .await
            .unwrap();
        assert_eq!(imported.commit, remote_commit.to_hex());
        assert_eq!(fs::read(imported.path.join("payload")).unwrap(), payload);
        assert!(transferred > 0);
        assert_eq!(
            repository
                .get_ref_statistics(&metadata)
                .await
                .unwrap()
                .needed_objects,
            0
        );

        let requests = server.requests();
        assert_eq!(
            requests
                .iter()
                .filter(|request| **request == info_request)
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| **request == payload_request)
                .count(),
            1
        );
        let object_requests = requests
            .iter()
            .filter(|request| request.contains("/objects/"))
            .count();

        let mut second_transfer = 0_u64;
        let second = repository
            .pull_with_progress(&reference, &remote, "binary", |bytes| {
                second_transfer = second_transfer.saturating_add(bytes);
            })
            .await
            .unwrap();
        assert_eq!(second.commit, remote_commit.to_hex());
        assert_eq!(second_transfer, 0);
        assert_eq!(
            server
                .requests()
                .iter()
                .filter(|request| request.contains("/objects/"))
                .count(),
            object_requests
        );
    }

    #[tokio::test]
    async fn prune_objects_removes_unreachable_commits_only() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo-root");
        let source = temporary.path().join("source");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec(&info()).unwrap(),
        )
        .unwrap();
        fs::write(source.join("payload"), "old").unwrap();

        let mut repository = LocalRepository::create(&root, config()).await.unwrap();
        let old = repository
            .import_layer_dir(&source, &[], None)
            .await
            .unwrap();
        fs::write(source.join("payload"), "new").unwrap();
        let current = repository
            .import_layer_dir(&source, &[], None)
            .await
            .unwrap();
        assert_ne!(old.commit, current.commit);

        let object_path = |commit: &str| {
            root.join("repo/objects")
                .join(&commit[..2])
                .join(format!("{}.commit", &commit[2..]))
        };
        assert!(object_path(&old.commit).is_file());
        assert!(object_path(&current.commit).is_file());

        repository.prune_objects().await.unwrap();

        assert!(!object_path(&old.commit).exists());
        assert!(object_path(&current.commit).is_file());
        assert_eq!(
            fs::read_to_string(current.path.join("payload")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn deferred_layers_stay_reachable_until_collected() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo-root");
        let source = temporary.path().join("source");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec(&info()).unwrap(),
        )
        .unwrap();
        fs::write(source.join("payload"), "old").unwrap();

        let mut repository = LocalRepository::create(&root, config()).await.unwrap();
        let old = repository
            .import_layer_dir(&source, &[], None)
            .await
            .unwrap();
        let reference = reference_from_info(&old.info).unwrap();
        assert!(repository.mark_layer_deleted(&reference, "binary").unwrap());
        assert!(repository.list_layer_items().is_empty());
        assert_eq!(repository.list_deleted_layer_items().len(), 1);
        assert!(repository.layer_item(&reference, "binary").is_err());

        let object_path = |commit: &str| {
            root.join("repo/objects")
                .join(&commit[..2])
                .join(format!("{}.commit", &commit[2..]))
        };
        repository.prune_objects().await.unwrap();
        assert!(object_path(&old.commit).is_file());

        assert!(
            repository
                .restore_deleted_layer(&reference, "binary")
                .unwrap()
        );
        assert_eq!(
            repository.layer_item(&reference, "binary").unwrap().commit,
            old.commit
        );
        assert!(repository.mark_layer_deleted(&reference, "binary").unwrap());

        fs::write(source.join("payload"), "new").unwrap();
        let current = repository
            .import_layer_dir(&source, &[], None)
            .await
            .unwrap();
        assert_ne!(old.commit, current.commit);
        assert_eq!(repository.list_layer_items().len(), 1);
        assert_eq!(repository.list_deleted_layer_items().len(), 1);
        assert_eq!(
            repository.layer_item(&reference, "binary").unwrap().commit,
            current.commit
        );

        let deleted = repository.list_deleted_layer_items().remove(0);
        assert!(repository.remove_layer_item(&deleted).await.unwrap());
        assert_eq!(
            repository.layer_item(&reference, "binary").unwrap().commit,
            current.commit
        );
        repository.prune_objects().await.unwrap();
        assert!(!object_path(&old.commit).exists());
        assert!(object_path(&current.commit).is_file());
    }

    #[tokio::test]
    async fn clean_unreferenced_removes_uab_subrefs() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo-root");
        let source = temporary.path().join("source");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&source).unwrap();
        let mut runtime = info();
        runtime.id = "org.example.runtime".to_string();
        runtime.kind = "runtime".to_string();
        runtime.module = "binary".to_string();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec(&runtime).unwrap(),
        )
        .unwrap();
        fs::write(source.join("payload"), "runtime").unwrap();

        let mut repository = LocalRepository::create(&root, config()).await.unwrap();
        let imported = repository
            .import_layer_dir(&source, &[], Some("bundle-uuid"))
            .await
            .unwrap();
        let reference = reference_from_info(&runtime).unwrap();
        let ref_name = ostree_ref(&reference, "binary", Some("bundle-uuid")).unwrap();
        let ref_path = root.join("repo/refs/remotes/local").join(ref_name);
        let object_path = root
            .join("repo/objects")
            .join(&imported.commit[..2])
            .join(format!("{}.commit", &imported.commit[2..]));
        assert!(ref_path.is_file());
        assert!(repository.remove_layer(&reference, "binary").await.unwrap());
        assert!(ref_path.is_file());

        repository.clean_unreferenced().await.unwrap();

        assert!(!ref_path.exists());
        assert!(!object_path.exists());
    }

    #[tokio::test]
    async fn resolves_latest_version_and_runtime_module_compatibility() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo-root");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        let mut old = info();
        old.module = "runtime".to_string();
        fs::write(first.join("info.json"), serde_json::to_vec(&old).unwrap()).unwrap();
        let mut new = old.clone();
        new.version = "2.0.0.0".to_string();
        fs::write(second.join("info.json"), serde_json::to_vec(&new).unwrap()).unwrap();

        let mut repository = LocalRepository::create(&root, config()).await.unwrap();
        repository
            .import_layer_dir(&first, &[], None)
            .await
            .unwrap();
        let latest = repository
            .import_layer_dir(&second, &[], None)
            .await
            .unwrap();

        let fuzzy = "org.example.demo".parse::<FuzzyReference>().unwrap();
        let resolved = repository.resolve_local(&fuzzy, false).unwrap();
        assert_eq!(resolved.version.to_string(), "2.0.0.0");
        assert_eq!(
            repository.layer_path(&resolved, "binary").unwrap(),
            latest.path
        );

        let exact = "org.example.demo/1.2.3.4"
            .parse::<FuzzyReference>()
            .unwrap();
        assert_eq!(
            repository
                .resolve_local(&exact, false)
                .unwrap()
                .version
                .to_string(),
            "1.2.3.4"
        );
    }

    #[tokio::test]
    async fn create_repairs_missing_cache_from_local_refs() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo-root");
        let source = temporary.path().join("source");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("info.json"),
            serde_json::to_vec(&info()).unwrap(),
        )
        .unwrap();
        let mut repository = LocalRepository::create(&root, config()).await.unwrap();
        repository
            .import_layer_dir(&source, &[], None)
            .await
            .unwrap();
        drop(repository);
        fs::remove_file(root.join("states.json")).unwrap();
        assert!(LocalRepository::open(&root).await.is_err());

        let repaired = LocalRepository::create(&root, config()).await.unwrap();
        assert_eq!(repaired.cache().existing_layers().len(), 1);
    }

    #[tokio::test]
    async fn update_config_persists_yaml_and_cache() {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join("repo-root");
        fs::create_dir_all(&root).unwrap();
        let mut repository = LocalRepository::create(&root, config()).await.unwrap();
        let updated = RepoConfigV2 {
            default_repo: "testing".to_string(),
            repos: vec![Repo {
                alias: None,
                mirror_enabled: Some(true),
                name: "testing".to_string(),
                priority: 100,
                url: "https://testing.example".to_string(),
            }],
            version: 2,
        };
        repository.update_config(updated.clone()).unwrap();

        assert_eq!(load_config(&root.join("config.yaml")).unwrap(), updated);
        assert_eq!(
            RepositoryCacheStore::load(root.join("states.json"))
                .unwrap()
                .data()
                .config,
            updated
        );
    }
}
