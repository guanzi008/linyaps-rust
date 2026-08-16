use std::fs;
use std::path::{Path, PathBuf};

use linyaps_api::{CdiDeviceEntry, CdiSpec};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CdiError {
    #[error("invalid device format: {0}")]
    InvalidDevice(String),
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    #[error("failed to read CDI spec {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse CDI spec {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("CDI spec checksum mismatch: {0}")]
    ChecksumMismatch(PathBuf),
    #[error("CDI device kind mismatch: expected {expected}, got {actual}")]
    KindMismatch { expected: String, actual: String },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct ContainerEdits {
    #[serde(rename = "additionalGids")]
    pub additional_gids: Option<Vec<u32>>,
    #[serde(rename = "deviceNodes")]
    pub device_nodes: Option<Vec<CdiDeviceNode>>,
    pub env: Option<Vec<String>>,
    pub hooks: Option<Vec<CdiHook>>,
    pub mounts: Option<Vec<CdiMount>>,
    #[serde(rename = "netDevices")]
    pub network_devices: Option<Vec<CdiNetworkDevice>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CdiDeviceNode {
    pub gid: Option<u32>,
    #[serde(rename = "hostPath")]
    pub host_path: Option<String>,
    pub major: Option<i64>,
    pub minor: Option<i64>,
    pub path: String,
    pub permissions: Option<String>,
    #[serde(rename = "type")]
    pub node_type: Option<String>,
    pub uid: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CdiMount {
    #[serde(rename = "containerPath")]
    pub container_path: String,
    #[serde(rename = "hostPath")]
    pub host_path: String,
    pub options: Option<Vec<String>>,
    #[serde(rename = "type")]
    pub mount_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CdiHook {
    pub args: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    #[serde(rename = "hookName")]
    pub hook_name: String,
    pub path: String,
    pub timeout: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CdiNetworkDevice {
    #[serde(rename = "hostInterfaceName")]
    pub host_interface_name: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CdiFile {
    #[serde(rename = "cdiVersion")]
    _version: String,
    #[serde(rename = "containerEdits")]
    container_edits: Option<ContainerEdits>,
    devices: Vec<CdiDevice>,
    kind: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CdiDevice {
    #[serde(rename = "containerEdits")]
    container_edits: ContainerEdits,
    name: String,
}

struct ParsedSpec {
    checksum: String,
    path: PathBuf,
    spec: CdiFile,
}

pub fn get_devices(
    spec_directories: &[PathBuf],
    requested: Option<&[String]>,
) -> Result<Vec<CdiDeviceEntry>, CdiError> {
    let specs = read_specs(spec_directories);
    if requested.is_none() {
        return Ok(specs
            .iter()
            .flat_map(|parsed| {
                parsed.spec.devices.iter().map(|device| CdiDeviceEntry {
                    kind: parsed.spec.kind.clone(),
                    name: device.name.clone(),
                    spec: CdiSpec {
                        checksum: parsed.checksum.clone(),
                        path: parsed.path.to_string_lossy().into_owned(),
                    },
                })
            })
            .collect());
    }

    let mut output = Vec::new();
    for raw in requested.unwrap_or_default() {
        let parts = raw.split('=').collect::<Vec<_>>();
        if parts.len() != 2 {
            return Err(CdiError::InvalidDevice(raw.clone()));
        }
        let kind = parts[0];
        let name = parts[1];
        let found = specs.iter().find(|parsed| {
            parsed.spec.kind == kind && parsed.spec.devices.iter().any(|device| device.name == name)
        });
        let Some(parsed) = found else {
            return Err(CdiError::DeviceNotFound(raw.clone()));
        };
        output.push(CdiDeviceEntry {
            kind: kind.to_string(),
            name: name.to_string(),
            spec: CdiSpec {
                checksum: parsed.checksum.clone(),
                path: parsed.path.to_string_lossy().into_owned(),
            },
        });
    }
    Ok(output)
}

pub fn get_device_edits(device: &CdiDeviceEntry) -> Result<ContainerEdits, CdiError> {
    let path = Path::new(&device.spec.path);
    let bytes = fs::read(path).map_err(|source| CdiError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let checksum = checksum(&bytes);
    if !device.spec.checksum.is_empty() && checksum != device.spec.checksum {
        return Err(CdiError::ChecksumMismatch(path.to_path_buf()));
    }
    let spec = parse_spec(path, &bytes)?;
    if spec.kind != device.kind {
        return Err(CdiError::KindMismatch {
            expected: device.kind.clone(),
            actual: spec.kind,
        });
    }
    let local = spec
        .devices
        .into_iter()
        .find(|candidate| candidate.name == device.name)
        .ok_or_else(|| CdiError::DeviceNotFound(format!("{}={}", device.kind, device.name)))?;
    Ok(merge_edits(
        spec.container_edits.unwrap_or_default(),
        local.container_edits,
    ))
}

fn read_specs(directories: &[PathBuf]) -> Vec<ParsedSpec> {
    let mut specs = Vec::new();
    for directory in directories {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            let Ok(spec) = parse_spec(&path, &bytes) else {
                continue;
            };
            specs.push(ParsedSpec {
                checksum: checksum(&bytes),
                path,
                spec,
            });
        }
    }
    specs
}

fn parse_spec(path: &Path, bytes: &[u8]) -> Result<CdiFile, CdiError> {
    let extension = path.extension().and_then(|value| value.to_str());
    match extension {
        Some("json") => serde_json::from_slice(bytes).map_err(|error| CdiError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
        Some("yaml" | "yml") => {
            let value = std::str::from_utf8(bytes).map_err(|error| CdiError::Parse {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
            serde_yml::from_str(value).map_err(|error| CdiError::Parse {
                path: path.to_path_buf(),
                message: error.to_string(),
            })
        }
        _ => Err(CdiError::Parse {
            path: path.to_path_buf(),
            message: "invalid file extension".to_string(),
        }),
    }
}

fn checksum(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn merge_edits(mut global: ContainerEdits, local: ContainerEdits) -> ContainerEdits {
    append(&mut global.additional_gids, local.additional_gids);
    append(&mut global.device_nodes, local.device_nodes);
    append(&mut global.env, local.env);
    append(&mut global.hooks, local.hooks);
    append(&mut global.mounts, local.mounts);
    append(&mut global.network_devices, local.network_devices);
    global
}

fn append<T>(target: &mut Option<Vec<T>>, source: Option<Vec<T>>) {
    let Some(mut source) = source else {
        return;
    };
    target.get_or_insert_with(Vec::new).append(&mut source);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_devices_merges_edits_and_checks_digest() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("vendor.json");
        fs::write(
            &path,
            r#"{
                "cdiVersion":"0.6.0",
                "kind":"vendor.example/device",
                "containerEdits":{"env":["GLOBAL=yes"]},
                "devices":[{
                    "name":"all",
                    "containerEdits":{
                        "env":["LOCAL=yes"],
                        "mounts":[{"hostPath":"/host","containerPath":"/container"}]
                    }
                }]
            }"#,
        )
        .unwrap();
        let entries = get_devices(
            &[temporary.path().to_path_buf()],
            Some(&["vendor.example/device=all".to_string()]),
        )
        .unwrap();
        assert_eq!(entries.len(), 1);
        let edits = get_device_edits(&entries[0]).unwrap();
        assert_eq!(edits.env.unwrap(), ["GLOBAL=yes", "LOCAL=yes"]);
        fs::write(path, "{}").unwrap();
        assert!(matches!(
            get_device_edits(&entries[0]),
            Err(CdiError::ChecksumMismatch(_))
        ));
    }
}
