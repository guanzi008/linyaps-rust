use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::{Command, Output};

use linyaps_api::{BuilderConfig, PackageInfoV2};
use linyaps_core::Architecture;
use linyaps_repository::write_layer_file;
use sha2::{Digest, Sha256};
use tempfile::tempdir;

fn run(current_directory: &Path, config: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ll-builder"))
        .current_dir(current_directory)
        .env("LINGLONG_BUILDER_CONFIG", config)
        .env("LINGLONG_OCI_RUNTIME", "/bin/true")
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn version_and_create_match_expected_shape() {
    let temporary = tempdir().unwrap();
    let config = temporary.path().join("unused.yaml");
    let output = run(temporary.path(), &config, &["--version"]);
    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("linyaps build tool version "));

    let output = run(temporary.path(), &config, &["create", "org.example.Demo"]);
    assert_success(&output);
    let project =
        fs::read_to_string(temporary.path().join("org.example.Demo/linglong.yaml")).unwrap();
    assert!(project.contains("id: org.example.Demo"));
    assert!(project.contains("version: 0.0.0.1"));
}

#[test]
fn no_subcommand_checks_runtime_and_project_before_help() {
    let temporary = tempdir().unwrap();
    let repository = temporary.path().join("repository");
    let config = temporary.path().join("builder.yaml");
    fs::write(
        &config,
        serde_yml::to_string(&BuilderConfig {
            arch: None,
            cache: None,
            offline: None,
            repo: repository.to_string_lossy().into_owned(),
            version: 1,
        })
        .unwrap(),
    )
    .unwrap();

    let missing_project = run(temporary.path(), &config, &[]);
    assert_eq!(missing_project.status.code(), Some(255));
    assert!(missing_project.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&missing_project.stderr),
        "the project file is not found\n"
    );

    let architecture = Architecture::current().unwrap();
    fs::write(
        temporary
            .path()
            .join(format!("linglong.{architecture}.yaml")),
        r#"version: "1"
package:
  id: org.example.App
  name: Demo
  version: 1.0.0.0
  kind: app
  architecture: x86_64
  description: Demo
command: [demo]
base: org.deepin.base/23.1.0
build: demo
"#,
    )
    .unwrap();
    let help = run(temporary.path(), &config, &[]);
    assert_success(&help);
    assert!(String::from_utf8_lossy(&help.stdout).contains("--skip-run-container"));
    assert!(String::from_utf8_lossy(&help.stderr).starts_with("Using project file "));

    let no_runtime = Command::new(env!("CARGO_BIN_EXE_ll-builder"))
        .current_dir(temporary.path())
        .env("LINGLONG_BUILDER_CONFIG", &config)
        .env("LINGLONG_OCI_RUNTIME", "missing-builder-runtime")
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(no_runtime.status.code(), Some(255));
    assert_eq!(
        String::from_utf8_lossy(&no_runtime.stderr),
        "missing-builder-runtime not found\n"
    );
}

#[test]
fn layer_and_offline_build_commands_round_trip() {
    let temporary = tempdir().unwrap();
    let repository = temporary.path().join("repository");
    let config = temporary.path().join("builder.yaml");
    fs::write(
        &config,
        serde_yml::to_string(&BuilderConfig {
            arch: None,
            cache: None,
            offline: Some(true),
            repo: repository.to_string_lossy().into_owned(),
            version: 1,
        })
        .unwrap(),
    )
    .unwrap();
    let architecture = Architecture::current().unwrap().to_string();
    let base = temporary.path().join("base");
    fs::create_dir_all(base.join("files/bin")).unwrap();
    fs::write(base.join("files/bin/bash"), "base-shell").unwrap();
    let base_info = PackageInfoV2 {
        arch: vec![architecture.clone()],
        base: String::new(),
        channel: "main".to_string(),
        command: None,
        compatible_version: None,
        description: Some("Base".to_string()),
        extension_implementation: None,
        extensions: None,
        id: "org.deepin.base".to_string(),
        kind: "base".to_string(),
        module: "binary".to_string(),
        name: "Base".to_string(),
        permissions: None,
        runtime: None,
        schema_version: "1.0".to_string(),
        size: 10,
        uuid: None,
        version: "23.1.0.0".to_string(),
    };
    fs::write(
        base.join("info.json"),
        serde_json::to_vec(&base_info).unwrap(),
    )
    .unwrap();
    let base_layer = temporary.path().join("base.layer");
    write_layer_file(&base, &base_info, &base_layer).unwrap();
    let output = run(
        temporary.path(),
        &config,
        &["import", base_layer.to_str().unwrap()],
    );
    assert_success(&output);

    fs::write(
        temporary.path().join("linglong.yaml"),
        format!(
            r#"version: "1"
package:
  id: org.example.App
  name: Demo
  version: 1.0.0.0
  kind: app
  architecture: {architecture}
  description: Demo
command: [/opt/apps/org.example.App/files/bin/demo]
base: org.deepin.base/23.1.0
build: demo
"#
        ),
    )
    .unwrap();
    let build_output = temporary.path().join("linglong/output/_build/bin");
    fs::create_dir_all(&build_output).unwrap();
    fs::write(build_output.join("demo"), "application").unwrap();
    let output = run(
        temporary.path(),
        &config,
        &[
            "build",
            "--offline",
            "--skip-run-container",
            "--skip-output-check",
        ],
    );
    assert_success(&output);

    let output = run(temporary.path(), &config, &["list"]);
    assert_success(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("org.example.App"));

    let app = temporary.path().join("app");
    fs::create_dir_all(app.join("files/bin")).unwrap();
    fs::write(app.join("files/bin/demo"), "application").unwrap();
    let app_info = PackageInfoV2 {
        arch: vec![architecture.clone()],
        base: "org.deepin.base/23.1.0".to_string(),
        channel: "main".to_string(),
        command: Some(vec!["/opt/apps/org.example.App/files/bin/demo".to_string()]),
        compatible_version: None,
        description: Some("Demo".to_string()),
        extension_implementation: None,
        extensions: None,
        id: "org.example.App".to_string(),
        kind: "app".to_string(),
        module: "binary".to_string(),
        name: "Demo".to_string(),
        permissions: None,
        runtime: None,
        schema_version: "1.0".to_string(),
        size: 11,
        uuid: None,
        version: "1.0.0.0".to_string(),
    };
    fs::write(
        app.join("info.json"),
        serde_json::to_vec(&app_info).unwrap(),
    )
    .unwrap();
    let app_layer = temporary.path().join("app.layer");
    write_layer_file(&app, &app_info, &app_layer).unwrap();
    let output = run(
        temporary.path(),
        &config,
        &["import", app_layer.to_str().unwrap()],
    );
    assert_success(&output);

    let output = run(temporary.path(), &config, &["list"]);
    assert_success(&output);
    let list = String::from_utf8_lossy(&output.stdout);
    let reference = format!("main:org.example.App/1.0.0.0/{architecture}");
    assert!(list.contains(&reference));

    let output = run(
        temporary.path(),
        &config,
        &["export", "--layer", "--no-develop"],
    );
    assert_success(&output);
    let layer = temporary.path().join(format!(
        "org.example.App_1.0.0.0_{architecture}_binary.layer"
    ));
    assert!(layer.is_file());
    let extracted = temporary.path().join("extracted");
    let output = run(
        temporary.path(),
        &config,
        &[
            "extract",
            layer.to_str().unwrap(),
            extracted.to_str().unwrap(),
        ],
    );
    assert_success(&output);
    assert_eq!(
        fs::read(extracted.join("files/bin/demo")).unwrap(),
        b"application"
    );

    let output = run(temporary.path(), &config, &["remove", &reference]);
    assert_success(&output);
    let output = run(temporary.path(), &config, &["list"]);
    assert_success(&output);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("org.example.App"));
}

#[test]
fn legacy_helpers_dispatch_by_installed_file_name() {
    let temporary = tempdir().unwrap();
    let fetch = temporary.path().join("fetch-file-source");
    symlink(env!("CARGO_BIN_EXE_ll-builder"), &fetch).unwrap();
    let source = temporary.path().join("source");
    fs::write(&source, b"legacy helper payload").unwrap();
    let digest = format!("{:x}", Sha256::digest(b"legacy helper payload"));
    let output = temporary.path().join("output");
    let result = Command::new(&fetch)
        .arg(&output)
        .arg(format!("file://{}", source.display()))
        .arg(&digest)
        .arg(temporary.path().join("cache"))
        .output()
        .unwrap();
    assert_success(&result);
    assert_eq!(fs::read(output).unwrap(), b"legacy helper payload");

    let config_check = temporary.path().join("config-check.sh");
    symlink(env!("CARGO_BIN_EXE_ll-builder"), &config_check).unwrap();
    let files = temporary.path().join("app/files");
    let applications = files.join("share/applications");
    fs::create_dir_all(&applications).unwrap();
    fs::write(applications.join("org.example.App.desktop"), "valid").unwrap();
    let result = Command::new(&config_check)
        .env("LINGLONG_APPID", "org.example.App")
        .env("LINGLONG_HELPER_FILES", &files)
        .output()
        .unwrap();
    assert_success(&result);

    fs::write(applications.join("invalid.desktop"), "invalid").unwrap();
    let result = Command::new(&config_check)
        .env("LINGLONG_APPID", "org.example.App")
        .env("LINGLONG_HELPER_FILES", &files)
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stdout).contains("invalid.desktop"));

    let ldd_check = temporary.path().join("ldd-check.sh");
    symlink(env!("CARGO_BIN_EXE_ll-builder"), &ldd_check).unwrap();
    let binaries = temporary.path().join("checked/bin");
    fs::create_dir_all(&binaries).unwrap();
    fs::copy("/bin/true", binaries.join("true")).unwrap();
    let depends = temporary.path().join("depends.yaml");
    let result = Command::new(&ldd_check)
        .arg(temporary.path().join("checked"))
        .env("LINGLONG_DEPENDS_OUTPUT", &depends)
        .output()
        .unwrap();
    assert_success(&result);
    assert!(fs::read_to_string(depends).unwrap().contains("depends:"));
}
