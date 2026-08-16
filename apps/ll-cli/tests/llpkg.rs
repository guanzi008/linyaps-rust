use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn llpkg_executes_ll_cli_with_unchanged_arguments() {
    let temporary = tempfile::tempdir().unwrap();
    let output = temporary.path().join("arguments");
    let shim = temporary.path().join("ll-cli");
    fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 23\n",
            output.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_llpkg"))
        .args(["install", "org.example.App/1.0.0", "--force"])
        .env("PATH", temporary.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(23));
    assert_eq!(
        fs::read_to_string(output).unwrap(),
        "install\norg.example.App/1.0.0\n--force\n"
    );
}
