//! Integration tests for rawdog CLI argument validation and file collection logic.
//!
//! These tests exercise the binary from the outside where feasible, and
//! validate end-to-end behavior with filesystem fixtures.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

/// Helper: path to the built binary.
fn rawdog_bin() -> std::path::PathBuf {
    // `cargo test` places the binary in target/debug
    let mut path = std::env::current_exe()
        .unwrap()
        .parent() // deps/
        .unwrap()
        .parent() // debug/
        .unwrap()
        .to_path_buf();
    path.push("rawdog");
    path
}

#[test]
fn cli_no_args_shows_error() {
    let output = Command::new(rawdog_bin())
        .output()
        .expect("failed to run rawdog");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    // clap should complain about missing required argument
    assert!(
        stderr.contains("required") || stderr.contains("Usage"),
        "Expected usage/required error, got: {stderr}"
    );
}

#[test]
fn cli_nonexistent_input_reports_no_arw_files() {
    let output = Command::new(rawdog_bin())
        .arg("/nonexistent/path")
        .output()
        .expect("failed to run rawdog");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("No ARW files") || combined.contains("does not exist"),
        "Expected missing-file message, got: {combined}"
    );
}

#[test]
fn cli_empty_directory_reports_no_arw_files() {
    let tmp = TempDir::new().unwrap();
    let output = Command::new(rawdog_bin())
        .arg(tmp.path())
        .output()
        .expect("failed to run rawdog");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("No ARW files"),
        "Expected 'No ARW files' message, got: {combined}"
    );
}

#[test]
fn cli_directory_with_only_non_arw_files() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("photo.jpg"), b"fake").unwrap();
    fs::write(tmp.path().join("photo.cr2"), b"fake").unwrap();

    let output = Command::new(rawdog_bin())
        .arg(tmp.path())
        .output()
        .expect("failed to run rawdog");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("No ARW files"),
        "Expected 'No ARW files', got: {combined}"
    );
}

#[test]
fn cli_invalid_quality_value() {
    let output = Command::new(rawdog_bin())
        .args(["--quality", "0", "/tmp/fake.arw"])
        .output()
        .expect("failed to run rawdog");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value") || stderr.contains("not in"),
        "Expected validation error, got: {stderr}"
    );
}

#[test]
fn cli_invalid_format_value() {
    let output = Command::new(rawdog_bin())
        .args(["--format", "bmp", "/tmp/fake.arw"])
        .output()
        .expect("failed to run rawdog");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid value") || stderr.contains("possible values"),
        "Expected invalid format error, got: {stderr}"
    );
}

#[test]
fn cli_help_flag_shows_usage() {
    let output = Command::new(rawdog_bin())
        .arg("--help")
        .output()
        .expect("failed to run rawdog");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Raw files in, images out"));
    assert!(stdout.contains("--format"));
    assert!(stdout.contains("--quality"));
    assert!(stdout.contains("--output"));
}
