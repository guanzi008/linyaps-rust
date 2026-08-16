use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use linyaps_api::OciConfigurationPatch;
use oci_spec::runtime::Spec;
use serde_json::Value;

use crate::apply_json_patch;

pub fn apply_oci_configuration_patches(
    configuration: &mut Value,
    app_id: &str,
    directory: &Path,
) -> Result<(), String> {
    if !directory.exists() {
        return Ok(());
    }
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "failed to iterate directory {}: {error}",
            directory.display()
        )
    })?;
    let mut global = Vec::new();
    let mut application = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        if path.is_file() {
            global.push(path);
        } else if path.is_dir() && entry.file_name() == app_id {
            let entries = fs::read_dir(&path).map_err(|error| {
                format!("failed to iterate directory {}: {error}", path.display())
            })?;
            for entry in entries {
                let path = entry.map_err(|error| error.to_string())?.path();
                if path.is_file() {
                    application.push(path);
                }
            }
        }
    }
    global.sort();
    application.sort();
    for path in global.into_iter().chain(application) {
        if let Err(error) = apply_oci_configuration_patch(configuration, &path) {
            eprintln!("skip applying failed patch {}: {error}", path.display());
        }
    }
    Ok(())
}

fn apply_oci_configuration_patch(configuration: &mut Value, path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "Failed to get status of patch file {}: {error}",
            path.display()
        )
    })?;
    let patched = if metadata.permissions().mode() & 0o111 != 0 {
        apply_executable_patch(configuration, path)?
    } else if path.extension() == Some(OsStr::new("json")) {
        let patch: OciConfigurationPatch = serde_json::from_slice(
            &fs::read(path)
                .map_err(|error| format!("Failed to open file {}: {error}", path.display()))?,
        )
        .map_err(|error| format!("Failed to apply JSON patch {}: {error}", path.display()))?;
        if configuration.get("ociVersion").and_then(Value::as_str)
            != Some(patch.oci_version.as_str())
        {
            return Err("ociVersion mismatched".to_string());
        }
        apply_json_patch(configuration, &patch.patch)?
    } else {
        return Err("Patch file is not an executable or a JSON patch file".to_string());
    };
    validate_oci_configuration(&patched)
        .map_err(|error| format!("patched config is not a valid OCI configuration: {error}"))?;
    *configuration = patched;
    Ok(())
}

fn validate_oci_configuration(configuration: &Value) -> Result<(), serde_json::Error> {
    let mut validation = configuration.clone();
    if let Some(process) = validation.get_mut("process").and_then(Value::as_object_mut) {
        process
            .entry("user")
            .or_insert_with(|| serde_json::json!({"uid": 0, "gid": 0}));
    }
    serde_json::from_value::<Spec>(validation).map(drop)
}

fn apply_executable_patch(configuration: &Value, path: &Path) -> Result<Value, String> {
    let input = serde_json::to_vec(configuration).map_err(|error| error.to_string())?;
    let mut child = Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to execute patch {}: {error}", path.display()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "patch stdin is unavailable".to_string())?;
    let writer = thread::spawn(move || stdin.write_all(&input));
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    let _ = writer.join();
    if !output.status.success() {
        return Err(format!(
            "Failed to execute patch {}: command execute failed with {}: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "Failed to process output from {}: {error}. Output: {}",
            path.display(),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn applies_global_then_application_json_and_executable_patches() {
        let temporary = tempfile::tempdir().unwrap();
        let app_directory = temporary.path().join("org.example.App");
        fs::create_dir(&app_directory).unwrap();
        fs::write(
            temporary.path().join("10-global.json"),
            serde_json::to_vec(&OciConfigurationPatch {
                oci_version: "1.0.1".to_string(),
                patch: vec![json!({"op":"add","path":"/order","value":["global"]})],
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            app_directory.join("20-app.json"),
            serde_json::to_vec(&OciConfigurationPatch {
                oci_version: "1.0.1".to_string(),
                patch: vec![json!({"op":"add","path":"/order/-","value":"app"})],
            })
            .unwrap(),
        )
        .unwrap();
        let executable = app_directory.join("30-executable");
        fs::write(
            &executable,
            "#!/bin/sh\ninput=$(cat)\nprintf '%s' \"${input%?}\"\nprintf ',\"executable\":true}'\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let mut configuration = json!({"ociVersion":"1.0.1"});
        apply_oci_configuration_patches(&mut configuration, "org.example.App", temporary.path())
            .unwrap();
        assert_eq!(
            configuration,
            json!({"ociVersion":"1.0.1","order":["global","app"],"executable":true})
        );
    }

    #[test]
    fn skips_failed_patch_and_continues() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(temporary.path().join("10-invalid.json"), "not json").unwrap();
        fs::write(
            temporary.path().join("20-valid.json"),
            serde_json::to_vec(&OciConfigurationPatch {
                oci_version: "1.0.1".to_string(),
                patch: vec![json!({"op":"add","path":"/applied","value":true})],
            })
            .unwrap(),
        )
        .unwrap();
        let mut configuration = json!({"ociVersion":"1.0.1"});
        apply_oci_configuration_patches(&mut configuration, "app", temporary.path()).unwrap();
        assert_eq!(configuration["applied"], true);
    }

    #[test]
    fn skips_patch_with_invalid_oci_field_types() {
        let temporary = tempfile::tempdir().unwrap();
        fs::write(
            temporary.path().join("10-invalid.json"),
            serde_json::to_vec(&OciConfigurationPatch {
                oci_version: "1.0.1".to_string(),
                patch: vec![json!({"op":"replace","path":"/process","value":"invalid"})],
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            temporary.path().join("20-valid.json"),
            serde_json::to_vec(&OciConfigurationPatch {
                oci_version: "1.0.1".to_string(),
                patch: vec![json!({"op":"add","path":"/applied","value":true})],
            })
            .unwrap(),
        )
        .unwrap();
        let mut configuration = json!({
            "ociVersion":"1.0.1",
            "process":{"args":["true"],"cwd":"/"}
        });
        apply_oci_configuration_patches(&mut configuration, "app", temporary.path()).unwrap();
        assert!(configuration["process"].is_object());
        assert_eq!(configuration["applied"], true);
    }
}
