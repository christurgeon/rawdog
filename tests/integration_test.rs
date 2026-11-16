//! Integration tests for rawdog CLI argument validation and file collection logic.
//!
//! These tests exercise the binary from the outside where feasible, and
//! validate end-to-end behavior with filesystem fixtures.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

/// Helper: path to the built binary.
fn rawdog_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_rawdog"))
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

/// Write a small PNG. imagepipe falls back to `image::open` for non-RAW files,
/// so an explicit PNG argument goes through the full convert path.
fn write_png(path: &std::path::Path, rgb: [u8; 3]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    image::RgbImage::from_pixel(8, 8, image::Rgb(rgb))
        .save(path)
        .unwrap();
}

fn files_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    if !dir.exists() {
        return Vec::new();
    }
    walkdir::WalkDir::new(dir)
        .into_iter()
        .map(|e| e.unwrap().into_path())
        .filter(|p| p.is_file())
        .collect()
}

#[test]
fn cli_output_collision_is_an_error_and_converts_nothing() {
    let tmp = TempDir::new().unwrap();
    let a = tmp.path().join("a/x.png");
    let b = tmp.path().join("b/x.png");
    write_png(&a, [200, 10, 10]);
    write_png(&b, [10, 200, 10]);
    let out = tmp.path().join("out");

    let output = Command::new(rawdog_bin())
        .arg(&a)
        .arg(&b)
        .arg("-o")
        .arg(&out)
        .output()
        .expect("failed to run rawdog");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "expected failure, stderr: {stderr}"
    );
    assert!(
        stderr.contains(&*a.to_string_lossy()) && stderr.contains(&*b.to_string_lossy()),
        "expected both conflicting inputs in error, got: {stderr}"
    );
    assert!(files_under(&out).is_empty(), "nothing should be converted");
}

#[test]
fn cli_failed_conversion_exits_nonzero_after_summary() {
    let tmp = TempDir::new().unwrap();
    let bad = tmp.path().join("bad.arw");
    fs::write(&bad, b"not a raw file").unwrap();

    let output = Command::new(rawdog_bin())
        .arg(&bad)
        .output()
        .expect("failed to run rawdog");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0 succeeded, 1 failed, 0 skipped"),
        "got: {stdout}"
    );
    assert!(!output.status.success(), "expected non-zero exit");
}

#[test]
fn cli_missing_input_exits_nonzero_but_converts_valid_inputs() {
    let tmp = TempDir::new().unwrap();
    let good = tmp.path().join("good.png");
    write_png(&good, [120, 120, 120]);
    let out = tmp.path().join("out");

    let output = Command::new(rawdog_bin())
        .arg(&good)
        .arg(tmp.path().join("missing.arw"))
        .arg("-o")
        .arg(&out)
        .output()
        .expect("failed to run rawdog");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does not exist"), "got: {stderr}");
    assert!(
        stdout.contains("1 succeeded, 0 failed, 0 skipped"),
        "got: {stdout}"
    );
    assert!(out.join("good.jpg").is_file());
    assert!(!output.status.success(), "expected non-zero exit");
}

#[test]
fn cli_overwrite_refuses_to_replace_its_own_input() {
    let tmp = TempDir::new().unwrap();
    let same = tmp.path().join("same.png");
    write_png(&same, [10, 20, 30]);
    let before = fs::read(&same).unwrap();

    let output = Command::new(rawdog_bin())
        .arg(&same)
        .args(["-f", "png", "--overwrite"])
        .output()
        .expect("failed to run rawdog");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("same file as the input"), "got: {stderr}");
    assert!(
        stdout.contains("0 succeeded, 1 failed, 0 skipped"),
        "got: {stdout}"
    );
    assert!(!output.status.success(), "expected non-zero exit");
    assert_eq!(
        fs::read(&same).unwrap(),
        before,
        "input must not be modified"
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

fn run(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(rawdog_bin())
        .args(args)
        .output()
        .expect("failed to run rawdog")
}

#[test]
fn cli_version_flag_prints_version() {
    let output = run(&["--version".as_ref()]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("rawdog "), "got: {stdout}");
}

#[test]
fn cli_format_aliases_are_accepted() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [50, 100, 150]);

    let output = run(&[input.as_ref(), "-f".as_ref(), "jpg".as_ref()]);
    assert!(output.status.success());
    assert!(tmp.path().join("in.jpg").is_file());

    let output = run(&[input.as_ref(), "-f".as_ref(), "tif".as_ref()]);
    assert!(output.status.success());
    assert!(tmp.path().join("in.tiff").is_file());
}

#[test]
fn cli_resize_zero_is_rejected() {
    let output = run(&["-r".as_ref(), "0".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn cli_resize_extreme_aspect_ratio_succeeds() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("wide.png");
    image::RgbImage::from_pixel(1000, 2, image::Rgb([9, 9, 9]))
        .save(&input)
        .unwrap();

    let output = run(&[input.as_ref(), "-r".as_ref(), "100".as_ref()]);
    assert!(output.status.success());
    let (w, h) = image::image_dimensions(tmp.path().join("wide.jpg")).unwrap();
    assert_eq!((w, h), (100, 1));
}

#[test]
fn cli_directory_named_like_arw_is_not_an_input() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join("folder.arw")).unwrap();

    let output = run(&[tmp.path().as_ref()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No ARW files"), "got: {stderr}");
}

#[test]
fn cli_leaves_no_temporary_files() {
    let tmp = TempDir::new().unwrap();
    let good = tmp.path().join("good.png");
    write_png(&good, [1, 2, 3]);
    let bad = tmp.path().join("bad.arw");
    fs::write(&bad, b"not a raw file").unwrap();

    run(&[good.as_ref(), bad.as_ref()]);

    let names: Vec<String> = files_under(tmp.path())
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(names.contains(&"good.jpg".to_string()), "got: {names:?}");
    assert!(
        names.iter().all(|n| !n.contains("rawdog-tmp")),
        "got: {names:?}"
    );
}

#[test]
fn cli_dry_run_lists_jobs_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let out = tmp.path().join("out");

    let output = run(&[input.as_ref(), "-o".as_ref(), out.as_ref(), "--dry-run".as_ref()]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("convert"), "got: {stdout}");
    assert!(stdout.contains("in.jpg"), "got: {stdout}");
    assert!(files_under(&out).is_empty());
}

#[test]
fn cli_dry_run_marks_existing_outputs_as_skipped() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    assert!(run(&[input.as_ref()]).status.success());

    let output = run(&[input.as_ref(), "-n".as_ref()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("skip"), "got: {stdout}");
}

#[test]
fn cli_jobs_limits_are_validated_and_applied() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);

    let output = run(&[input.as_ref(), "-j".as_ref(), "0".as_ref()]);
    assert_eq!(output.status.code(), Some(2));

    let output = run(&[input.as_ref(), "-j".as_ref(), "1".as_ref()]);
    assert!(output.status.success());
    assert!(tmp.path().join("in.jpg").is_file());
}

#[test]
fn cli_quiet_prints_nothing_on_success() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);

    let output = run(&[input.as_ref(), "--quiet".as_ref()]);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(tmp.path().join("in.jpg").is_file());
}

#[test]
fn cli_update_converts_only_when_input_is_newer() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let jpg = tmp.path().join("in.jpg");
    assert!(run(&[input.as_ref()]).status.success());

    // Output is newer than the input: skipped.
    let output = run(&[input.as_ref(), "--update".as_ref()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0 succeeded, 0 failed, 1 skipped"), "got: {stdout}");

    // Make the output older than the input: converted again.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    fs::File::options()
        .write(true)
        .open(&jpg)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let output = run(&[input.as_ref(), "--update".as_ref()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1 succeeded, 0 failed, 0 skipped"), "got: {stdout}");
}

#[test]
fn cli_fast_requires_resize() {
    let output = run(&["--fast".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn cli_fast_resizes_to_the_requested_long_edge() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("big.png");
    image::RgbImage::from_pixel(400, 200, image::Rgb([90, 90, 90]))
        .save(&input)
        .unwrap();
    let out = tmp.path().join("out");

    for (format, name) in [("jpeg", "big.jpg"), ("png", "big.png"), ("tiff", "big.tiff")] {
        let output = run(&[
            input.as_ref(),
            "-o".as_ref(),
            out.as_ref(),
            "-r".as_ref(),
            "100".as_ref(),
            "--fast".as_ref(),
            "-f".as_ref(),
            format.as_ref(),
        ]);
        assert!(output.status.success(), "{format}");
        let (w, h) = image::image_dimensions(out.join(name)).unwrap();
        assert_eq!((w, h), (100, 50), "{name}");
    }
}

#[test]
fn cli_copyright_and_artist_are_written_to_every_format() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let out = tmp.path().join("out");

    for (format, name) in [("jpeg", "in.jpg"), ("png", "in.png"), ("tiff", "in.tiff")] {
        let output = run(&[
            input.as_ref(),
            "-o".as_ref(),
            out.as_ref(),
            "-f".as_ref(),
            format.as_ref(),
            "--artist".as_ref(),
            "A. Tester".as_ref(),
            "--copyright".as_ref(),
            "(c) Test".as_ref(),
        ]);
        assert!(output.status.success(), "{format}");

        let file = fs::File::open(out.join(name)).unwrap();
        let parsed = exif::Reader::new()
            .read_from_container(&mut std::io::BufReader::new(file))
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let text = |tag| {
            parsed
                .get_field(tag, exif::In::PRIMARY)
                .unwrap_or_else(|| panic!("{name}: missing {tag}"))
                .display_value()
                .to_string()
        };
        assert_eq!(text(exif::Tag::Artist), "\"A. Tester\"", "{name}");
        assert_eq!(text(exif::Tag::Copyright), "\"(c) Test\"", "{name}");
    }
}
