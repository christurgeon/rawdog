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
        combined.contains("No RAW files") || combined.contains("does not exist"),
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
        combined.contains("No RAW files"),
        "Expected 'No RAW files' message, got: {combined}"
    );
}

#[test]
fn cli_directory_with_only_non_arw_files() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("photo.jpg"), b"fake").unwrap();
    fs::write(tmp.path().join("photo.xmp"), b"fake").unwrap();

    let output = Command::new(rawdog_bin())
        .arg(tmp.path())
        .output()
        .expect("failed to run rawdog");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{stderr}{stdout}");
    assert!(
        combined.contains("No RAW files"),
        "Expected 'No RAW files', got: {combined}"
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
    assert!(stderr.contains("No RAW files"), "got: {stderr}");
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
        // The decoder rounds the short edge, so it can be one pixel smaller.
        assert_eq!(w, 100, "{name}");
        assert!((49..=50).contains(&h), "{name}: height {h}");
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

/// Mean value of each channel of an image file.
fn channel_means(path: &std::path::Path) -> [f64; 3] {
    let img = image::open(path).unwrap().to_rgb8();
    let n = (img.width() * img.height()) as f64;
    let mut sums = [0.0; 3];
    for p in img.pixels() {
        for c in 0..3 {
            sums[c] += p[c] as f64;
        }
    }
    sums.map(|s| s / n)
}

fn convert_grey_with(args: &[&str]) -> [f64; 3] {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("grey.png");
    // The decoder's full pipeline needs more than a few pixels per side.
    image::RgbImage::from_pixel(64, 64, image::Rgb([100, 100, 100]))
        .save(&input)
        .unwrap();
    let mut all: Vec<&std::ffi::OsStr> = vec![input.as_ref()];
    all.extend(args.iter().map(|a| a.as_ref() as &std::ffi::OsStr));
    let output = run(&all);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    channel_means(&tmp.path().join("grey.jpg"))
}

#[test]
fn cli_exposure_brightens_and_darkens() {
    let normal = convert_grey_with(&[])[1];
    let up = convert_grey_with(&["--exposure", "1"])[1];
    let down = convert_grey_with(&["--exposure", "-1"])[1];
    assert!((normal - 100.0).abs() < 3.0, "normal {normal}");
    assert!(up > normal + 20.0, "up {up} vs {normal}");
    assert!(down < normal - 20.0, "down {down} vs {normal}");
    assert!(up < 255.0);
}

#[test]
fn cli_exposure_range_is_validated() {
    let output = run(&["--exposure".as_ref(), "9".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn cli_wb_shifts_colour_in_the_expected_direction() {
    // A low kelvin setting corrects for warm light, so the result is bluer.
    let [r_low, _, b_low] = convert_grey_with(&["--wb", "3000"]);
    let [r_high, _, b_high] = convert_grey_with(&["--wb", "9000"]);
    assert!(b_low > r_low, "3000 K: r {r_low} b {b_low}");
    assert!(r_high > b_high, "9000 K: r {r_high} b {b_high}");
}

#[test]
fn cli_tint_requires_wb() {
    let output = run(&["--tint".as_ref(), "1.2".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn cli_name_template_places_output_and_rejects_bad_templates() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let out = tmp.path().join("out");

    let output = run(&[
        input.as_ref(),
        "-o".as_ref(),
        out.as_ref(),
        "--name".as_ref(),
        "web/{stem}_small".as_ref(),
    ]);
    assert!(output.status.success());
    assert!(out.join("web/in_small.jpg").is_file());

    for bad in ["../{stem}", "{nope}", "/abs/{stem}"] {
        let output = run(&[input.as_ref(), "--name".as_ref(), bad.as_ref()]);
        assert_eq!(output.status.code(), Some(2), "{bad}");
    }
}

#[test]
fn cli_sharpen_accepts_no_value_or_a_value_in_range() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    image::RgbImage::from_fn(32, 32, |x, _| image::Rgb([if x < 16 { 60 } else { 180 }; 3]))
        .save(&input)
        .unwrap();
    let plain = tmp.path().join("plain");
    let sharp = tmp.path().join("sharp");

    assert!(run(&[input.as_ref(), "-f".as_ref(), "png".as_ref(), "-o".as_ref(), plain.as_ref()])
        .status
        .success());
    // The flag without a value must not consume the next argument.
    assert!(run(&[
        input.as_ref(),
        "-f".as_ref(),
        "png".as_ref(),
        "-o".as_ref(),
        sharp.as_ref(),
        "--sharpen".as_ref(),
    ])
    .status
    .success());

    let px = |dir: &std::path::Path, x| image::open(dir.join("in.png")).unwrap().to_rgb8().get_pixel(x, 8)[0];
    assert!(px(&sharp, 15) < px(&plain, 15));
    assert!(px(&sharp, 16) > px(&plain, 16));

    let output = run(&[input.as_ref(), "--sharpen".as_ref(), "9".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn cli_lists_failed_files_when_more_than_one_fails() {
    let tmp = TempDir::new().unwrap();
    for name in ["a.arw", "b.arw"] {
        fs::write(tmp.path().join(name), b"not a raw file").unwrap();
    }
    let output = run(&[tmp.path().as_ref()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let list = stderr.split("Failed files:").nth(1).expect("no failure list");
    assert!(list.contains("a.arw") && list.contains("b.arw"), "got: {stderr}");
    assert!(!output.status.success());
}

#[test]
fn cli_dry_run_reports_a_refused_job() {
    let tmp = TempDir::new().unwrap();
    let same = tmp.path().join("same.png");
    write_png(&same, [1, 2, 3]);

    let output = run(&[
        same.as_ref(),
        "-f".as_ref(),
        "png".as_ref(),
        "--overwrite".as_ref(),
        "-n".as_ref(),
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("refuse"), "got: {stdout}");
    assert!(stdout.contains("same file"), "got: {stdout}");
    assert!(!output.status.success());
}

#[test]
fn cli_output_that_is_a_file_is_rejected_before_any_work() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let not_a_dir = tmp.path().join("afile");
    fs::write(&not_a_dir, b"x").unwrap();

    for extra in [None, Some("-n")] {
        let mut args: Vec<&std::ffi::OsStr> = vec![input.as_ref(), "-o".as_ref(), not_a_dir.as_ref()];
        args.extend(extra.map(|e| e.as_ref() as &std::ffi::OsStr));
        let output = run(&args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not a directory"), "got: {stderr}");
        assert!(!output.status.success());
    }
}

#[test]
fn cli_exposure_accepts_a_negative_value_without_leading_zero() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    image::RgbImage::from_pixel(64, 64, image::Rgb([100, 100, 100]))
        .save(&input)
        .unwrap();
    let output = run(&[input.as_ref(), "-e".as_ref(), "-.5".as_ref()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_tint_above_one_is_more_magenta() {
    // Magenta is red and blue above green. Green is the reference channel
    // of the white balance, so the shift shows in red and blue.
    let magenta_cast = |args: &[&str]| {
        let [r, g, b] = convert_grey_with(args);
        (r + b) / 2.0 - g
    };
    let high = magenta_cast(&["--wb", "5500", "--tint", "1.25"]);
    let neutral = magenta_cast(&["--wb", "5500"]);
    let low = magenta_cast(&["--wb", "5500", "--tint", "0.8"]);
    assert!(high > neutral + 3.0, "{high} vs {neutral}");
    assert!(low < neutral - 3.0, "{low} vs {neutral}");
}

#[test]
fn cli_preview_conflicts_with_development_options() {
    for other in [&["--fast", "-r", "100"][..], &["--exposure", "1"], &["--wb", "5000"]] {
        let mut args: Vec<&std::ffi::OsStr> = vec!["--preview".as_ref(), "/tmp/fake.arw".as_ref()];
        args.extend(other.iter().map(|a| a.as_ref() as &std::ffi::OsStr));
        assert_eq!(run(&args).status.code(), Some(2), "{other:?}");
    }
}

#[test]
fn cli_preview_fails_cleanly_for_a_file_without_a_preview() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let output = run(&[input.as_ref(), "--preview".as_ref()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("preview"), "got: {stderr}");
    assert!(!output.status.success());
    assert!(!tmp.path().join("in.jpg").exists());
}

#[test]
fn cli_look_values_are_validated() {
    let output = run(&["--look".as_ref(), "vivid".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
    let output = run(&["--help".as_ref()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--look"), "got: {stdout}");
}

#[test]
fn cli_look_does_not_change_raster_input() {
    // A PNG is already rendered; the look applies to RAW files only.
    let camera = convert_grey_with(&["--look", "camera"]);
    let flat = convert_grey_with(&["--look", "flat"]);
    for c in 0..3 {
        assert!((camera[c] - flat[c]).abs() < 0.5, "{camera:?} vs {flat:?}");
    }
}

#[test]
fn cli_output_carries_an_srgb_icc_profile_in_every_format() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let out = tmp.path().join("out");
    for format in ["jpeg", "png", "tiff"] {
        let output = run(&[input.as_ref(), "-o".as_ref(), out.as_ref(), "-f".as_ref(), format.as_ref()]);
        assert!(output.status.success(), "{format}");
    }
    for name in ["in.jpg", "in.png", "in.tiff"] {
        let bytes = fs::read(out.join(name)).unwrap();
        // Every ICC profile carries the signature "acsp" in its header. PNG
        // stores the profile compressed, in an iCCP chunk.
        let marker: &[u8] = if name.ends_with(".png") { b"iCCP" } else { b"acsp" };
        assert!(bytes.windows(4).any(|w| w == marker), "{name}: no ICC profile");
    }
}

#[test]
fn cli_min_rating_uses_sidecar_files_and_lists_exclusions_in_dry_run() {
    let tmp = TempDir::new().unwrap();
    for (name, rating) in [("keep", Some(4)), ("drop", Some(1)), ("none", None)] {
        write_png(&tmp.path().join(format!("{name}.png")), [1, 2, 3]);
        if let Some(r) = rating {
            let xmp = format!("<x:xmpmeta><rdf:Description xmp:Rating=\"{r}\"/></x:xmpmeta>");
            fs::write(tmp.path().join(format!("{name}.xmp")), xmp).unwrap();
        }
    }
    let inputs: Vec<std::path::PathBuf> = ["keep", "drop", "none"]
        .iter()
        .map(|n| tmp.path().join(format!("{n}.png")))
        .collect();
    let out = tmp.path().join("out");
    let mut args: Vec<&std::ffi::OsStr> = inputs.iter().map(|p| p.as_os_str()).collect();
    args.extend(["--min-rating", "3", "-o"].iter().map(|a| a.as_ref() as &std::ffi::OsStr));
    args.push(out.as_os_str());

    let mut dry = args.clone();
    dry.push("-n".as_ref());
    let stdout = String::from_utf8_lossy(&run(&dry).stdout).to_string();
    assert!(stdout.contains("exclude") && stdout.contains("drop.png (rating 1)"), "{stdout}");
    assert!(stdout.contains("none.png (no rating)"), "{stdout}");

    let output = run(&args);
    assert!(output.status.success());
    assert!(out.join("keep.jpg").is_file());
    assert!(!out.join("drop.jpg").exists());
    assert!(!out.join("none.jpg").exists());
}

#[test]
fn cli_selection_that_matches_nothing_is_not_an_error() {
    let tmp = TempDir::new().unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let output = run(&[input.as_ref(), "--camera".as_ref(), "Z 9".as_ref()]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No RAW files match"), "{stdout}");
    assert!(!tmp.path().join("in.jpg").exists());
}

#[test]
fn cli_since_rejects_a_bad_date() {
    let output = run(&["--since".as_ref(), "14/11/2026".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn cli_report_lists_every_file_with_its_result() {
    let tmp = TempDir::new().unwrap();
    let good = tmp.path().join("good.png");
    write_png(&good, [1, 2, 3]);
    let bad = tmp.path().join("bad.arw");
    fs::write(&bad, b"not a raw file").unwrap();
    let csv = tmp.path().join("run.csv");
    let json = tmp.path().join("run.json");

    run(&[good.as_ref(), bad.as_ref(), "--report".as_ref(), csv.as_ref()]);
    let text = fs::read_to_string(&csv).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "input,output,status,message,seconds");
    assert!(lines.iter().any(|l| l.contains("bad.arw") && l.contains(",failed,")), "{text}");
    assert!(lines.iter().any(|l| l.contains("good.png") && l.contains(",converted,")), "{text}");

    // Second run: the good file is skipped because its output exists.
    run(&[good.as_ref(), "--report".as_ref(), json.as_ref()]);
    let text = fs::read_to_string(&json).unwrap();
    assert!(text.contains(r#""status": "skipped""#), "{text}");
}

#[test]
fn cli_original_copies_files_into_the_name_layout() {
    let tmp = TempDir::new().unwrap();
    let card = tmp.path().join("card");
    fs::create_dir(&card).unwrap();
    fs::write(card.join("DSC1.ARW"), b"raw bytes one").unwrap();
    fs::write(card.join("DSC2.arw"), b"raw bytes two").unwrap();
    let archive = tmp.path().join("archive");

    let output = run(&[
        card.as_ref(),
        "-f".as_ref(),
        "original".as_ref(),
        "-o".as_ref(),
        archive.as_ref(),
        "--name".as_ref(),
        "{date}/{stem}".as_ref(),
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fs::read(archive.join("unknown/DSC1.ARW")).unwrap(), b"raw bytes one");
    assert_eq!(fs::read(archive.join("unknown/DSC2.arw")).unwrap(), b"raw bytes two");
    // The card is unchanged.
    assert_eq!(fs::read(card.join("DSC1.ARW")).unwrap(), b"raw bytes one");

    // Running again skips the existing copies.
    let output = run(&[card.as_ref(), "-f".as_ref(), "original".as_ref(), "-o".as_ref(), archive.as_ref(), "--name".as_ref(), "{date}/{stem}".as_ref()]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("0 succeeded, 0 failed, 2 skipped"));
}

#[test]
fn cli_original_rejects_image_options() {
    let output = run(&["-f".as_ref(), "original".as_ref(), "-r".as_ref(), "100".as_ref(), "/tmp/fake.arw".as_ref()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--resize"), "{stderr}");
    assert!(!output.status.success());
}

#[test]
fn cli_original_without_output_dir_refuses_to_copy_onto_itself() {
    let tmp = TempDir::new().unwrap();
    let raw = tmp.path().join("DSC1.ARW");
    fs::write(&raw, b"raw").unwrap();
    let output = run(&[raw.as_ref(), "-f".as_ref(), "original".as_ref(), "--overwrite".as_ref()]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("same file"));
    assert!(!output.status.success());
    assert_eq!(fs::read(&raw).unwrap(), b"raw");
}

#[test]
fn cli_gallery_lists_the_output_images() {
    let tmp = TempDir::new().unwrap();
    let card = tmp.path().join("card");
    write_png(&card.join("one.png"), [1, 2, 3]);
    write_png(&card.join("sub/two.png"), [4, 5, 6]);
    let out = tmp.path().join("proofs");

    // A directory scan picks up RAW files only, so the PNGs are given as
    // files; the template puts them in subdirectories.
    let output = run(&[
        card.join("one.png").as_ref(),
        card.join("sub/two.png").as_ref(),
        "-o".as_ref(),
        out.as_ref(),
        "--name".as_ref(),
        "day 1/{stem}".as_ref(),
        "--gallery".as_ref(),
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let page = fs::read_to_string(out.join("gallery.html")).unwrap();
    assert!(page.contains(r#"src="day%201/one.jpg""#), "{page}");
    assert!(page.contains(r#"src="day%201/two.jpg""#), "{page}");
    assert!(page.contains("2 images"));
}

#[test]
fn cli_gallery_needs_an_output_directory_and_a_web_format() {
    let output = run(&["--gallery".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
    let output = run(&["--gallery".as_ref(), "-o".as_ref(), "/tmp/x".as_ref(), "-f".as_ref(), "tiff".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("JPEG or PNG"));
}

#[test]
fn cli_selection_rejects_contradictory_or_empty_options() {
    let output = run(&[
        "--since".as_ref(),
        "2026-01-02".as_ref(),
        "--until".as_ref(),
        "2026-01-01".as_ref(),
        "/tmp/fake.arw".as_ref(),
    ]);
    assert!(String::from_utf8_lossy(&output.stderr).contains("--since"));
    assert!(!output.status.success());

    let output = run(&["--camera".as_ref(), "".as_ref(), "/tmp/fake.arw".as_ref()]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn cli_defaults_file_applies_and_can_be_overridden_or_skipped() {
    let tmp = TempDir::new().unwrap();
    let defaults = tmp.path().join("defaults");
    fs::write(&defaults, "# test\n--artist Default Name\n--format png\n").unwrap();
    let input = tmp.path().join("in.png");
    write_png(&input, [1, 2, 3]);
    let out = tmp.path().join("out");
    let rawdog = |extra: &[&str]| {
        Command::new(rawdog_bin())
            .env("RAWDOG_DEFAULTS", &defaults)
            .arg(&input)
            .arg("-o")
            .arg(&out)
            .args(extra)
            .output()
            .unwrap()
    };

    // The default format applies: PNG output.
    assert!(rawdog(&[]).status.success());
    assert!(out.join("in.png").is_file());

    // The command line wins over the defaults file.
    assert!(rawdog(&["--format", "jpeg"]).status.success());
    let file = fs::File::open(out.join("in.jpg")).unwrap();
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(file))
        .unwrap();
    let artist = exif.get_field(exif::Tag::Artist, exif::In::PRIMARY).unwrap();
    assert_eq!(artist.display_value().to_string(), "\"Default Name\"");

    // --no-defaults ignores the file: JPEG, no artist.
    fs::remove_file(out.join("in.jpg")).unwrap();
    assert!(rawdog(&["--no-defaults"]).status.success());
    let file = fs::File::open(out.join("in.jpg")).unwrap();
    let exif = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file));
    assert!(exif.map_or(true, |e| e.get_field(exif::Tag::Artist, exif::In::PRIMARY).is_none()));
}

#[test]
fn cli_reports_a_bad_line_in_the_defaults_file() {
    let tmp = TempDir::new().unwrap();
    let defaults = tmp.path().join("defaults");
    fs::write(&defaults, "--quality 90\nphoto.ARW\n").unwrap();
    let output = Command::new(rawdog_bin())
        .env("RAWDOG_DEFAULTS", &defaults)
        .arg("/tmp/fake.arw")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("line 2"), "{stderr}");
    assert!(!output.status.success());
}
