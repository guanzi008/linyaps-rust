use std::process::Command;

#[test]
fn preserves_frozen_no_argument_exit_status() {
    let status = Command::new(env!("CARGO_BIN_EXE_ll-init"))
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(139));
}

#[test]
fn propagates_exit_status_outside_pid_one() {
    let status = Command::new(env!("CARGO_BIN_EXE_ll-init"))
        .args(["/bin/sh", "-c", "exit 37"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(37));
}

#[test]
fn reports_signal_exit_status_outside_pid_one() {
    let status = Command::new(env!("CARGO_BIN_EXE_ll-init"))
        .args(["/bin/sh", "-c", "kill -TERM $$"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(143));
}

#[test]
fn reports_exec_failure_like_frozen_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_ll-init"))
        .arg("/definitely/missing")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "failed to exec for delegate run: No such file or directory\n"
    );
}

#[test]
fn verbose_delegate_messages_keep_frozen_order() {
    let output = Command::new(env!("CARGO_BIN_EXE_ll-init"))
        .env("LINYAPS_INIT_VERBOSE_OUTPUT", "1")
        .args(["/bin/sh", "-c", "exit 37"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(37));
    let stderr = String::from_utf8(output.stderr).unwrap();
    let done = stderr.find("delegate done").unwrap();
    let status = stderr.find("exited with status 37").unwrap();
    assert!(done < status, "{stderr}");
}
