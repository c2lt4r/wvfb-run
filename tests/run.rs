//! End-to-end: run the built wvfb-run.exe and check the exit codes we promise.

use std::process::{Command, Output};
use std::time::{Duration, Instant};

fn wvfb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wvfb-run"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn passes_through_exit_code_and_output() {
    let o = wvfb(&["cmd", "/c", "echo hello& exit 3"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&o.stdout).contains("hello"));
}

#[test]
fn timeout_kills_with_124() {
    let start = Instant::now();
    let o = wvfb(&["--timeout", "1", "ping", "-n", "30", "127.0.0.1"]);
    assert_eq!(o.status.code(), Some(124));
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[test]
fn dialog_kills_with_125() {
    let o = wvfb(&[
        "--timeout",
        "20",
        "powershell",
        "-NoProfile",
        "-c",
        "Add-Type -A System.Windows.Forms; [System.Windows.Forms.MessageBox]::Show('boom', 'wvfb-test')",
    ]);
    assert_eq!(o.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&o.stderr).contains("boom"));
}

#[test]
fn missing_program_is_127() {
    assert_eq!(wvfb(&["wvfb-no-such-program"]).status.code(), Some(127));
}

#[test]
fn quoted_program_path_is_rejected() {
    let o = wvfb(&[r#""C:\Windows\System32\cmd.exe""#, "/c", "exit 0"]);
    assert_eq!(o.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&o.stderr).contains("literal quotes"));
}
