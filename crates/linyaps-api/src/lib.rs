use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplicationConfiguration {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<ApplicationConfigurationPermissions>,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplicationConfigurationPermissionsBind {
    pub destination: String,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplicationConfigurationPermissionsInnerBind {
    pub destination: String,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct XdgDirectoryPermission {
    pub allowed: bool,
    #[serde(rename = "dirType")]
    pub directory_type: String,
}

pub type XdgDirectoryPermissions = Vec<XdgDirectoryPermission>;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplicationPermissionsRequest {
    #[serde(rename = "appID")]
    pub app_id: String,
    #[serde(rename = "xdgDirectories")]
    pub xdg_directories: XdgDirectoryPermissions,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplicationConfigurationPermissions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binds: Option<Vec<ApplicationConfigurationPermissionsBind>>,
    #[serde(rename = "innerBinds", skip_serializing_if = "Option::is_none")]
    pub inner_binds: Option<Vec<ApplicationConfigurationPermissionsInnerBind>>,
    #[serde(rename = "xdgDirectories", skip_serializing_if = "Option::is_none")]
    pub xdg_directories: Option<Vec<XdgDirectoryPermission>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExtensionDefine {
    #[serde(rename = "allow_env", skip_serializing_if = "Option::is_none")]
    pub allow_env: Option<BTreeMap<String, String>>,
    pub directory: String,
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceNode {
    #[serde(rename = "hostPath", skip_serializing_if = "Option::is_none")]
    pub host_path: Option<String>,
    pub path: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExtensionImplementation {
    #[serde(rename = "deviceNodes", skip_serializing_if = "Option::is_none")]
    pub device_nodes: Option<Vec<DeviceNode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libs: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Mount {
    pub destination: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    pub source: String,
    #[serde(rename = "src_type", skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    #[serde(rename = "type")]
    pub mount_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CdiSpec {
    pub checksum: String,
    pub path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CdiDeviceEntry {
    pub kind: String,
    pub name: String,
    pub spec: CdiSpec,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceOption {
    Passthru,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DialogHandShakePayload {
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DialogMessage {
    pub payload: String,
    #[serde(rename = "type")]
    pub message_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExportDirs {
    #[serde(rename = "export-paths")]
    pub export_paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(i32)]
pub enum InteractionMessageType {
    Downgrade = 0,
    Install = 1,
    Uninstall = 2,
    Unknown = 3,
    Upgrade = 4,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InteractionRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<String>>,
    #[serde(rename = "appName")]
    pub app_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    pub summary: String,
    pub timeout: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OciConfigurationPatch {
    #[serde(rename = "ociVersion")]
    pub oci_version: String,
    pub patch: Vec<Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeConfigure {
    #[serde(rename = "device_mode", skip_serializing_if = "Option::is_none")]
    pub device_mode: Option<Vec<DeviceOption>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devices: Option<Vec<String>>,
    #[serde(rename = "disable_xdp", skip_serializing_if = "Option::is_none")]
    pub disable_xdp: Option<bool>,
    #[serde(rename = "enable_atspi", skip_serializing_if = "Option::is_none")]
    pub enable_atspi: Option<bool>,
    #[serde(rename = "enable_pipewire", skip_serializing_if = "Option::is_none")]
    pub enable_pipewire: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(rename = "ext_defs", skip_serializing_if = "Option::is_none")]
    pub extension_definitions: Option<BTreeMap<String, Vec<ExtensionDefine>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instances: Option<BTreeMap<String, RuntimeConfigure>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<Mount>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Repo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(rename = "mirror_enabled", skip_serializing_if = "Option::is_none")]
    pub mirror_enabled: Option<bool>,
    pub name: String,
    pub priority: i64,
    pub url: String,
}
impl Repo {
    pub fn effective_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepoConfig {
    #[serde(rename = "defaultRepo")]
    pub default_repo: String,
    pub repos: BTreeMap<String, String>,
    pub version: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepoConfigV2 {
    #[serde(rename = "defaultRepo")]
    pub default_repo: String,
    pub repos: Vec<Repo>,
    pub version: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PackageInfo {
    pub appid: String,
    pub arch: Vec<String>,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub kind: String,
    pub module: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<ApplicationConfigurationPermissions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    pub size: i64,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PackageInfoV2 {
    pub arch: Vec<String>,
    pub base: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(rename = "compatible_version", skip_serializing_if = "Option::is_none")]
    pub compatible_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "ext_impl", skip_serializing_if = "Option::is_none")]
    pub extension_implementation: Option<ExtensionImplementation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<ExtensionDefine>>,
    pub id: String,
    pub kind: String,
    #[serde(rename = "module")]
    pub module: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<ApplicationConfigurationPermissions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(rename = "schema_version")]
    pub schema_version: String,
    pub size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PackageInfoDisplay {
    pub arch: Vec<String>,
    pub base: String,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(rename = "compatible_version", skip_serializing_if = "Option::is_none")]
    pub compatible_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "ext_impl", skip_serializing_if = "Option::is_none")]
    pub extension_implementation: Option<ExtensionImplementation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<ExtensionDefine>>,
    pub id: String,
    pub kind: String,
    #[serde(rename = "module")]
    pub module: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<ApplicationConfigurationPermissions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(rename = "schema_version")]
    pub schema_version: String,
    pub size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub version: String,
    #[serde(rename = "install_time", skip_serializing_if = "Option::is_none")]
    pub install_time: Option<i64>,
}

impl From<PackageInfoV2> for PackageInfoDisplay {
    fn from(info: PackageInfoV2) -> Self {
        Self {
            arch: info.arch,
            base: info.base,
            channel: info.channel,
            command: info.command,
            compatible_version: info.compatible_version,
            description: info.description,
            extension_implementation: info.extension_implementation,
            extensions: info.extensions,
            id: info.id,
            kind: info.kind,
            module: info.module,
            name: info.name,
            permissions: info.permissions,
            runtime: info.runtime,
            schema_version: info.schema_version,
            size: info.size,
            uuid: info.uuid,
            version: info.version,
            install_time: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LayerInfo {
    pub info: Value,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UabLayer {
    pub info: PackageInfoV2,
    pub minified: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UabSections {
    pub bundle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UabMetaInfo {
    pub digest: String,
    pub layers: Vec<UabLayer>,
    #[serde(rename = "onlyApp", skip_serializing_if = "Option::is_none")]
    pub only_app: Option<bool>,
    pub sections: UabSections,
    pub uuid: String,
    #[serde(
        deserialize_with = "deserialize_uab_version",
        serialize_with = "serialize_uab_version"
    )]
    pub version: String,
}

fn deserialize_uab_version<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let version = String::deserialize(deserializer)?;
    if version == "1" {
        Ok(version)
    } else {
        Err(serde::de::Error::custom(
            "UAB metadata version must be \"1\"",
        ))
    }
}

fn serialize_uab_version<S>(version: &String, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if version == "1" {
        serializer.serialize_str(version)
    } else {
        Err(serde::ser::Error::custom(
            "UAB metadata version must be \"1\"",
        ))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InspectResult {
    #[serde(rename = "appID", skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CliContainer {
    pub id: String,
    pub package: String,
    pub pid: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContainerProcessStateInfo {
    pub app: String,
    pub base: String,
    #[serde(rename = "containerID")]
    pub container_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpgradeListResult {
    pub id: String,
    #[serde(rename = "new_version")]
    pub new_version: String,
    #[serde(rename = "old_version")]
    pub old_version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommonOptions {
    pub force: bool,
    #[serde(rename = "noAutoPrune", skip_serializing_if = "Option::is_none")]
    pub no_auto_prune: Option<bool>,
    #[serde(rename = "skipInteraction")]
    pub skip_interaction: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommonResult {
    pub code: i64,
    pub message: String,
    #[serde(rename = "type")]
    pub result_type: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(i32)]
pub enum TaskStateName {
    Canceled = 0,
    Failed = 1,
    Pending = 2,
    Processing = 3,
    Queued = 4,
    Succeed = 5,
    Unknown = 6,
}

pub type State = TaskStateName;

impl TaskStateName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Queued => "Queued",
            Self::Pending => "Pending",
            Self::Processing => "Processing",
            Self::Succeed => "Succeed",
            Self::Failed => "Failed",
            Self::Canceled => "Canceled",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TaskState {
    pub message: String,
    pub progress: f64,
    pub state: TaskStateName,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerPackage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub id: String,
    #[serde(rename = "module", skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerInstallPackage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modules: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerInstallParameters {
    pub options: CommonOptions,
    pub package: PackageManagerInstallPackage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerUninstallParameters {
    pub options: CommonOptions,
    pub package: PackageManagerPackage,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerUpdateParameters {
    #[serde(rename = "depsOnly")]
    pub deps_only: bool,
    #[serde(rename = "noAutoPrune", skip_serializing_if = "Option::is_none")]
    pub no_auto_prune: Option<bool>,
    pub packages: Vec<PackageManagerPackage>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerSearchParameters {
    pub id: String,
    pub repos: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerTaskResult {
    pub code: i64,
    pub message: String,
    #[serde(rename = "taskObjectPath", skip_serializing_if = "Option::is_none")]
    pub task_object_path: Option<String>,
    #[serde(rename = "type")]
    pub result_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerJobInfo {
    pub code: i64,
    pub id: String,
    pub message: String,
    #[serde(rename = "type", default)]
    pub result_type: String,
}

pub type PackageManager1JobInfo = PackageManagerJobInfo;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PackageManagerSearchResult {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<BTreeMap<String, Vec<PackageInfoV2>>>,
    #[serde(rename = "type")]
    pub result_type: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PackageManagerPruneResult {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<Vec<PackageInfoV2>>,
    #[serde(rename = "type")]
    pub result_type: String,
}

pub type PackageManager1InstallLayerFdResult = CommonResult;
pub type PackageManager1ModifyRepoResult = CommonResult;
pub type PackageManager1PackageTaskResult = PackageManagerTaskResult;
pub type PackageManager1PruneResult = PackageManagerPruneResult;
pub type PackageManager1SearchResult = PackageManagerSearchResult;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManager1ModifyRepoParameters {
    #[serde(rename = "defaultRepo")]
    pub default_repo: String,
    pub repos: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManager1GetRepoInfoResultRepoInfo {
    #[serde(rename = "defaultRepo")]
    pub default_repo: String,
    pub repos: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManager1GetRepoInfoResult {
    pub code: i64,
    pub message: String,
    #[serde(rename = "repoInfo")]
    pub repo_info: PackageManager1GetRepoInfoResultRepoInfo,
    #[serde(rename = "type")]
    pub result_type: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InteractionReply {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PackageManagerInteractionMessage {
    #[serde(rename = "LocalRef")]
    pub local_ref: String,
    #[serde(rename = "RemoteRef")]
    pub remote_ref: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RepositoryCacheLayersItem {
    pub commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    pub info: PackageInfoV2,
    pub repo: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepositoryCacheMergedItem {
    #[serde(rename = "binaryCommit", skip_serializing_if = "Option::is_none")]
    pub binary_commit: Option<String>,
    pub commits: Vec<String>,
    pub id: String,
    pub modules: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RepositoryCache {
    pub config: RepoConfigV2,
    pub layers: Vec<RepositoryCacheLayersItem>,
    #[serde(rename = "ll-version")]
    pub ll_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged: Option<Vec<RepositoryCacheMergedItem>>,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BuilderProjectPackage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    pub description: String,
    #[serde(rename = "deviceNodes", skip_serializing_if = "Option::is_none")]
    pub device_nodes: Option<Vec<DeviceNode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(rename = "extension_of", skip_serializing_if = "Option::is_none")]
    pub extension_of: Option<String>,
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libs: Option<Vec<String>>,
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuilderProjectSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submodules: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuilderProjectModule {
    pub files: Vec<String>,
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuilderProjectApt {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_depends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuilderProjectBuildExt {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apt: Option<BuilderProjectApt>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BuilderProject {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    pub build: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buildext: Option<BuilderProjectBuildExt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modules: Option<Vec<BuilderProjectModule>>,
    pub package: BuilderProjectPackage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<ApplicationConfigurationPermissions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<BuilderProjectSource>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strip: Option<String>,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuilderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline: Option<bool>,
    pub repo: String,
    pub version: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunContextConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(rename = "cdiDevices", skip_serializing_if = "Option::is_none")]
    pub cdi_devices: Option<Vec<CdiDeviceEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<BTreeMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<Mount>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlayfs: Option<String>,
    #[serde(rename = "resolvConf", skip_serializing_if = "Option::is_none")]
    pub resolv_conf: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    pub version: String,
}

pub type CDIDeviceEntry = CdiDeviceEntry;
pub type CLIContainer = CliContainer;
pub type ExtensionImpl = ExtensionImplementation;
pub type OCIConfigurationPatch = OciConfigurationPatch;
pub type PackageManager1InstallParameters = PackageManagerInstallParameters;
pub type PackageManager1Package = PackageManagerPackage;
pub type PackageManager1RequestInteractionAdditionalMessage = PackageManagerInteractionMessage;
pub type PackageManager1SearchParameters = PackageManagerSearchParameters;
pub type PackageManager1UninstallParameters = PackageManagerUninstallParameters;
pub type PackageManager1UpdateParameters = PackageManagerUpdateParameters;
pub type Sections = UabSections;
pub type UABLayer = UabLayer;
pub type UABMetaInfo = UabMetaInfo;
pub type XDGDirectoryPermissions = XdgDirectoryPermissions;
pub type PackageManager1InstallLayerFDResult = CommonResult;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_info_uses_schema_field_names() {
        let value = serde_json::json!({
            "arch": ["x86_64"],
            "base": "org.deepin.base/23.1.0",
            "channel": "main",
            "id": "org.deepin.demo",
            "kind": "app",
            "module": "binary",
            "name": "Demo",
            "schema_version": "1.0",
            "size": 10,
            "version": "1.0.0.0"
        });
        let info: PackageInfoV2 = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(info.module, "binary");
        assert_eq!(serde_json::to_value(info).unwrap(), value);
    }

    #[test]
    fn display_package_adds_only_install_time() {
        let value = serde_json::json!({
            "arch": ["x86_64"],
            "base": "org.deepin.base/23.1.0",
            "channel": "main",
            "id": "org.deepin.demo",
            "kind": "app",
            "module": "binary",
            "name": "Demo",
            "schema_version": "1.0",
            "size": 10,
            "version": "1.0.0.0"
        });
        let info: PackageInfoV2 = serde_json::from_value(value.clone()).unwrap();
        let mut display = PackageInfoDisplay::from(info);
        assert_eq!(serde_json::to_value(&display).unwrap(), value);
        display.install_time = Some(123);
        assert_eq!(serde_json::to_value(display).unwrap()["install_time"], 123);
    }

    #[test]
    fn process_state_uses_original_container_id_key() {
        let state = ContainerProcessStateInfo {
            app: "main:org.example.demo/1.0.0.0/x86_64".to_string(),
            base: "main:org.deepin.base/23.1.0/x86_64".to_string(),
            container_id: "container".to_string(),
            extensions: None,
            runtime: None,
        };
        let value = serde_json::to_value(state).unwrap();
        assert_eq!(value["containerID"], "container");
        assert!(value.get("container_id").is_none());
    }

    #[test]
    fn upgrade_result_uses_snake_case_version_keys() {
        let value = serde_json::to_value(UpgradeListResult {
            id: "org.example.demo".to_string(),
            new_version: "2.0.0.0".to_string(),
            old_version: "1.0.0.0".to_string(),
        })
        .unwrap();
        assert_eq!(value["new_version"], "2.0.0.0");
        assert_eq!(value["old_version"], "1.0.0.0");
    }

    #[test]
    fn repo_uses_mirror_enabled_key() {
        let repo = Repo {
            alias: Some("stable".to_string()),
            mirror_enabled: Some(true),
            name: "main".to_string(),
            priority: 100,
            url: "https://example.invalid".to_string(),
        };
        assert_eq!(serde_json::to_value(repo).unwrap()["mirror_enabled"], true);
    }

    #[test]
    fn repository_cache_uses_persistent_field_names() {
        let cache = RepositoryCache {
            config: RepoConfigV2 {
                default_repo: "stable".to_string(),
                repos: Vec::new(),
                version: 2,
            },
            layers: Vec::new(),
            ll_version: "1.14.0".to_string(),
            merged: Some(vec![RepositoryCacheMergedItem {
                binary_commit: Some("abc".to_string()),
                commits: vec!["abc".to_string()],
                id: "demo".to_string(),
                modules: vec!["binary".to_string()],
                name: None,
            }]),
            version: "2".to_string(),
        };
        let value = serde_json::to_value(cache).unwrap();
        assert_eq!(value["ll-version"], "1.14.0");
        assert_eq!(value["merged"][0]["binaryCommit"], "abc");
        assert!(value["merged"][0].get("name").is_none());
    }

    #[test]
    fn package_manager_parameters_use_dbus_schema_names() {
        let parameters = PackageManagerInstallParameters {
            options: CommonOptions {
                force: true,
                no_auto_prune: Some(true),
                skip_interaction: false,
            },
            package: PackageManagerInstallPackage {
                channel: None,
                id: "org.example.demo".to_string(),
                modules: Some(vec!["binary".to_string()]),
                version: None,
            },
            repo: Some("stable".to_string()),
        };
        let value = serde_json::to_value(parameters).unwrap();
        assert_eq!(value["options"]["skipInteraction"], false);
        assert_eq!(value["options"]["noAutoPrune"], true);
        assert_eq!(value["package"]["modules"][0], "binary");
        assert!(value["package"].get("channel").is_none());
    }

    #[test]
    fn task_protocol_models_match_generated_json() {
        let state = TaskState {
            message: "searching".to_string(),
            progress: 50.0,
            state: TaskStateName::Processing,
        };
        assert_eq!(serde_json::to_value(state).unwrap()["state"], "Processing");

        let result = PackageManagerTaskResult {
            code: 0,
            message: "queued".to_string(),
            task_object_path: Some("/org/deepin/linglong/Task1/id".to_string()),
            result_type: String::new(),
        };
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["taskObjectPath"], "/org/deepin/linglong/Task1/id");
        assert_eq!(value["type"], "");
    }

    #[test]
    fn interaction_message_preserves_legacy_capitalization() {
        let value = serde_json::to_value(PackageManagerInteractionMessage {
            local_ref: "old".to_string(),
            remote_ref: "new".to_string(),
        })
        .unwrap();
        assert_eq!(value["LocalRef"], "old");
        assert_eq!(value["RemoteRef"], "new");
    }

    #[test]
    fn legacy_schema_models_preserve_wire_keys() {
        let permissions = ApplicationPermissionsRequest {
            app_id: "org.example.demo".to_string(),
            xdg_directories: vec![XdgDirectoryPermission {
                allowed: true,
                directory_type: "Documents".to_string(),
            }],
        };
        let value = serde_json::to_value(permissions).unwrap();
        assert_eq!(value["appID"], "org.example.demo");
        assert_eq!(value["xdgDirectories"][0]["dirType"], "Documents");

        let exports = serde_json::to_value(ExportDirs {
            export_paths: vec!["entries/applications".to_string()],
        })
        .unwrap();
        assert_eq!(exports["export-paths"][0], "entries/applications");

        let patch = serde_json::to_value(OciConfigurationPatch {
            oci_version: "1.0.2".to_string(),
            patch: vec![serde_json::json!({"op": "add"})],
        })
        .unwrap();
        assert_eq!(patch["ociVersion"], "1.0.2");
    }

    #[test]
    fn legacy_package_and_repo_results_round_trip() {
        let package = PackageInfo {
            appid: "org.example.demo".to_string(),
            arch: vec!["x86_64".to_string()],
            base: "org.deepin.base/23.1.0".to_string(),
            channel: Some("main".to_string()),
            command: None,
            description: None,
            kind: "app".to_string(),
            module: "binary".to_string(),
            name: "Demo".to_string(),
            permissions: None,
            runtime: None,
            size: 1,
            version: "1.0.0.0".to_string(),
        };
        assert_eq!(
            serde_json::to_value(package).unwrap()["appid"],
            "org.example.demo"
        );

        let result = PackageManager1GetRepoInfoResult {
            code: 0,
            message: String::new(),
            repo_info: PackageManager1GetRepoInfoResultRepoInfo {
                default_repo: "stable".to_string(),
                repos: BTreeMap::from([(
                    "stable".to_string(),
                    "https://example.invalid".to_string(),
                )]),
            },
            result_type: String::new(),
        };
        let value = serde_json::to_value(result).unwrap();
        assert_eq!(value["repoInfo"]["defaultRepo"], "stable");
        assert_eq!(value["type"], "");
    }

    #[test]
    fn interaction_reply_accepts_missing_action() {
        let reply: InteractionReply = serde_json::from_str("{}").unwrap();
        assert_eq!(reply.action, None);
        assert_eq!(serde_json::to_string(&reply).unwrap(), "{}");
    }

    #[test]
    fn enum_discriminants_match_quicktype_generated_api() {
        assert_eq!(InteractionMessageType::Downgrade as i32, 0);
        assert_eq!(InteractionMessageType::Upgrade as i32, 4);
        assert_eq!(TaskStateName::Canceled as i32, 0);
        assert_eq!(TaskStateName::Unknown as i32, 6);
    }

    #[test]
    fn uab_metadata_only_accepts_schema_version_one() {
        let mut value = serde_json::json!({
            "digest": "sha256:demo",
            "layers": [],
            "sections": {"bundle": "linglong.bundle"},
            "uuid": "00000000-0000-4000-8000-000000000000",
            "version": "1"
        });
        serde_json::from_value::<UabMetaInfo>(value.clone()).unwrap();
        value["version"] = serde_json::json!("2");
        assert!(serde_json::from_value::<UabMetaInfo>(value).is_err());
    }
}
