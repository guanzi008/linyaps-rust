use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use linyaps_api::PackageInfoV2;
use serde_json::Value;

#[test]
fn loader_generates_oci_bundle_and_invokes_ll_box() {
    let temporary = tempfile::tempdir().unwrap();
    let bundle = temporary.path().join("bundle");
    let module = bundle.join("layers/org.example.App/binary");
    let extra = bundle.join("extra");
    fs::create_dir_all(module.join("files")).unwrap();
    fs::create_dir_all(&extra).unwrap();
    let loader = bundle.join("loader");
    fs::copy(env!("CARGO_BIN_EXE_uab-loader"), &loader).unwrap();
    fs::set_permissions(&loader, fs::Permissions::from_mode(0o755)).unwrap();
    let info = PackageInfoV2 {
        arch: vec!["x86_64".to_string()],
        base: "org.deepin.Base/23.1.0".to_string(),
        channel: "main".to_string(),
        command: Some(vec!["/bin/demo".to_string()]),
        compatible_version: None,
        description: None,
        extension_implementation: None,
        extensions: None,
        id: "org.example.App".to_string(),
        kind: "app".to_string(),
        module: "binary".to_string(),
        name: "Example".to_string(),
        permissions: None,
        runtime: None,
        schema_version: "1.0".to_string(),
        size: 0,
        uuid: None,
        version: "1.0.0".to_string(),
    };
    fs::write(module.join("info.json"), serde_json::to_vec(&info).unwrap()).unwrap();
    let box_binary = extra.join("ll-box");
    fs::write(
        &box_binary,
        "#!/bin/sh\nbundle=\nprintf 'BOXARGS'\nfor argument in \"$@\"; do\n  printf '|%s' \"$argument\"\n  case \"$argument\" in --bundle=*) bundle=${argument#--bundle=} ;; esac\ndone\nprintf '\\nBUNDLE=%s\\nCONFIG\\n' \"$bundle\"\ncat \"$bundle/config.json\"\nprintf 'ENTRYPOINT\\n'\ncat \"$bundle/entrypoint.sh\"\nprintf '\\nEND\\n'\nexit 27\n",
    )
    .unwrap();
    fs::set_permissions(&box_binary, fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(&loader)
        .args(["--flag", "value with space", "quote's"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(27));
    assert!(String::from_utf8_lossy(&output.stderr).contains("loader: container exit: 27"));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("BOXARGS|--cgroup-manager=disabled|run|--bundle="));
    assert!(stdout.contains("|--config=config.json|"));
    let bundle_path = stdout
        .lines()
        .find_map(|line| line.strip_prefix("BUNDLE="))
        .unwrap();
    let config_text = stdout
        .split_once("CONFIG\n")
        .unwrap()
        .1
        .split_once("ENTRYPOINT\n")
        .unwrap()
        .0;
    let config: Value = serde_json::from_str(config_text).unwrap();
    assert_eq!(config["ociVersion"], "1.0.1");
    assert_eq!(config["root"]["path"], "rootfs");
    assert_eq!(config["linux"]["rootfsPropagation"], "slave");
    assert_eq!(
        config["process"]["args"],
        serde_json::json!(["/entrypoint.sh"])
    );
    assert!(
        config["process"]["env"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "LINGLONG_APPID=org.example.App")
    );
    let application_source = module.join("files").to_string_lossy().into_owned();
    assert!(config["mounts"].as_array().unwrap().iter().any(|mount| {
        mount["destination"] == "/opt/apps/org.example.App/files"
            && mount["source"] == application_source
    }));
    let entrypoint = stdout
        .split_once("ENTRYPOINT\n")
        .unwrap()
        .1
        .split_once("\nEND")
        .unwrap()
        .0;
    assert!(entrypoint.contains("exec '/bin/demo' '--flag' 'value with space' 'quote'\\''s' "));
    assert!(!Path::new(bundle_path).exists());
}
