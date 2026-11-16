mod converter;
mod exif;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::{generate, Shell};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

#[derive(Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    #[value(alias = "jpg")]
    Jpeg,
    #[value(alias = "tif")]
    Tiff,
    Png,
}

#[derive(Parser)]
#[command(name = "rawdog", version, about = "Raw files in, images out. No Lightroom, no fuss.")]
struct Cli {
    /// One or more ARW files or directories containing ARW files
    #[arg(required_unless_present = "completions")]
    input: Vec<PathBuf>,

    /// Generate shell completion script and exit
    #[arg(long, value_enum, value_name = "SHELL")]
    completions: Option<Shell>,

    /// Output directory (defaults to same directory as each input file)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Output format
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Jpeg)]
    format: OutputFormat,

    /// JPEG quality (1-100, ignored for tiff/png)
    #[arg(short, long, default_value_t = 92, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,

    /// Resize long edge to this many pixels, preserving aspect ratio
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..))]
    resize: Option<u32>,

    /// Scale during RAW decoding when --resize is set. Several times
    /// faster and slightly softer; intended for proofs and culling
    #[arg(long, requires = "resize")]
    fast: bool,

    /// Do not copy GPS location to the output
    #[arg(long)]
    strip_gps: bool,

    /// Do not copy any metadata to the output
    #[arg(long)]
    strip_metadata: bool,

    /// Set the EXIF Artist tag in every output file
    #[arg(long, value_name = "NAME")]
    artist: Option<String>,

    /// Set the EXIF Copyright tag in every output file
    #[arg(long, value_name = "TEXT")]
    copyright: Option<String>,

    /// Overwrite existing output files (default: skip if exists)
    #[arg(long)]
    overwrite: bool,

    /// Recurse into subdirectories when scanning for ARW files
    #[arg(short = 'R', long)]
    recursive: bool,

    /// Also convert when the input file is newer than the existing output
    #[arg(short, long, conflicts_with = "overwrite")]
    update: bool,

    /// Print what would be converted, then exit without writing anything
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Number of files to convert in parallel (default: one per CPU core).
    /// Each job holds a full-size image in memory, so lower this for
    /// high-resolution files on machines with little RAM
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..))]
    jobs: Option<u32>,

    /// Print errors only
    #[arg(long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle shell completion generation
    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        generate(shell, &mut cmd, name, &mut std::io::stdout());
        return Ok(());
    }

    // Collect all ARW files from inputs
    let files = collect_arw_files(&cli.input, cli.recursive)?;
    // Same condition under which collect_arw_files warns "does not exist".
    let missing_inputs = cli.input.iter().any(|p| !p.is_dir() && !p.is_file());

    if files.is_empty() {
        if missing_inputs {
            bail!("No ARW files found: one or more input paths do not exist.");
        }
        bail!("No ARW files found in the provided inputs.");
    }

    let jobs = plan_outputs(&files, cli.output.as_deref(), cli.format)?;

    if cli.dry_run {
        for (input_path, output_path) in &jobs {
            let action = if should_skip(input_path, output_path, cli.overwrite, cli.update) {
                "skip   "
            } else {
                "convert"
            };
            println!("{action} {} -> {}", input_path.display(), output_path.display());
        }
        if missing_inputs {
            std::process::exit(1);
        }
        return Ok(());
    }

    if let Some(jobs) = cli.jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs as usize)
            .build_global()
            .context("Failed to configure the thread pool")?;
    }

    if !cli.quiet {
        println!("Found {} ARW file(s)", files.len());
    }

    let opts = converter::ConvertOptions {
        format: cli.format,
        quality: cli.quality,
        resize: cli.resize,
        fast: cli.fast,
        metadata: exif::MetadataOptions {
            strip_all: cli.strip_metadata,
            strip_gps: cli.strip_gps,
            artist: cli.artist.clone(),
            copyright: cli.copyright.clone(),
        },
    };

    let succeeded = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);

    let pb = if cli.quiet {
        ProgressBar::hidden()
    } else {
        ProgressBar::new(jobs.len() as u64)
    };
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .expect("invalid progress bar template")
            .progress_chars("=> "),
    );

    // Decoder panics are reported per file below; the default hook would
    // print a backtrace message for each one.
    std::panic::set_hook(Box::new(|_| {}));

    jobs.par_iter().for_each(|(input_path, output_path)| {
        if output_path.exists() && canonical(output_path) == canonical(input_path) {
            pb.suspend(|| {
                eprintln!(
                    "Error converting {}: output path {} is the same file as the input",
                    input_path.display(),
                    output_path.display()
                );
            });
            failed.fetch_add(1, Ordering::Relaxed);
            pb.inc(1);
            return;
        }

        if should_skip(input_path, output_path, cli.overwrite, cli.update) {
            skipped.fetch_add(1, Ordering::Relaxed);
            pb.inc(1);
            return;
        }

        // The RAW decoder can panic on corrupt files. Report that as a
        // failure of this file instead of aborting the batch.
        let result = std::panic::catch_unwind(|| {
            converter::convert_arw(input_path, output_path, &opts)
        })
        .unwrap_or_else(|_| Err(anyhow!("decoder panicked; the file is probably corrupt")));

        match result {
            Ok(()) => {
                succeeded.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                pb.suspend(|| {
                    eprintln!("Error converting {}: {:#}", input_path.display(), e);
                });
                failed.fetch_add(1, Ordering::Relaxed);
            }
        }

        pb.inc(1);
    });

    pb.finish_and_clear();

    let s = succeeded.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);
    let sk = skipped.load(Ordering::Relaxed);

    if !cli.quiet {
        println!("{s} succeeded, {f} failed, {sk} skipped");
    }

    if f > 0 || missing_inputs {
        std::process::exit(1);
    }

    Ok(())
}

/// An existing output is skipped unless `overwrite` is set, or `update` is
/// set and the input was modified after the output.
fn should_skip(input: &Path, output: &Path, overwrite: bool, update: bool) -> bool {
    if overwrite || !output.exists() {
        return false;
    }
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let input_is_newer = match (modified(input), modified(output)) {
        (Some(i), Some(o)) => i > o,
        _ => false,
    };
    !(update && input_is_newer)
}

/// An input file and where its output goes relative to the `-o` directory.
struct InputFile {
    path: PathBuf,
    /// Parent of `path` relative to the scanned input directory. Empty for
    /// explicit file arguments and for files at the top of a scanned directory.
    rel_dir: PathBuf,
}

/// Collect all `.arw`/`.ARW` files from the given paths.
/// Paths can be individual files or directories.
/// When `recursive` is true, directories are scanned recursively.
/// Inputs that resolve to the same file are kept only once.
fn collect_arw_files(inputs: &[PathBuf], recursive: bool) -> Result<Vec<InputFile>> {
    let mut files = Vec::new();

    for path in inputs {
        if path.is_dir() {
            if recursive {
                let walker = WalkDir::new(path)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|entry| match entry {
                        Ok(e) => Some(e),
                        Err(e) => {
                            eprintln!("Warning: {}", e);
                            None
                        }
                    });
                for entry in walker {
                    let p = entry.into_path();
                    if p.is_file() && is_arw(&p) {
                        let rel_dir = p
                            .parent()
                            .and_then(|d| d.strip_prefix(path).ok())
                            .unwrap_or(Path::new(""))
                            .to_path_buf();
                        files.push(InputFile { path: p, rel_dir });
                    }
                }
            } else {
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    let p = entry.path();
                    if p.is_file() && is_arw(&p) {
                        files.push(InputFile {
                            path: p,
                            rel_dir: PathBuf::new(),
                        });
                    }
                }
            }
        } else if path.is_file() {
            files.push(InputFile {
                path: path.clone(),
                rel_dir: PathBuf::new(),
            });
        } else {
            eprintln!("Warning: {} does not exist, skipping", path.display());
        }
    }

    // Stable sort: when one file is reached through several inputs, the
    // first input argument decides its `rel_dir`.
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut seen = HashSet::new();
    files.retain(|f| seen.insert(canonical(&f.path)));
    Ok(files)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Pair each input with its output path. Fails without converting anything
/// if two inputs would write the same output path.
fn plan_outputs(
    files: &[InputFile],
    output_dir: Option<&Path>,
    format: OutputFormat,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut jobs = Vec::with_capacity(files.len());
    let mut claimed: HashMap<PathBuf, &Path> = HashMap::new();
    let mut conflicts = Vec::new();

    for f in files {
        let dir = output_dir.map(|d| d.join(&f.rel_dir));
        let output = make_output_path(&f.path, dir.as_deref(), format);
        // Compare outputs by where they land on disk, not by how the input was spelled.
        let key = make_output_path(&canonical(&f.path), dir.as_deref(), format);
        if let Some(first) = claimed.get(&key) {
            conflicts.push(format!(
                "  {} and {} both write {}",
                first.display(),
                f.path.display(),
                output.display()
            ));
        } else {
            claimed.insert(key, &f.path);
        }
        jobs.push((f.path.clone(), output));
    }

    if !conflicts.is_empty() {
        bail!(
            "Output path conflicts, nothing converted:\n{}",
            conflicts.join("\n")
        );
    }
    Ok(jobs)
}

fn is_arw(path: &Path) -> bool {
    path.extension()
        .map(|ext| ext.eq_ignore_ascii_case("arw"))
        .unwrap_or(false)
}

/// Build the output path for a given input ARW file.
fn make_output_path(input: &Path, output_dir: Option<&Path>, format: OutputFormat) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    let ext = match format {
        OutputFormat::Jpeg => "jpg",
        OutputFormat::Tiff => "tiff",
        OutputFormat::Png => "png",
    };
    let file_name = format!("{}.{}", stem.to_string_lossy(), ext);

    match output_dir {
        Some(dir) => dir.join(file_name),
        None => input.with_file_name(file_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // is_arw tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_arw_lowercase() {
        assert!(is_arw(Path::new("photo.arw")));
    }

    #[test]
    fn is_arw_uppercase() {
        assert!(is_arw(Path::new("PHOTO.ARW")));
    }

    #[test]
    fn is_arw_mixed_case() {
        assert!(is_arw(Path::new("photo.Arw")));
        assert!(is_arw(Path::new("photo.aRW")));
        assert!(is_arw(Path::new("photo.ArW")));
    }

    #[test]
    fn is_arw_rejects_non_arw_extensions() {
        assert!(!is_arw(Path::new("photo.jpg")));
        assert!(!is_arw(Path::new("photo.cr2")));
        assert!(!is_arw(Path::new("photo.nef")));
        assert!(!is_arw(Path::new("photo.tiff")));
        assert!(!is_arw(Path::new("photo.png")));
    }

    #[test]
    fn is_arw_no_extension() {
        assert!(!is_arw(Path::new("photo")));
        assert!(!is_arw(Path::new("/some/path/noext")));
    }

    #[test]
    fn is_arw_empty_path() {
        assert!(!is_arw(Path::new("")));
    }

    #[test]
    fn is_arw_hidden_file() {
        assert!(is_arw(Path::new(".hidden.arw")));
    }

    #[test]
    fn is_arw_dotfile_named_arw() {
        // ".arw" as a dotfile: on Unix, Path sees no stem and extension "arw"
        // actually Path::new(".arw").extension() is None (it is treated as a
        // hidden file with no extension), so this should be false.
        assert!(!is_arw(Path::new(".arw")));
    }

    #[test]
    fn is_arw_double_extension() {
        // "photo.old.arw" -> extension is "arw", should be true
        assert!(is_arw(Path::new("photo.old.arw")));
    }

    #[test]
    fn is_arw_similar_extension() {
        assert!(!is_arw(Path::new("photo.arw2")));
        assert!(!is_arw(Path::new("photo.arwx")));
        assert!(!is_arw(Path::new("photo.raw")));
    }

    // -----------------------------------------------------------------------
    // make_output_path tests
    // -----------------------------------------------------------------------

    #[test]
    fn make_output_path_jpeg_no_output_dir() {
        let input = Path::new("/photos/DSC00001.ARW");
        let result = make_output_path(input, None, OutputFormat::Jpeg);
        assert_eq!(result, PathBuf::from("/photos/DSC00001.jpg"));
    }

    #[test]
    fn make_output_path_tiff_no_output_dir() {
        let input = Path::new("/photos/DSC00001.ARW");
        let result = make_output_path(input, None, OutputFormat::Tiff);
        assert_eq!(result, PathBuf::from("/photos/DSC00001.tiff"));
    }

    #[test]
    fn make_output_path_png_no_output_dir() {
        let input = Path::new("/photos/DSC00001.ARW");
        let result = make_output_path(input, None, OutputFormat::Png);
        assert_eq!(result, PathBuf::from("/photos/DSC00001.png"));
    }

    #[test]
    fn make_output_path_with_output_dir() {
        let input = Path::new("/photos/DSC00001.ARW");
        let output_dir = Path::new("/output");
        let result = make_output_path(input, Some(output_dir), OutputFormat::Jpeg);
        assert_eq!(result, PathBuf::from("/output/DSC00001.jpg"));
    }

    #[test]
    fn make_output_path_preserves_stem_with_dots() {
        let input = Path::new("/photos/my.photo.name.arw");
        let result = make_output_path(input, None, OutputFormat::Jpeg);
        assert_eq!(result, PathBuf::from("/photos/my.photo.name.jpg"));
    }

    #[test]
    fn make_output_path_relative_input() {
        let input = Path::new("DSC00001.arw");
        let result = make_output_path(input, None, OutputFormat::Png);
        assert_eq!(result, PathBuf::from("DSC00001.png"));
    }

    #[test]
    fn make_output_path_output_dir_overrides_input_dir() {
        let input = Path::new("/deep/nested/dir/photo.arw");
        let output_dir = Path::new("/flat");
        let result = make_output_path(input, Some(output_dir), OutputFormat::Tiff);
        assert_eq!(result, PathBuf::from("/flat/photo.tiff"));
    }

    // -----------------------------------------------------------------------
    // OutputFormat extension mapping tests
    // -----------------------------------------------------------------------

    #[test]
    fn output_format_jpeg_extension() {
        let input = Path::new("x.arw");
        let result = make_output_path(input, None, OutputFormat::Jpeg);
        assert_eq!(result.extension().unwrap(), "jpg");
    }

    #[test]
    fn output_format_tiff_extension() {
        let input = Path::new("x.arw");
        let result = make_output_path(input, None, OutputFormat::Tiff);
        assert_eq!(result.extension().unwrap(), "tiff");
    }

    #[test]
    fn output_format_png_extension() {
        let input = Path::new("x.arw");
        let result = make_output_path(input, None, OutputFormat::Png);
        assert_eq!(result.extension().unwrap(), "png");
    }

    // -----------------------------------------------------------------------
    // collect_arw_files tests (non-recursive mode)
    // -----------------------------------------------------------------------

    #[test]
    fn collect_arw_files_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn collect_arw_files_finds_arw_in_directory() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("photo1.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("photo2.ARW"), b"fake").unwrap();
        fs::write(tmp.path().join("photo3.Arw"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn collect_arw_files_ignores_non_arw_in_directory() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("photo.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("photo.jpg"), b"fake").unwrap();
        fs::write(tmp.path().join("photo.cr2"), b"fake").unwrap();
        fs::write(tmp.path().join("readme.txt"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.to_string_lossy().contains("photo.arw"));
    }

    #[test]
    fn collect_arw_files_accepts_individual_files() {
        let tmp = TempDir::new().unwrap();
        let arw_path = tmp.path().join("single.arw");
        fs::write(&arw_path, b"fake").unwrap();

        let inputs = vec![arw_path.clone()];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, arw_path);
    }

    #[test]
    fn collect_arw_files_individual_file_not_filtered_by_ext() {
        let tmp = TempDir::new().unwrap();
        let jpg_path = tmp.path().join("photo.jpg");
        fs::write(&jpg_path, b"fake").unwrap();

        // Individual file paths are passed through without extension checking
        let inputs = vec![jpg_path.clone()];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, jpg_path);
    }

    #[test]
    fn collect_arw_files_nonexistent_path_is_skipped() {
        let inputs = vec![PathBuf::from("/nonexistent/path/photo.arw")];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn collect_arw_files_mixed_dirs_and_files() {
        let tmp = TempDir::new().unwrap();

        let subdir = tmp.path().join("album");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("a.arw"), b"fake").unwrap();
        fs::write(subdir.join("b.arw"), b"fake").unwrap();
        fs::write(subdir.join("c.jpg"), b"fake").unwrap();

        let single = tmp.path().join("standalone.arw");
        fs::write(&single, b"fake").unwrap();

        let inputs = vec![subdir.clone(), single.clone()];
        let files = collect_arw_files(&inputs, false).unwrap();
        // 2 from dir + 1 individual file
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn collect_arw_files_results_are_sorted() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("z_photo.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("a_photo.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("m_photo.arw"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 3);

        let names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a_photo.arw", "m_photo.arw", "z_photo.arw"]);
    }

    #[test]
    fn collect_arw_files_does_not_recurse_by_default() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("top.arw"), b"fake").unwrap();

        let nested = tmp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("deep.arw"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.to_string_lossy().contains("top.arw"));
    }

    #[test]
    fn collect_arw_files_recurse_finds_nested() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("top.arw"), b"fake").unwrap();

        let nested = tmp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("deep.arw"), b"fake").unwrap();

        let deep = nested.join("deeper");
        fs::create_dir(&deep).unwrap();
        fs::write(deep.join("very_deep.arw"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_arw_files(&inputs, true).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn collect_arw_files_recurse_ignores_non_arw() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("top.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("top.jpg"), b"fake").unwrap();

        let nested = tmp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("deep.arw"), b"fake").unwrap();
        fs::write(nested.join("deep.txt"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_arw_files(&inputs, true).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn collect_arw_files_removes_duplicate_inputs() {
        let tmp = TempDir::new().unwrap();
        let arw = tmp.path().join("photo.arw");
        fs::write(&arw, b"fake").unwrap();
        let dotted = tmp.path().join(".").join("photo.arw");

        let inputs = vec![tmp.path().to_path_buf(), arw.clone(), dotted];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
    }

    // -----------------------------------------------------------------------
    // plan_outputs tests
    // -----------------------------------------------------------------------

    #[test]
    fn plan_outputs_mirrors_scanned_subdirectories_under_output_dir() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("in");
        fs::create_dir_all(input.join("a/deep")).unwrap();
        fs::create_dir_all(input.join("b")).unwrap();
        fs::write(input.join("top.arw"), b"fake").unwrap();
        fs::write(input.join("a/deep/x.arw"), b"fake").unwrap();
        fs::write(input.join("b/x.arw"), b"fake").unwrap();

        let files = collect_arw_files(&[input], true).unwrap();
        let out = Path::new("/out");
        let jobs = plan_outputs(&files, Some(out), OutputFormat::Jpeg).unwrap();
        let outputs: Vec<&Path> = jobs.iter().map(|(_, o)| o.as_path()).collect();
        assert_eq!(
            outputs,
            vec![
                Path::new("/out/a/deep/x.jpg"),
                Path::new("/out/b/x.jpg"),
                Path::new("/out/top.jpg"),
            ]
        );
    }

    #[test]
    fn plan_outputs_explicit_files_go_directly_into_output_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("x.arw"), b"fake").unwrap();

        let files = collect_arw_files(&[nested.join("x.arw")], true).unwrap();
        let jobs = plan_outputs(&files, Some(Path::new("/out")), OutputFormat::Png).unwrap();
        assert_eq!(jobs[0].1, PathBuf::from("/out/x.png"));
    }

    #[test]
    fn plan_outputs_without_output_dir_writes_next_to_input() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("a/x.arw"), b"fake").unwrap();

        let files = collect_arw_files(&[tmp.path().to_path_buf()], true).unwrap();
        let jobs = plan_outputs(&files, None, OutputFormat::Jpeg).unwrap();
        assert_eq!(jobs[0].1, tmp.path().join("a/x.jpg"));
    }

    #[test]
    fn plan_outputs_rejects_two_inputs_with_same_output() {
        let tmp = TempDir::new().unwrap();
        for d in ["a", "b"] {
            fs::create_dir_all(tmp.path().join(d)).unwrap();
            fs::write(tmp.path().join(d).join("x.arw"), b"fake").unwrap();
        }
        let a = tmp.path().join("a/x.arw");
        let b = tmp.path().join("b/x.arw");

        let files = collect_arw_files(&[a.clone(), b.clone()], false).unwrap();
        let err = plan_outputs(&files, Some(Path::new("/out")), OutputFormat::Jpeg)
            .expect_err("expected a conflict error");
        let msg = err.to_string();
        assert!(msg.contains(&*a.to_string_lossy()), "{msg}");
        assert!(msg.contains(&*b.to_string_lossy()), "{msg}");
    }

    #[test]
    fn plan_outputs_rejects_same_stem_in_same_dir_without_output_dir() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("x.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("x.png"), b"fake").unwrap();

        let inputs = vec![tmp.path().join("x.arw"), tmp.path().join(".").join("x.png")];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert!(plan_outputs(&files, None, OutputFormat::Jpeg).is_err());
    }

    #[test]
    fn collect_arw_files_empty_inputs() {
        let inputs: Vec<PathBuf> = vec![];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert!(files.is_empty());
    }
}
