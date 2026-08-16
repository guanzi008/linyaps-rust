use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use linyaps_api::RuntimeConfigure;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeConfigError {
    #[error("failed to read runtime config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse runtime config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

pub fn default_config_directories() -> Vec<PathBuf> {
    if let Some(value) = std::env::var_os("LINGLONG_RUNTIME_CONFIG_DIRS") {
        return std::env::split_paths(&value).collect();
    }
    let mut output = vec![PathBuf::from("/etc/linglong")];
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
    {
        output.push(config_home.join("linglong"));
    }
    output
}

pub fn load_runtime_config(
    app_id: &str,
    instance: &str,
) -> Result<Option<RuntimeConfigure>, RuntimeConfigError> {
    load_runtime_config_from(&default_config_directories(), app_id, instance)
}

pub fn load_runtime_config_from(
    directories: &[PathBuf],
    app_id: &str,
    instance: &str,
) -> Result<Option<RuntimeConfigure>, RuntimeConfigError> {
    let mut configs = Vec::new();
    for directory in directories {
        if directory.as_os_str().is_empty() {
            continue;
        }
        if let Some(config) = load_from_directory(directory, "")? {
            configs.push(config);
        }
        if !app_id.is_empty()
            && let Some(config) = load_from_directory(directory, app_id)?
        {
            configs.push(config);
        }
    }
    if configs.is_empty() {
        return Ok(None);
    }
    let mut merged = merge_runtime_configs(configs);
    if !instance.is_empty()
        && let Some(instance_config) = merged
            .instances
            .as_mut()
            .and_then(|instances| instances.remove(instance))
    {
        merged = merge_runtime_configs([merged, instance_config]);
    }
    merged.instances = None;
    Ok(Some(merged))
}

pub fn merge_runtime_configs(
    configs: impl IntoIterator<Item = RuntimeConfigure>,
) -> RuntimeConfigure {
    let mut result = RuntimeConfigure::default();
    for mut config in configs {
        if config.disable_xdp.is_some() {
            result.disable_xdp = config.disable_xdp;
        }
        if config.enable_pipewire.is_some() {
            result.enable_pipewire = config.enable_pipewire;
        }
        if config.enable_atspi.is_some() {
            result.enable_atspi = config.enable_atspi;
        }
        append(&mut result.device_mode, config.device_mode.take());
        append(&mut result.devices, config.devices.take());
        merge_map(&mut result.env, config.env.take());
        append_map_vectors(
            &mut result.extension_definitions,
            config.extension_definitions.take(),
        );
        append(&mut result.mounts, config.mounts.take());
        merge_instances(&mut result.instances, config.instances.take());
    }
    result
}

fn load_from_directory(
    directory: &Path,
    app_id: &str,
) -> Result<Option<RuntimeConfigure>, RuntimeConfigError> {
    let base = if app_id.is_empty() {
        directory.to_path_buf()
    } else {
        directory.join("apps").join(app_id)
    };
    let mut configs = Vec::new();
    let config = base.join("config.json");
    if config.exists() {
        configs.push(load_runtime_config_file(&config)?);
    }
    let drop_in = base.join("config.d");
    if drop_in.is_dir() {
        let mut paths = fs::read_dir(&drop_in)
            .map_err(|source| RuntimeConfigError::Read {
                path: drop_in.clone(),
                source,
            })?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|value| value == "json"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            configs.push(load_runtime_config_file(&path)?);
        }
    }
    Ok((!configs.is_empty()).then(|| merge_runtime_configs(configs)))
}

pub fn load_runtime_config_file(
    path: impl AsRef<Path>,
) -> Result<RuntimeConfigure, RuntimeConfigError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| RuntimeConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| RuntimeConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn append<T>(target: &mut Option<Vec<T>>, source: Option<Vec<T>>) {
    let Some(mut source) = source else {
        return;
    };
    target.get_or_insert_with(Vec::new).append(&mut source);
}

fn merge_map<K: Ord, V>(target: &mut Option<BTreeMap<K, V>>, source: Option<BTreeMap<K, V>>) {
    let Some(source) = source else {
        return;
    };
    target.get_or_insert_with(BTreeMap::new).extend(source);
}

fn append_map_vectors<K: Ord, V>(
    target: &mut Option<BTreeMap<K, Vec<V>>>,
    source: Option<BTreeMap<K, Vec<V>>>,
) {
    let Some(source) = source else {
        return;
    };
    let target = target.get_or_insert_with(BTreeMap::new);
    for (key, mut values) in source {
        target.entry(key).or_default().append(&mut values);
    }
}

fn merge_instances(
    target: &mut Option<BTreeMap<String, RuntimeConfigure>>,
    source: Option<BTreeMap<String, RuntimeConfigure>>,
) {
    let Some(source) = source else {
        return;
    };
    let target = target.get_or_insert_with(BTreeMap::new);
    for (name, config) in source {
        if let Some(previous) = target.remove(&name) {
            target.insert(name, merge_runtime_configs([previous, config]));
        } else {
            target.insert(name, config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linyaps_api::{DeviceOption, ExtensionDefine, Mount};

    fn mount(source: &str, destination: &str) -> Mount {
        Mount {
            destination: destination.to_string(),
            options: None,
            source: source.to_string(),
            source_type: None,
            mount_type: "bind".to_string(),
        }
    }

    fn extension(name: &str) -> ExtensionDefine {
        ExtensionDefine {
            allow_env: None,
            directory: format!("/opt/{name}"),
            name: name.to_string(),
            version: "1.0.0".to_string(),
        }
    }

    #[test]
    fn loads_runtime_config_from_path() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("config.json");
        fs::write(
            &path,
            r#"{
                "disable_xdp":true,
                "enable_pipewire":true,
                "enable_atspi":true,
                "device_mode":["passthru"],
                "env":{"PATH":"/usr/bin","HOME":"/home/user"},
                "ext_defs":{"test-app":[{
                    "name":"test-extension",
                    "version":"1.0.0",
                    "directory":"/opt/extension"
                }]}
            }"#,
        )
        .unwrap();

        let config = load_runtime_config_file(&path).unwrap();
        assert_eq!(config.disable_xdp, Some(true));
        assert_eq!(config.enable_pipewire, Some(true));
        assert_eq!(config.enable_atspi, Some(true));
        assert_eq!(config.device_mode, Some(vec![DeviceOption::Passthru]));
        assert_eq!(config.env.unwrap()["PATH"], "/usr/bin");
        assert_eq!(
            config.extension_definitions.unwrap()["test-app"][0].name,
            "test-extension"
        );
        assert!(load_runtime_config_file(temporary.path().join("missing.json")).is_err());
    }

    #[test]
    fn merges_every_runtime_config_field_in_upstream_order() {
        let first = RuntimeConfigure {
            device_mode: Some(vec![DeviceOption::Passthru]),
            devices: Some(vec!["vendor.com/device=gpu0".to_string()]),
            disable_xdp: Some(false),
            enable_atspi: Some(false),
            enable_pipewire: Some(false),
            env: Some(BTreeMap::from([
                ("HOME".to_string(), "/home/user1".to_string()),
                ("PATH".to_string(), "/usr/bin".to_string()),
            ])),
            extension_definitions: Some(BTreeMap::from([(
                "app1".to_string(),
                vec![extension("extension1")],
            )])),
            instances: None,
            mounts: Some(vec![mount("/host/a", "/tmp/a")]),
        };
        let second = RuntimeConfigure {
            device_mode: Some(vec![DeviceOption::Passthru]),
            devices: Some(vec!["vendor.com/device=gpu1".to_string()]),
            disable_xdp: Some(true),
            enable_atspi: Some(true),
            enable_pipewire: Some(true),
            env: Some(BTreeMap::from([
                ("PATH".to_string(), "/usr/local/bin".to_string()),
                ("USER".to_string(), "testuser".to_string()),
            ])),
            extension_definitions: Some(BTreeMap::from([
                ("app1".to_string(), vec![extension("extension2")]),
                ("app2".to_string(), vec![extension("extension2")]),
            ])),
            instances: None,
            mounts: Some(vec![mount("/host/b", "/tmp/b")]),
        };

        let merged = merge_runtime_configs([first, second]);
        assert_eq!(merged.disable_xdp, Some(true));
        assert_eq!(merged.enable_pipewire, Some(true));
        assert_eq!(merged.enable_atspi, Some(true));
        assert_eq!(merged.device_mode.unwrap().len(), 2);
        assert_eq!(merged.devices.unwrap().len(), 2);
        let environment = merged.env.unwrap();
        assert_eq!(environment["PATH"], "/usr/local/bin");
        assert_eq!(environment["HOME"], "/home/user1");
        assert_eq!(environment["USER"], "testuser");
        let extensions = merged.extension_definitions.unwrap();
        assert_eq!(extensions["app1"].len(), 2);
        assert_eq!(extensions["app2"].len(), 1);
        let mounts = merged.mounts.unwrap();
        assert_eq!(mounts[0].destination, "/tmp/a");
        assert_eq!(mounts[1].destination, "/tmp/b");
    }

    #[test]
    fn merges_instances_recursively() {
        let first = RuntimeConfigure {
            disable_xdp: Some(false),
            instances: Some(BTreeMap::from([
                (
                    "dev".to_string(),
                    RuntimeConfigure {
                        disable_xdp: Some(true),
                        env: Some(BTreeMap::from([("DEBUG".to_string(), "1".to_string())])),
                        mounts: Some(vec![mount("/host/instance-a", "/tmp/instance-a")]),
                        ..RuntimeConfigure::default()
                    },
                ),
                (
                    "prod".to_string(),
                    RuntimeConfigure {
                        disable_xdp: Some(false),
                        ..RuntimeConfigure::default()
                    },
                ),
            ])),
            ..RuntimeConfigure::default()
        };
        let second = RuntimeConfigure {
            instances: Some(BTreeMap::from([
                (
                    "dev".to_string(),
                    RuntimeConfigure {
                        env: Some(BTreeMap::from([("VERBOSE".to_string(), "1".to_string())])),
                        mounts: Some(vec![mount("/host/instance-b", "/tmp/instance-b")]),
                        ..RuntimeConfigure::default()
                    },
                ),
                (
                    "test".to_string(),
                    RuntimeConfigure {
                        disable_xdp: Some(true),
                        ..RuntimeConfigure::default()
                    },
                ),
            ])),
            ..RuntimeConfigure::default()
        };

        let merged = merge_runtime_configs([first, second]);
        let instances = merged.instances.unwrap();
        assert_eq!(instances.len(), 3);
        let dev = &instances["dev"];
        assert_eq!(dev.disable_xdp, Some(true));
        assert_eq!(dev.env.as_ref().unwrap().len(), 2);
        assert_eq!(dev.mounts.as_ref().unwrap().len(), 2);
        assert_eq!(instances["prod"].disable_xdp, Some(false));
        assert_eq!(instances["test"].disable_xdp, Some(true));
    }

    #[test]
    fn merging_no_configs_returns_empty_config() {
        assert_eq!(
            merge_runtime_configs(std::iter::empty()),
            RuntimeConfigure::default()
        );
    }

    #[test]
    fn loads_global_app_dropins_and_instance_in_order() {
        let temporary = tempfile::tempdir().unwrap();
        let system = temporary.path().join("system");
        let user = temporary.path().join("user");
        fs::create_dir_all(system.join("config.d")).unwrap();
        fs::create_dir_all(user.join("apps/demo/config.d")).unwrap();
        fs::write(
            system.join("config.json"),
            r#"{"env":{"ORDER":"system"},"devices":["vendor/device=system"]}"#,
        )
        .unwrap();
        fs::write(
            system.join("config.d/10-extra.json"),
            r#"{"env":{"GLOBAL":"yes"}}"#,
        )
        .unwrap();
        fs::write(
            user.join("apps/demo/config.json"),
            r#"{
                "env":{"ORDER":"app"},
                "mounts":[{"source":"/base","destination":"/base","type":"bind"}],
                "instances":{"dev":{"env":{"ORDER":"instance"},"enable_pipewire":true}}
            }"#,
        )
        .unwrap();
        fs::write(
            user.join("apps/demo/config.d/20-last.json"),
            r#"{"env":{"DROPIN":"yes"}}"#,
        )
        .unwrap();

        let config = load_runtime_config_from(&[system, user], "demo", "dev")
            .unwrap()
            .unwrap();
        let environment = config.env.unwrap();
        assert_eq!(environment["ORDER"], "instance");
        assert_eq!(environment["GLOBAL"], "yes");
        assert_eq!(environment["DROPIN"], "yes");
        assert_eq!(config.devices.unwrap(), ["vendor/device=system"]);
        assert_eq!(config.mounts.unwrap().len(), 1);
        assert_eq!(config.enable_pipewire, Some(true));
        assert!(config.instances.is_none());
    }
}
