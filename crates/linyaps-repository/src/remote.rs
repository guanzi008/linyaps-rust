use std::collections::{HashSet, VecDeque};
use std::env;
use std::fs;
use std::io::Write;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_lite::StreamExt;
use linyaps_api::{PackageInfoV2, Repo};
use linyaps_core::{Architecture, FuzzyReference, Reference, Version};
use ostrya::{
    CheckoutMode, CheckoutOptions, Checksum, Commit, CreateOptions, DirTree, ObjectType,
    Repo as OstreeRepo, RepoMode,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_CONNECT_TIMEOUT_SECONDS: u64 = 5;
const TOKEN_HEADER: &str = "X-Token";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ApiResponse<T> {
    #[serde(default)]
    pub code: i64,
    pub data: Option<T>,
    pub msg: Option<String>,
    pub trace_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FuzzySearchRequest {
    #[serde(rename = "appId")]
    pub app_id: String,
    pub arch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(rename = "repoName")]
    pub repo_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct RemotePackage {
    #[serde(default, rename = "appId")]
    pub app_id: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub module: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "repoName")]
    pub repo_name: Option<String>,
    #[serde(default)]
    pub runtime: Option<String>,
    #[serde(default)]
    pub size: i64,
    #[serde(default, rename = "uabUrl")]
    pub uab_url: Option<String>,
    #[serde(default)]
    pub version: String,
}

impl From<RemotePackage> for PackageInfoV2 {
    fn from(package: RemotePackage) -> Self {
        Self {
            arch: vec![package.arch],
            base: package.base,
            channel: package.channel,
            command: None,
            compatible_version: None,
            description: package.description,
            extension_implementation: None,
            extensions: None,
            id: package.app_id,
            kind: package.kind,
            module: package.module,
            name: package.name,
            permissions: None,
            runtime: package.runtime,
            schema_version: String::new(),
            size: package.size,
            uuid: None,
            version: package.version,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SignInData {
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NewUploadTask {
    pub id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct UploadStatus {
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PulledLayer {
    pub commit: String,
    pub path: PathBuf,
    pub archive_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResolvedRemoteLayer {
    pub selected_ref: String,
    pub commit: Checksum,
}

#[derive(Debug, Error)]
pub enum RemoteError {
    #[error("failed to create HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("failed to send request to remote server: {0}")]
    Request(#[from] reqwest::Error),
    #[error("remote server returned code {code}: {message}")]
    Api { code: i64, message: String },
    #[error("remote server response has no data")]
    MissingData,
    #[error("failed to access repository file: {0}")]
    UploadFile(#[from] std::io::Error),
    #[error("failed to detect current architecture: {0}")]
    Architecture(String),
    #[error("packages is empty")]
    EmptyPackages,
    #[error("remote reference not found: {0}")]
    MissingReference(String),
    #[error("invalid remote repository object: {0}")]
    InvalidObject(String),
    #[error("failed to read pulled OSTree repository: {0}")]
    Ostree(#[from] ostrya::Error),
}

#[derive(Clone, Debug, Default)]
pub struct RemotePackages {
    repositories: Vec<(Repo, Vec<PackageInfoV2>)>,
}

impl RemotePackages {
    pub fn add_packages(&mut self, repo: Repo, packages: Vec<PackageInfoV2>) -> &mut Self {
        self.repositories.push((repo, packages));
        self
    }

    pub fn repositories(&self) -> &[(Repo, Vec<PackageInfoV2>)] {
        &self.repositories
    }

    pub fn is_empty(&self) -> bool {
        self.repositories.is_empty()
    }

    pub fn latest_package(&self) -> Result<(Repo, PackageInfoV2), RemoteError> {
        let mut latest: Option<(&Repo, &PackageInfoV2)> = None;
        for (repo, packages) in &self.repositories {
            for package in packages {
                if latest.is_none_or(|(_, current)| package_version_is_older(current, package)) {
                    latest = Some((repo, package));
                }
            }
        }
        latest
            .map(|(repo, package)| (repo.clone(), package.clone()))
            .ok_or(RemoteError::EmptyPackages)
    }

    pub fn reference_modules(&self, reference: &Reference) -> Vec<String> {
        let mut modules = Vec::new();
        for (_, packages) in &self.repositories {
            for package in packages {
                if package.id == reference.id
                    && package.channel == reference.channel
                    && package.version == reference.version.to_string()
                    && package.arch.first() == Some(&reference.architecture.to_string())
                {
                    modules.push(package.module.clone());
                }
            }
        }
        modules
    }
}

#[derive(Clone, Debug)]
pub struct RemoteRepositoryClient {
    base_url: String,
    client: reqwest::Client,
}

impl RemoteRepositoryClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self, RemoteError> {
        linyaps_core::tls::install_default_provider();
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(connect_timeout_seconds()))
            .user_agent(format!("linglong/{}", linyaps_core::VERSION_FULL))
            .build()
            .map_err(RemoteError::Client)?;
        Ok(Self { base_url, client })
    }

    pub async fn fuzzy_search(
        &self,
        request: &FuzzySearchRequest,
    ) -> Result<Vec<RemotePackage>, RemoteError> {
        let response: ApiResponse<Vec<RemotePackage>> = self
            .client
            .post(self.endpoint("/api/v0/apps/fuzzysearchapp"))
            .json(request)
            .send()
            .await?
            .json()
            .await?;
        success_data(response).map(Option::unwrap_or_default)
    }

    pub async fn search_packages(
        &self,
        reference: &FuzzyReference,
        repo: &Repo,
        semantic_matching: bool,
    ) -> Result<Vec<PackageInfoV2>, RemoteError> {
        let architecture = reference
            .architecture
            .map(Ok)
            .unwrap_or_else(Architecture::current)
            .map_err(|error| RemoteError::Architecture(error.to_string()))?;
        let packages = self
            .fuzzy_search(&FuzzySearchRequest {
                app_id: reference.id.clone(),
                arch: architecture.to_string(),
                channel: reference.channel.clone(),
                repo_name: repo.name.clone(),
                version: reference.version.clone(),
            })
            .await?;
        Ok(packages
            .into_iter()
            .map(PackageInfoV2::from)
            .filter(|package| {
                !semantic_matching || package_semantically_matches(package, reference)
            })
            .collect())
    }

    pub async fn pull_layer(
        &self,
        reference: &Reference,
        repo: &Repo,
        module: &str,
        work_directory: &Path,
    ) -> Result<PulledLayer, RemoteError> {
        let archive_path = work_directory.join("archive");
        let archive =
            OstreeRepo::create(&archive_path, CreateOptions::new(RepoMode::Archive)).await?;
        let resolved = self.resolve_layer(reference, repo, module).await?;
        let selected_ref = resolved.selected_ref;
        let commit_checksum = resolved.commit;
        let commit_bytes = self
            .fetch_metadata_object(
                repo,
                &archive_path,
                &commit_checksum,
                ObjectType::Commit,
                None,
            )
            .await?;
        let commit = Commit::parse(&commit_bytes)
            .map_err(|error| RemoteError::InvalidObject(error.to_string()))?;

        let mut pending = VecDeque::from([(commit.root_dirtree, commit.root_dirmeta)]);
        let mut seen_trees = HashSet::new();
        let mut seen_metadata = HashSet::new();
        let mut files = HashSet::new();
        while let Some((tree_checksum, metadata_checksum)) = pending.pop_front() {
            if seen_metadata.insert(metadata_checksum) {
                self.fetch_metadata_object(
                    repo,
                    &archive_path,
                    &metadata_checksum,
                    ObjectType::DirMeta,
                    None,
                )
                .await?;
            }
            if !seen_trees.insert(tree_checksum) {
                continue;
            }
            let tree_bytes = self
                .fetch_metadata_object(
                    repo,
                    &archive_path,
                    &tree_checksum,
                    ObjectType::DirTree,
                    None,
                )
                .await?;
            let tree = DirTree::parse(&tree_bytes)
                .map_err(|error| RemoteError::InvalidObject(error.to_string()))?;
            files.extend(tree.files.into_iter().map(|(_, checksum)| checksum));
            pending.extend(
                tree.dirs
                    .into_iter()
                    .map(|(_, tree, metadata)| (tree, metadata)),
            );
        }
        for checksum in files {
            self.fetch_file_object(repo, &archive_path, &checksum, None)
                .await?;
        }

        let checkout = work_directory.join("layer");
        clear_checkout(&checkout)?;
        let parent = fs::File::open(work_directory)?;
        let mut options = CheckoutOptions::new(CheckoutMode::User);
        archive
            .checkout_at(
                &mut options,
                parent.as_fd(),
                Path::new("layer"),
                &commit_checksum,
            )
            .await?;
        if !checkout.join("info.json").is_file() {
            return Err(RemoteError::InvalidObject(format!(
                "{selected_ref} has no info.json"
            )));
        }
        Ok(PulledLayer {
            commit: commit_checksum.to_hex(),
            path: checkout,
            archive_path,
        })
    }

    pub async fn sign_in(&self, username: &str, password: &str) -> Result<SignInData, RemoteError> {
        #[derive(Serialize)]
        struct Auth<'a> {
            username: &'a str,
            password: &'a str,
        }
        let response: ApiResponse<SignInData> = self
            .client
            .post(self.endpoint("/api/v1/sign-in"))
            .json(&Auth { username, password })
            .send()
            .await?
            .json()
            .await?;
        success_data(response)?.ok_or(RemoteError::MissingData)
    }

    pub async fn new_upload_task(
        &self,
        token: &str,
        repo_name: &str,
        reference: &str,
    ) -> Result<NewUploadTask, RemoteError> {
        #[derive(Serialize)]
        struct Request<'a> {
            #[serde(rename = "repoName")]
            repo_name: &'a str,
            #[serde(rename = "ref")]
            reference: &'a str,
        }
        let response: ApiResponse<NewUploadTask> = self
            .client
            .post(self.endpoint("/api/v1/upload-tasks"))
            .header(TOKEN_HEADER, token)
            .json(&Request {
                repo_name,
                reference,
            })
            .send()
            .await?
            .json()
            .await?;
        success_data(response)?.ok_or(RemoteError::MissingData)
    }

    pub async fn upload_layer_file(
        &self,
        token: &str,
        task_id: &str,
        path: &Path,
    ) -> Result<(), RemoteError> {
        let part = reqwest::multipart::Part::file(path).await?.file_name(
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "layer.tgz".to_string()),
        );
        let form = reqwest::multipart::Form::new().part("file", part);
        let response: ApiResponse<serde_json::Value> = self
            .client
            .put(self.endpoint(&format!("/api/v1/upload-tasks/{task_id}/layer")))
            .header(TOKEN_HEADER, token)
            .multipart(form)
            .send()
            .await?
            .json()
            .await?;
        success_data(response)?;
        Ok(())
    }

    pub async fn upload_task_status(
        &self,
        token: &str,
        task_id: &str,
    ) -> Result<UploadStatus, RemoteError> {
        let response: ApiResponse<UploadStatus> = self
            .client
            .get(self.endpoint(&format!("/api/v1/upload-tasks/{task_id}/status")))
            .header(TOKEN_HEADER, token)
            .send()
            .await?
            .json()
            .await?;
        success_data(response)?.ok_or(RemoteError::MissingData)
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub(crate) async fn resolve_layer(
        &self,
        reference: &Reference,
        repo: &Repo,
        module: &str,
    ) -> Result<ResolvedRemoteLayer, RemoteError> {
        let mut candidates = vec![remote_ref(reference, module)];
        if module == "binary" {
            candidates.push(remote_ref(reference, "runtime"));
        }
        for candidate in candidates {
            if let Some(commit) = self.fetch_ref(repo, &candidate).await? {
                return Ok(ResolvedRemoteLayer {
                    selected_ref: candidate,
                    commit,
                });
            }
        }
        Err(RemoteError::MissingReference(remote_ref(reference, module)))
    }

    async fn fetch_ref(
        &self,
        repo: &Repo,
        reference: &str,
    ) -> Result<Option<Checksum>, RemoteError> {
        let response = self
            .client
            .get(self.endpoint(&format!("/repos/{}/refs/heads/{reference}", repo.name)))
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let value = response.error_for_status()?.text().await?;
        let value = value.trim();
        let checksum = Checksum::from_hex(value).map_err(|error| {
            RemoteError::InvalidObject(format!("invalid checksum for {reference}: {error}"))
        })?;
        Ok(Some(checksum))
    }

    pub(crate) async fn fetch_metadata_object(
        &self,
        repo: &Repo,
        archive_path: &Path,
        checksum: &Checksum,
        object_type: ObjectType,
        progress: Option<&mut (dyn FnMut(u64) + Send)>,
    ) -> Result<Vec<u8>, RemoteError> {
        let relative = archive_object_path(checksum, object_type);
        let destination = archive_path.join("objects").join(&relative);
        let bytes = if destination.is_file() {
            fs::read(&destination)?
        } else {
            let response = self
                .client
                .get(self.endpoint(&format!("/repos/{}/objects/{relative}", repo.name)))
                .send()
                .await?
                .error_for_status()?;
            let bytes = response.bytes().await?.to_vec();
            write_object(&destination, &bytes)?;
            if let Some(progress) = progress {
                progress(bytes.len() as u64);
            }
            bytes
        };
        let actual = Checksum::sha256(&bytes);
        if &actual != checksum {
            return Err(RemoteError::InvalidObject(format!(
                "checksum mismatch for {relative}: expected {checksum}, got {actual}"
            )));
        }
        Ok(bytes)
    }

    pub(crate) async fn fetch_file_object(
        &self,
        repo: &Repo,
        archive_path: &Path,
        checksum: &Checksum,
        mut progress: Option<&mut (dyn FnMut(u64) + Send)>,
    ) -> Result<(), RemoteError> {
        let relative = archive_object_path(checksum, ObjectType::File);
        let destination = archive_path.join("objects").join(&relative);
        if destination.is_file() {
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = destination.with_extension("filez.part");
        let response = self
            .client
            .get(self.endpoint(&format!("/repos/{}/objects/{relative}", repo.name)))
            .send()
            .await?
            .error_for_status()?;
        let mut stream = response.bytes_stream();
        let mut output = fs::File::create(&temporary)?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            output.write_all(&chunk)?;
            if let Some(progress) = progress.as_deref_mut() {
                progress(chunk.len() as u64);
            }
        }
        output.sync_all()?;
        fs::rename(temporary, destination)?;
        Ok(())
    }
}

fn remote_ref(reference: &Reference, module: &str) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        reference.channel, reference.id, reference.version, reference.architecture, module
    )
}

fn archive_object_path(checksum: &Checksum, object_type: ObjectType) -> String {
    let checksum = checksum.to_hex();
    format!(
        "{}/{}.{}",
        &checksum[..2],
        &checksum[2..],
        object_type.extension(RepoMode::Archive)
    )
}

fn write_object(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("part");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)
}

fn clear_checkout(path: &Path) -> Result<(), std::io::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn success_data<T>(response: ApiResponse<T>) -> Result<Option<T>, RemoteError> {
    if response.code == 200 {
        return Ok(response.data);
    }
    Err(RemoteError::Api {
        code: response.code,
        message: response
            .msg
            .unwrap_or_else(|| "cannot send request to remote server".to_string()),
    })
}

fn connect_timeout_seconds() -> u64 {
    env::var("LINGLONG_CONNECT_TIMEOUT")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0 && *value <= i32::MAX as u64)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECONDS)
}

fn package_semantically_matches(package: &PackageInfoV2, fuzzy: &FuzzyReference) -> bool {
    let Some(architecture) = package.arch.first() else {
        return false;
    };
    let Ok(architecture) = architecture.parse() else {
        return false;
    };
    let Ok(version) = Version::parse(&package.version) else {
        return false;
    };
    let Ok(reference) = Reference::new(&package.channel, &package.id, version, architecture) else {
        return false;
    };
    reference.semantic_match(fuzzy)
}

fn package_version_is_older(left: &PackageInfoV2, right: &PackageInfoV2) -> bool {
    let left = Version::parse(&left.version);
    let right = Version::parse(&right.version);
    match (left, right) {
        (_, Err(_)) => false,
        (Err(_), Ok(_)) => true,
        (Ok(left), Ok(right)) => left < right,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    use tempfile::tempdir;

    use super::*;

    fn server(response: &'static str) -> (String, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let _ = sender.send(request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            )
            .unwrap();
        });
        (format!("http://{address}"), receiver)
    }

    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..count]);
            if let Some(header_end) = find_bytes(&request, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    return request;
                }
            }
        }
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn request_text(receiver: mpsc::Receiver<Vec<u8>>) -> String {
        String::from_utf8(receiver.recv().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn fuzzy_search_matches_generated_json_contract() {
        let response = r#"{"code":200,"data":[{"appId":"org.example.demo","arch":"x86_64","base":"org.deepin.base/23.1.0","channel":"main","description":"Demo","kind":"app","module":"binary","name":"Demo","runtime":"org.deepin.Runtime/23.1.0","size":42,"version":"1.2.3.4"}],"trace_id":"trace"}"#;
        let (base_url, receiver) = server(response);
        let client = RemoteRepositoryClient::new(base_url).unwrap();
        let packages = client
            .fuzzy_search(&FuzzySearchRequest {
                app_id: "org.example.demo".to_string(),
                arch: "x86_64".to_string(),
                channel: Some("main".to_string()),
                repo_name: "stable".to_string(),
                version: None,
            })
            .await
            .unwrap();
        assert_eq!(packages.len(), 1);
        let info = PackageInfoV2::from(packages[0].clone());
        assert_eq!(info.id, "org.example.demo");
        assert_eq!(info.module, "binary");

        let request = request_text(receiver);
        assert!(request.starts_with("POST /api/v0/apps/fuzzysearchapp HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("user-agent: linglong/1.14.0-dev")
        );
        let body = request.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            serde_json::json!({
                "appId": "org.example.demo",
                "arch": "x86_64",
                "channel": "main",
                "repoName": "stable"
            })
        );
    }

    #[tokio::test]
    #[ignore = "requires the public Linyaps repository"]
    async fn pulls_archive_z2_layer_from_public_repository() {
        let temporary = tempdir().unwrap();
        let repo = Repo {
            alias: None,
            mirror_enabled: None,
            name: "stable".to_string(),
            priority: 0,
            url: "https://mirror-repo-linglong.deepin.com".to_string(),
        };
        let reference = "main:org.deepin.calculator/6.5.37.1/x86_64"
            .parse::<Reference>()
            .unwrap();
        let pulled = RemoteRepositoryClient::new(&repo.url)
            .unwrap()
            .pull_layer(&reference, &repo, "binary", temporary.path())
            .await
            .unwrap();
        let info: PackageInfoV2 =
            serde_json::from_slice(&fs::read(pulled.path.join("info.json")).unwrap()).unwrap();
        assert_eq!(info.id, "org.deepin.calculator");
        assert_eq!(info.version, "6.5.37.1");
        assert_eq!(info.module, "binary");
    }

    #[tokio::test]
    async fn search_packages_applies_upstream_semantic_filter() {
        let response = r#"{"code":200,"data":[{"appId":"org.example.demo","arch":"x86_64","channel":"main","kind":"app","module":"binary","name":"Demo","version":"23.0.0.1"},{"appId":"org.example.demo.extra","arch":"x86_64","channel":"main","kind":"app","module":"binary","name":"Other","version":"23.0.0.1"}]}"#;
        let (base_url, receiver) = server(response);
        let client = RemoteRepositoryClient::new(base_url).unwrap();
        let packages = client
            .search_packages(
                &"main:org.example.demo/23.0.0/x86_64"
                    .parse::<FuzzyReference>()
                    .unwrap(),
                &Repo {
                    alias: Some("stable".to_string()),
                    mirror_enabled: None,
                    name: "origin".to_string(),
                    priority: 0,
                    url: "unused".to_string(),
                },
                true,
            )
            .await
            .unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].id, "org.example.demo");
        assert_eq!(packages[0].version, "23.0.0.1");
        let request = request_text(receiver);
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(value["repoName"], "origin");
        assert_eq!(value["version"], "23.0.0");
    }

    #[tokio::test]
    async fn authentication_and_upload_task_use_v1_contract() {
        let (base_url, sign_receiver) =
            server(r#"{"code":200,"data":{"token":"secret"},"msg":"ok"}"#);
        let sign_client = RemoteRepositoryClient::new(base_url).unwrap();
        assert_eq!(
            sign_client.sign_in("user", "password").await.unwrap().token,
            "secret"
        );
        let sign_request = request_text(sign_receiver);
        assert!(sign_request.starts_with("POST /api/v1/sign-in HTTP/1.1\r\n"));

        let (base_url, task_receiver) = server(r#"{"code":200,"data":{"id":"task-1"},"msg":"ok"}"#);
        let task_client = RemoteRepositoryClient::new(base_url).unwrap();
        assert_eq!(
            task_client
                .new_upload_task(
                    "secret",
                    "stable",
                    "main/org.example.demo/1.2.3.4/x86_64/binary",
                )
                .await
                .unwrap()
                .id,
            "task-1"
        );
        let task_request = request_text(task_receiver);
        assert!(task_request.starts_with("POST /api/v1/upload-tasks HTTP/1.1\r\n"));
        assert!(
            task_request
                .to_ascii_lowercase()
                .contains("x-token: secret")
        );
        assert!(task_request.contains("\"repoName\":\"stable\""));
    }

    #[tokio::test]
    async fn layer_upload_is_streamed_as_named_multipart_file() {
        let (base_url, receiver) = server(r#"{"code":200,"data":{"watchId":"watch"}}"#);
        let client = RemoteRepositoryClient::new(base_url).unwrap();
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("demo.tgz");
        std::fs::write(&path, b"layer-content").unwrap();
        client
            .upload_layer_file("secret", "task-1", &path)
            .await
            .unwrap();

        let request = request_text(receiver);
        assert!(request.starts_with("PUT /api/v1/upload-tasks/task-1/layer HTTP/1.1\r\n"));
        assert!(request.to_ascii_lowercase().contains("x-token: secret"));
        assert!(request.contains("name=\"file\""));
        assert!(request.contains("filename=\"demo.tgz\""));
        assert!(request.contains("layer-content"));
    }

    #[tokio::test]
    async fn upload_status_and_api_errors_are_preserved() {
        let (base_url, receiver) = server(r#"{"code":200,"data":{"status":"complete"}}"#);
        let client = RemoteRepositoryClient::new(base_url).unwrap();
        assert_eq!(
            client
                .upload_task_status("secret", "task-1")
                .await
                .unwrap()
                .status,
            "complete"
        );
        let request = request_text(receiver);
        assert!(request.starts_with("GET /api/v1/upload-tasks/task-1/status HTTP/1.1\r\n"));

        let (base_url, _) = server(r#"{"code":403,"msg":"denied"}"#);
        let client = RemoteRepositoryClient::new(base_url).unwrap();
        assert!(matches!(
            client.sign_in("user", "bad").await,
            Err(RemoteError::Api {
                code: 403,
                message
            }) if message == "denied"
        ));
    }

    #[test]
    fn timeout_parser_matches_upstream_bounds() {
        assert_eq!(DEFAULT_CONNECT_TIMEOUT_SECONDS, 5);
        for invalid in ["", "0", "-1", "not-a-number", "2147483648"] {
            assert_eq!(
                invalid
                    .parse::<u64>()
                    .ok()
                    .filter(|value| *value > 0 && *value <= i32::MAX as u64)
                    .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECONDS),
                DEFAULT_CONNECT_TIMEOUT_SECONDS
            );
        }
    }

    #[test]
    fn remote_packages_select_latest_and_collect_modules() {
        fn package(version: &str, module: &str) -> PackageInfoV2 {
            PackageInfoV2 {
                arch: vec!["x86_64".to_string()],
                base: String::new(),
                channel: "main".to_string(),
                command: None,
                compatible_version: None,
                description: None,
                extension_implementation: None,
                extensions: None,
                id: "org.example.demo".to_string(),
                kind: "app".to_string(),
                module: module.to_string(),
                name: "Demo".to_string(),
                permissions: None,
                runtime: None,
                schema_version: String::new(),
                size: 0,
                uuid: None,
                version: version.to_string(),
            }
        }
        let stable = Repo {
            alias: Some("stable".to_string()),
            mirror_enabled: None,
            name: "stable-origin".to_string(),
            priority: 100,
            url: "unused".to_string(),
        };
        let testing = Repo {
            alias: Some("testing".to_string()),
            mirror_enabled: None,
            name: "testing-origin".to_string(),
            priority: 0,
            url: "unused".to_string(),
        };
        let mut packages = RemotePackages::default();
        packages
            .add_packages(
                stable,
                vec![package("1.0.0.0", "binary"), package("1.0.0.0", "develop")],
            )
            .add_packages(testing.clone(), vec![package("2.0.0.0", "binary")]);
        let (repo, latest) = packages.latest_package().unwrap();
        assert_eq!(repo, testing);
        assert_eq!(latest.version, "2.0.0.0");

        let reference = Reference::new(
            "main",
            "org.example.demo",
            Version::parse("1.0.0.0").unwrap(),
            Architecture::X86_64,
        )
        .unwrap();
        assert_eq!(
            packages.reference_modules(&reference),
            ["binary".to_string(), "develop".to_string()]
        );
    }
}
