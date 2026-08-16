use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use linyaps_api::{PackageInfoV2, UabLayer, UabMetaInfo, UabSections};
use linyaps_repository::{append_elf_sections, build_erofs_image};
use sha2::{Digest, Sha256};

fn fixture(directory: &tempfile::TempDir) -> (std::path::PathBuf, UabMetaInfo) {
    let tree = directory.path().join("tree");
    let files = tree.join("layers/org.example.App/binary/files");
    fs::create_dir_all(&files).unwrap();
    fs::write(files.join("payload"), "application-data").unwrap();
    fs::write(
        tree.join("loader"),
        "#!/bin/sh\nprintf 'root=%s\\narg=%s\\nonly=%s\\n' \"$LINGLONG_UAB_APPROOT\" \"$1\" \"$LINGLONG_UAB_LOADER_ONLY_APP\"\nexit 19\n",
    )
    .unwrap();
    fs::set_permissions(tree.join("loader"), fs::Permissions::from_mode(0o755)).unwrap();
    let bundle = build_erofs_image(&tree).unwrap();
    let metadata = UabMetaInfo {
        digest: format!("{:x}", Sha256::digest(&bundle)),
        layers: vec![UabLayer {
            info: PackageInfoV2 {
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
                size: 16,
                uuid: None,
                version: "1.0.0".to_string(),
            },
            minified: false,
        }],
        only_app: Some(true),
        sections: UabSections {
            bundle: "linglong.bundle".to_string(),
            icon: None,
        },
        uuid: "integration-uab".to_string(),
        version: "1".to_string(),
    };
    let metadata_bytes = serde_json::to_vec(&metadata).unwrap();
    let output = directory.path().join("application.uab");
    append_elf_sections(
        env!("CARGO_BIN_EXE_uab-header"),
        &output,
        &[
            ("linglong.bundle", bundle.as_slice()),
            ("linglong.meta", metadata_bytes.as_slice()),
        ],
    )
    .unwrap();
    (output, metadata)
}

#[test]
fn appended_header_prints_metadata_and_extracts() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    let (uab, metadata) = fixture(&directory);
    let output = Command::new(&uab).arg("--print-meta").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<UabMetaInfo>(&output.stdout).unwrap(),
        metadata
    );
    let destination = directory.path().join("extracted");
    let output = Command::new(&uab)
        .arg(format!("--extract={}", destination.display()))
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(destination.join("layers/org.example.App/binary/files/payload"))
            .unwrap(),
        "application-data"
    );
}

#[test]
fn default_execution_runs_loader_and_preserves_status() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    fs::create_dir(&runtime).unwrap();
    let (uab, _) = fixture(&directory);
    let output = Command::new(&uab)
        .args(["--", "loader-value"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(19));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("arg=loader-value"));
    assert!(stdout.contains("only=true"));
    assert!(stdout.contains("root=".to_string().as_str()));
    assert!(stdout.contains("/linglong/UAB/integration-uab/layers/org.example.App/binary/files"));
    assert!(!runtime.join("linglong/UAB/integration-uab").exists());
}

#[test]
fn mount_mode_restores_hidden_directory_on_signal() {
    let directory = tempfile::tempdir().unwrap();
    let (uab, _) = fixture(&directory);
    let mount = directory.path().join("mount");
    fs::create_dir(&mount).unwrap();
    fs::write(mount.join("original"), "kept").unwrap();
    let mut child = Command::new(&uab)
        .arg(format!("--mount={}", mount.display()))
        .spawn()
        .unwrap();
    let mounted_file = mount.join("layers/org.example.App/binary/files/payload");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !mounted_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(mounted_file.exists());
    assert!(!mount.join("original").exists());
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(143));
    assert_eq!(fs::read_to_string(mount.join("original")).unwrap(), "kept");
    assert!(!mount.join("layers").exists());
}
