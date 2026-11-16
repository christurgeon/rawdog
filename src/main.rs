mod converter;
mod exif;
mod naming;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
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
    /// One or more RAW files, or directories that contain RAW files.
    /// Sony ARW and most other camera RAW formats are accepted
    #[arg(required_unless_present = "completions")]
    input: Vec<PathBuf>,

    /// Generate shell completion script and exit
    #[arg(long, value_enum, value_name = "SHELL")]
    completions: Option<Shell>,

    /// Output directory (defaults to same directory as each input file)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Name template for output files, relative to the output directory
    /// and without extension. Fields: {stem} {date} {year} {month} {day}
    /// {time} {model}. Example: "{date}/{model}_{stem}"
    #[arg(long, value_name = "TEMPLATE")]
    name: Option<naming::NameTemplate>,

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

    /// Exposure correction in stops, from -5 to 5. Highlights roll off
    /// instead of clipping
    #[arg(short, long, value_name = "EV", allow_hyphen_values = true,
          value_parser = float_in_range::<-5, 5>)]
    exposure: Option<f32>,

    /// White balance colour temperature in kelvin, from 1500 to 15000.
    /// Replaces the as-shot white balance of every file
    #[arg(long, value_name = "KELVIN", value_parser = float_in_range::<1500, 15000>)]
    wb: Option<f32>,

    /// White balance tint for --wb. Above 1 is more magenta, below 1 is
    /// more green
    #[arg(long, requires = "wb", default_value_t = 1.0, value_parser = tint_value)]
    tint: f32,

    /// Sharpen the output after the resize, from 0 to 3. Without a value
    /// the amount is 0.5, which suits images resized for screens
    #[arg(long, value_name = "AMOUNT", num_args = 0..=1, default_missing_value = "0.5",
          value_parser = float_in_range::<0, 3>)]
    sharpen: Option<f32>,

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

    /// Recurse into subdirectories when scanning for RAW files
    #[arg(short = 'R', long)]
    recursive: bool,

    /// Also convert when the input file is newer than the existing output
    #[arg(short, long, conflicts_with = "overwrite")]
    update: bool,

    /// Print what would be converted, then exit without writing anything
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Number of files to convert at the same time. Each file uses all
    /// CPU cores and holds a full-size image in memory. The default is
    /// chosen from the installed RAM and the size of the largest input
    #[arg(short, long, value_parser = clap::value_parser!(u32).range(1..))]
    jobs: Option<u32>,

    /// Print errors only
    #[arg(long)]
    quiet: bool,
}

fn float_in_range<const MIN: i32, const MAX: i32>(s: &str) -> Result<f32, String> {
    match s.parse::<f32>() {
        Ok(v) if v >= MIN as f32 && v <= MAX as f32 => Ok(v),
        _ => Err(format!("must be a number from {MIN} to {MAX}")),
    }
}

fn tint_value(s: &str) -> Result<f32, String> {
    match s.parse::<f32>() {
        Ok(v) if (0.5..=2.0).contains(&v) => Ok(v),
        _ => Err("must be a number from 0.5 to 2".to_string()),
    }
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

    if let Some(dir) = cli.output.as_deref().filter(|d| d.exists() && !d.is_dir()) {
        bail!("Output path {} exists and is not a directory.", dir.display());
    }

    // Collect all RAW files from inputs
    let files = collect_raw_files(&cli.input, cli.recursive)?;
    // Same condition under which collect_raw_files warns "does not exist".
    let missing_inputs = cli.input.iter().any(|p| !p.is_dir() && !p.is_file());

    if files.is_empty() {
        if missing_inputs {
            bail!("No RAW files found: one or more input paths do not exist.");
        }
        bail!("No RAW files found in the provided inputs.");
    }

    let jobs = plan_outputs(
        &files,
        cli.output.as_deref(),
        cli.format,
        cli.name.as_ref(),
    )?;

    let dir_names = DirNames::default();

    if cli.dry_run {
        let mut refused = false;
        for (input_path, output_path) in &jobs {
            let reason = refusal(input_path, output_path, &dir_names);
            let action = if reason.is_some() {
                "refuse "
            } else if should_skip(input_path, output_path, cli.overwrite, cli.update) {
                "skip   "
            } else {
                "convert"
            };
            let note = reason.map(|r| format!(" ({r})")).unwrap_or_default();
            println!(
                "{action} {} -> {}{note}",
                input_path.display(),
                output_path.display()
            );
            refused |= !note.is_empty();
        }
        if missing_inputs || refused {
            std::process::exit(1);
        }
        return Ok(());
    }

    let workers = match cli.jobs {
        Some(n) => n as usize,
        None => {
            let largest_input = jobs
                .iter()
                .filter_map(|(input, _)| std::fs::metadata(input).ok())
                .map(|m| m.len())
                .max()
                .unwrap_or(0);
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            default_workers(largest_input, cli.fast, total_memory(), cores)
        }
    };

    if !cli.quiet {
        println!("Found {} RAW file(s)", files.len());
    }

    let opts = converter::ConvertOptions {
        format: cli.format,
        quality: cli.quality,
        resize: cli.resize,
        fast: cli.fast,
        exposure: cli.exposure,
        white_balance: cli.wb.map(|kelvin| (kelvin, cli.tint)),
        sharpen: cli.sharpen,
        metadata: exif::MetadataOptions {
            strip_all: cli.strip_metadata,
            strip_gps: cli.strip_gps,
            artist: cli.artist.clone(),
            copyright: cli.copyright.clone(),
        },
    };

    let failures: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
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
            .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} (ETA {eta}) {msg}")
            .expect("invalid progress bar template")
            .progress_chars("=> "),
    );

    // Decoder panics are reported per file below; the default hook would
    // print a backtrace message for each one.
    std::panic::set_hook(Box::new(|_| {}));

    let convert_one = |(input_path, output_path): &(PathBuf, PathBuf)| {
        if let Some(reason) = refusal(input_path, output_path, &dir_names) {
            pb.suspend(|| {
                eprintln!("Error converting {}: {reason}", input_path.display());
            });
            failures.lock().unwrap().push(input_path.clone());
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
            converter::convert_raw(input_path, output_path, &opts)
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
                failures.lock().unwrap().push(input_path.clone());
                failed.fetch_add(1, Ordering::Relaxed);
            }
        }

        pb.inc(1);
    };

    // The worker threads only limit how many files are open at once. The
    // decoder runs its own parallel loops on the global thread pool, so each
    // file still uses every core.
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers.min(jobs.len()) {
            scope.spawn(|| {
                while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    convert_one(job);
                }
            });
        }
    });

    pb.finish_and_clear();

    let s = succeeded.load(Ordering::Relaxed);
    let f = failed.load(Ordering::Relaxed);
    let sk = skipped.load(Ordering::Relaxed);

    if !cli.quiet {
        println!("{s} succeeded, {f} failed, {sk} skipped");
    }

    // Errors scroll away in a long run, so repeat which files failed.
    let mut failures = failures.into_inner().unwrap();
    if failures.len() > 1 {
        failures.sort();
        eprintln!("Failed files:");
        for path in failures {
            eprintln!("  {}", path.display());
        }
    }

    if f > 0 || missing_inputs {
        std::process::exit(1);
    }

    Ok(())
}

/// Number of files to convert at the same time when `--jobs` is not given.
///
/// Converting more files than fit in memory makes the system swap, which is
/// many times slower than converting fewer files. Measured peak memory per
/// file is about 40 times the RAW file size, or 12 times with `--fast`.
/// The files share 30 percent of the installed memory.
fn default_workers(largest_input: u64, fast: bool, total_memory: Option<u64>, cores: usize) -> usize {
    let Some(total_memory) = total_memory else {
        return cores.min(2);
    };
    let per_file = largest_input.max(1) * if fast { 12 } else { 40 };
    let budget = total_memory / 10 * 3;
    ((budget / per_file) as usize).clamp(1, cores.max(1))
}

/// Installed physical memory in bytes, if the platform lets us read it.
fn total_memory() -> Option<u64> {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    } else if cfg!(target_os = "linux") {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kib * 1024)
    } else {
        None
    }
}

/// The exact file names in each directory, read once per directory.
#[derive(Default)]
struct DirNames(Mutex<HashMap<PathBuf, HashSet<std::ffi::OsString>>>);

impl DirNames {
    /// True if `path` exists under exactly this name. On a case-insensitive
    /// volume `path.exists()` is also true for a name in another letter case.
    fn has_exact(&self, path: &Path) -> bool {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let mut cache = self.0.lock().unwrap();
        let names = cache.entry(parent.to_path_buf()).or_insert_with(|| {
            std::fs::read_dir(parent)
                .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
                .unwrap_or_default()
        });
        names.contains(name)
    }
}

/// Why this job must not run, if there is a reason. These checks protect
/// files that rawdog did not write, so `--overwrite` does not disable them.
fn refusal(input: &Path, output: &Path, dir_names: &DirNames) -> Option<String> {
    if !output.exists() {
        return None;
    }
    if canonical(output) == canonical(input) {
        return Some(format!(
            "output path {} is the same file as the input",
            output.display()
        ));
    }
    // A camera that records RAW+JPEG writes DSC1.JPG next to DSC1.ARW. On a
    // case-insensitive volume that file answers to the name DSC1.jpg.
    if !dir_names.has_exact(output) {
        return Some(format!(
            "{} would replace an existing file whose name differs only in letter case \
             (a camera JPEG?); use -o or --name to write somewhere else",
            output.display()
        ));
    }
    None
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
fn collect_raw_files(inputs: &[PathBuf], recursive: bool) -> Result<Vec<InputFile>> {
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
                    if p.is_file() && is_raw(&p) && !is_hidden(&p) {
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
                    if p.is_file() && is_raw(&p) && !is_hidden(&p) {
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

    // When one file is reached through several inputs, the first input
    // argument decides its `rel_dir`.
    let mut seen = HashSet::new();
    files.retain(|f| seen.insert(canonical(&f.path)));
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Dot-files in a scanned directory are not photos. macOS writes a `._NAME`
/// companion for every file on FAT and exFAT memory cards.
fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

/// Where an output lands on disk, for conflict detection: symbolic links in
/// the directory are resolved, and letter case is ignored because common
/// volumes (APFS, NTFS, exFAT) are case-insensitive.
fn output_key(output: &Path) -> PathBuf {
    let mut existing = output.parent().unwrap_or(Path::new(""));
    let resolved = loop {
        if let Ok(resolved) = std::fs::canonicalize(if existing.as_os_str().is_empty() {
            Path::new(".")
        } else {
            existing
        }) {
            break resolved;
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            None => break existing.to_path_buf(),
        }
    };
    let rest = output.strip_prefix(existing).unwrap_or(output);
    resolved.join(rest.to_string_lossy().to_lowercase())
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Pair each input with its output path. Fails without converting anything
/// if two inputs would write the same output path.
///
/// With a name template, the output goes to the template's path below the
/// output directory (or below the input's directory when there is none), and
/// the scanned subdirectory is not mirrored.
fn plan_outputs(
    files: &[InputFile],
    output_dir: Option<&Path>,
    format: OutputFormat,
    template: Option<&naming::NameTemplate>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    // Reading EXIF for the template opens every file, so do it in parallel.
    let named: Option<Vec<PathBuf>> = template.map(|t| {
        let needs_exif = t.needs_exif();
        files
            .par_iter()
            .map(|f| t.expand(&naming::NameInfo::read(&f.path, needs_exif)))
            .collect()
    });

    let mut jobs = Vec::with_capacity(files.len());
    let mut claimed: HashMap<PathBuf, &Path> = HashMap::new();
    let mut conflicts = Vec::new();

    for (i, f) in files.iter().enumerate() {
        let output = match &named {
            Some(named) => {
                let mut name = named[i].clone().into_os_string();
                name.push(".");
                name.push(extension(format));
                match output_dir {
                    Some(dir) => dir.join(&name),
                    None => f.path.parent().unwrap_or(Path::new("")).join(&name),
                }
            }
            None => {
                let dir = output_dir.map(|d| d.join(&f.rel_dir));
                make_output_path(&f.path, dir.as_deref(), format)
            }
        };
        let key = output_key(&output);
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

fn extension(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Jpeg => "jpg",
        OutputFormat::Tiff => "tiff",
        OutputFormat::Png => "png",
    }
}

/// File extensions of the RAW formats that the decoder supports.
const RAW_EXTENSIONS: &[&str] = &[
    "arw", "srf", "sr2", "dng", "cr2", "crw", "nef", "nrw", "raf", "orf", "rw2", "pef", "srw",
    "erf", "kdc", "dcr", "dcs", "mef", "mos", "iiq", "3fr", "mrw", "x3f", "ari",
];

fn is_raw(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| RAW_EXTENSIONS.iter().any(|raw| ext.eq_ignore_ascii_case(raw)))
        .unwrap_or(false)
}

/// Build the output path for a given input ARW file.
fn make_output_path(input: &Path, output_dir: Option<&Path>, format: OutputFormat) -> PathBuf {
    let stem = input.file_stem().unwrap_or_default();
    let ext = extension(format);
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
    // is_raw tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_raw_lowercase() {
        assert!(is_raw(Path::new("photo.arw")));
    }

    #[test]
    fn is_raw_uppercase() {
        assert!(is_raw(Path::new("PHOTO.ARW")));
    }

    #[test]
    fn is_raw_mixed_case() {
        assert!(is_raw(Path::new("photo.Arw")));
        assert!(is_raw(Path::new("photo.aRW")));
        assert!(is_raw(Path::new("photo.ArW")));
    }

    #[test]
    fn is_raw_rejects_non_arw_extensions() {
        assert!(!is_raw(Path::new("photo.jpg")));
        assert!(!is_raw(Path::new("photo.xmp")));
        assert!(!is_raw(Path::new("photo.heic")));
        assert!(!is_raw(Path::new("photo.tiff")));
        assert!(!is_raw(Path::new("photo.png")));
    }

    #[test]
    fn is_raw_no_extension() {
        assert!(!is_raw(Path::new("photo")));
        assert!(!is_raw(Path::new("/some/path/noext")));
    }

    #[test]
    fn is_raw_empty_path() {
        assert!(!is_raw(Path::new("")));
    }

    #[test]
    fn is_raw_hidden_file() {
        assert!(is_raw(Path::new(".hidden.arw")));
    }

    #[test]
    fn is_raw_dotfile_named_arw() {
        // ".arw" as a dotfile: on Unix, Path sees no stem and extension "arw"
        // actually Path::new(".arw").extension() is None (it is treated as a
        // hidden file with no extension), so this should be false.
        assert!(!is_raw(Path::new(".arw")));
    }

    #[test]
    fn is_raw_double_extension() {
        // "photo.old.arw" -> extension is "arw", should be true
        assert!(is_raw(Path::new("photo.old.arw")));
    }

    #[test]
    fn is_raw_similar_extension() {
        assert!(!is_raw(Path::new("photo.arw2")));
        assert!(!is_raw(Path::new("photo.arwx")));
        assert!(!is_raw(Path::new("photo.raw")));
    }

    #[test]
    fn is_raw_accepts_other_camera_formats() {
        for name in ["a.CR2", "a.nef", "a.dng", "a.RAF", "a.orf", "a.rw2", "a.pef", "a.3fr"] {
            assert!(is_raw(Path::new(name)), "{name}");
        }
    }

    // -----------------------------------------------------------------------
    // refusal, output_key, and scan filter tests
    // -----------------------------------------------------------------------

    #[test]
    fn refusal_for_same_file_and_none_for_new_or_own_outputs() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("a.png");
        fs::write(&input, b"x").unwrap();
        let names = DirNames::default();
        assert!(refusal(&input, &input, &names).unwrap().contains("same file"));
        assert!(refusal(&input, &tmp.path().join("new.jpg"), &names).is_none());

        let own = tmp.path().join("a.jpg");
        fs::write(&own, b"x").unwrap();
        assert!(refusal(&input, &own, &DirNames::default()).is_none());
    }

    #[test]
    fn refusal_protects_a_file_that_differs_only_in_letter_case() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("DSC1.ARW");
        fs::write(&input, b"x").unwrap();
        fs::write(tmp.path().join("DSC1.JPG"), b"camera jpeg").unwrap();
        let output = tmp.path().join("DSC1.jpg");

        let reason = refusal(&input, &output, &DirNames::default());
        if output.exists() {
            // Case-insensitive volume: the camera file is in the way.
            assert!(reason.unwrap().contains("letter case"));
        } else {
            assert!(reason.is_none());
        }
    }

    #[test]
    fn output_key_ignores_letter_case_and_resolves_symlinked_directories() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        assert_eq!(
            output_key(&real.join("IMG.jpg")),
            output_key(&real.join("img.jpg"))
        );
        // Directories that do not exist yet.
        assert_eq!(
            output_key(&real.join("New/Sub/IMG.jpg")),
            output_key(&real.join("new/sub/img.jpg"))
        );
        assert_ne!(output_key(&real.join("a.jpg")), output_key(&real.join("b.jpg")));

        #[cfg(unix)]
        {
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert_eq!(output_key(&link.join("a.jpg")), output_key(&real.join("a.jpg")));
        }
    }

    #[test]
    fn plan_outputs_detects_conflicts_that_differ_only_in_letter_case() {
        let tmp = TempDir::new().unwrap();
        for dir in ["a", "b"] {
            fs::create_dir(tmp.path().join(dir)).unwrap();
        }
        fs::write(tmp.path().join("a/img.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("b/IMG.arw"), b"fake").unwrap();
        let files = collect_raw_files(
            &[tmp.path().join("a/img.arw"), tmp.path().join("b/IMG.arw")],
            false,
        )
        .unwrap();
        let out = tmp.path().join("out");
        assert!(plan_outputs(&files, Some(&out), OutputFormat::Jpeg, None).is_err());
    }

    #[test]
    fn collect_raw_files_skips_dot_files_in_scans_but_not_explicit_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("DSC1.ARW"), b"fake").unwrap();
        fs::write(tmp.path().join("._DSC1.ARW"), b"fake").unwrap();
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("._DSC2.ARW"), b"fake").unwrap();

        for recursive in [false, true] {
            let files = collect_raw_files(&[tmp.path().to_path_buf()], recursive).unwrap();
            assert_eq!(files.len(), 1, "recursive {recursive}");
        }
        let explicit = collect_raw_files(&[tmp.path().join("._DSC1.ARW")], false).unwrap();
        assert_eq!(explicit.len(), 1);
    }

    #[test]
    fn collect_raw_files_first_argument_decides_the_output_subdirectory() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("a");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("x.arw"), b"fake").unwrap();

        let scanned_first =
            collect_raw_files(&[tmp.path().to_path_buf(), sub.join("x.arw")], true).unwrap();
        assert_eq!(scanned_first[0].rel_dir, PathBuf::from("a"));

        let explicit_first =
            collect_raw_files(&[sub.join("x.arw"), tmp.path().to_path_buf()], true).unwrap();
        assert_eq!(explicit_first[0].rel_dir, PathBuf::new());
    }

    // -----------------------------------------------------------------------
    // default_workers tests
    // -----------------------------------------------------------------------

    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * MB;

    #[test]
    fn default_workers_scales_with_memory_and_file_size() {
        // 25 MB files need about 1 GB each: 8 GB of RAM gives 2 at a time.
        assert_eq!(default_workers(25 * MB, false, Some(8 * GB), 8), 2);
        assert_eq!(default_workers(25 * MB, false, Some(16 * GB), 8), 4);
        // Larger files need more memory each.
        assert_eq!(default_workers(85 * MB, false, Some(16 * GB), 8), 1);
        // --fast needs less memory per file.
        assert_eq!(default_workers(25 * MB, true, Some(8 * GB), 8), 8);
    }

    #[test]
    fn default_workers_stays_between_one_and_the_core_count() {
        assert_eq!(default_workers(500 * MB, false, Some(2 * GB), 8), 1);
        assert_eq!(default_workers(MB, false, Some(64 * GB), 8), 8);
        assert_eq!(default_workers(0, false, Some(8 * GB), 4), 4);
        assert_eq!(default_workers(25 * MB, false, None, 8), 2);
        assert_eq!(default_workers(25 * MB, false, None, 1), 1);
    }

    #[test]
    fn total_memory_is_readable_on_supported_systems() {
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(total_memory().unwrap() > 64 * MB);
        }
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
    // collect_raw_files tests (non-recursive mode)
    // -----------------------------------------------------------------------

    #[test]
    fn collect_raw_files_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn collect_raw_files_finds_arw_in_directory() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("photo1.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("photo2.ARW"), b"fake").unwrap();
        fs::write(tmp.path().join("photo3.Arw"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn collect_raw_files_ignores_non_arw_in_directory() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("photo.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("photo.jpg"), b"fake").unwrap();
        fs::write(tmp.path().join("photo.xmp"), b"fake").unwrap();
        fs::write(tmp.path().join("readme.txt"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.to_string_lossy().contains("photo.arw"));
    }

    #[test]
    fn collect_raw_files_accepts_individual_files() {
        let tmp = TempDir::new().unwrap();
        let arw_path = tmp.path().join("single.arw");
        fs::write(&arw_path, b"fake").unwrap();

        let inputs = vec![arw_path.clone()];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, arw_path);
    }

    #[test]
    fn collect_raw_files_individual_file_not_filtered_by_ext() {
        let tmp = TempDir::new().unwrap();
        let jpg_path = tmp.path().join("photo.jpg");
        fs::write(&jpg_path, b"fake").unwrap();

        // Individual file paths are passed through without extension checking
        let inputs = vec![jpg_path.clone()];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, jpg_path);
    }

    #[test]
    fn collect_raw_files_nonexistent_path_is_skipped() {
        let inputs = vec![PathBuf::from("/nonexistent/path/photo.arw")];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn collect_raw_files_mixed_dirs_and_files() {
        let tmp = TempDir::new().unwrap();

        let subdir = tmp.path().join("album");
        fs::create_dir(&subdir).unwrap();
        fs::write(subdir.join("a.arw"), b"fake").unwrap();
        fs::write(subdir.join("b.arw"), b"fake").unwrap();
        fs::write(subdir.join("c.jpg"), b"fake").unwrap();

        let single = tmp.path().join("standalone.arw");
        fs::write(&single, b"fake").unwrap();

        let inputs = vec![subdir.clone(), single.clone()];
        let files = collect_raw_files(&inputs, false).unwrap();
        // 2 from dir + 1 individual file
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn collect_raw_files_results_are_sorted() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("z_photo.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("a_photo.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("m_photo.arw"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 3);

        let names: Vec<String> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a_photo.arw", "m_photo.arw", "z_photo.arw"]);
    }

    #[test]
    fn collect_raw_files_does_not_recurse_by_default() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("top.arw"), b"fake").unwrap();

        let nested = tmp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("deep.arw"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].path.to_string_lossy().contains("top.arw"));
    }

    #[test]
    fn collect_raw_files_recurse_finds_nested() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("top.arw"), b"fake").unwrap();

        let nested = tmp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("deep.arw"), b"fake").unwrap();

        let deep = nested.join("deeper");
        fs::create_dir(&deep).unwrap();
        fs::write(deep.join("very_deep.arw"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_raw_files(&inputs, true).unwrap();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn collect_raw_files_recurse_ignores_non_arw() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("top.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("top.jpg"), b"fake").unwrap();

        let nested = tmp.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("deep.arw"), b"fake").unwrap();
        fs::write(nested.join("deep.txt"), b"fake").unwrap();

        let inputs = vec![tmp.path().to_path_buf()];
        let files = collect_raw_files(&inputs, true).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn collect_raw_files_removes_duplicate_inputs() {
        let tmp = TempDir::new().unwrap();
        let arw = tmp.path().join("photo.arw");
        fs::write(&arw, b"fake").unwrap();
        let dotted = tmp.path().join(".").join("photo.arw");

        let inputs = vec![tmp.path().to_path_buf(), arw.clone(), dotted];
        let files = collect_raw_files(&inputs, false).unwrap();
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

        let files = collect_raw_files(&[input], true).unwrap();
        let out = Path::new("/out");
        let jobs = plan_outputs(&files, Some(out), OutputFormat::Jpeg, None).unwrap();
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
    fn plan_outputs_with_template_names_files_below_the_output_dir() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("in");
        fs::create_dir_all(input.join("a")).unwrap();
        fs::write(input.join("a/x.arw"), b"fake").unwrap();
        fs::write(input.join("y.arw"), b"fake").unwrap();
        let files = collect_raw_files(std::slice::from_ref(&input), true).unwrap();
        let template: naming::NameTemplate = "{date}/{stem}-web".parse().unwrap();

        let jobs =
            plan_outputs(&files, Some(Path::new("/out")), OutputFormat::Jpeg, Some(&template))
                .unwrap();
        let outputs: Vec<&Path> = jobs.iter().map(|(_, o)| o.as_path()).collect();
        assert_eq!(
            outputs,
            vec![
                Path::new("/out/unknown/x-web.jpg"),
                Path::new("/out/unknown/y-web.jpg"),
            ]
        );

        // Without an output directory, the path is below each input's directory.
        let jobs = plan_outputs(&files, None, OutputFormat::Png, Some(&template)).unwrap();
        assert_eq!(jobs[0].1, input.join("a/unknown/x-web.png"));
    }

    #[test]
    fn plan_outputs_with_template_detects_collisions() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("b.arw"), b"fake").unwrap();
        let files = collect_raw_files(&[tmp.path().to_path_buf()], false).unwrap();
        let template: naming::NameTemplate = "{date}".parse().unwrap();
        assert!(plan_outputs(&files, None, OutputFormat::Jpeg, Some(&template)).is_err());
    }

    #[test]
    fn plan_outputs_explicit_files_go_directly_into_output_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("x.arw"), b"fake").unwrap();

        let files = collect_raw_files(&[nested.join("x.arw")], true).unwrap();
        let jobs = plan_outputs(&files, Some(Path::new("/out")), OutputFormat::Png, None).unwrap();
        assert_eq!(jobs[0].1, PathBuf::from("/out/x.png"));
    }

    #[test]
    fn plan_outputs_without_output_dir_writes_next_to_input() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("a/x.arw"), b"fake").unwrap();

        let files = collect_raw_files(&[tmp.path().to_path_buf()], true).unwrap();
        let jobs = plan_outputs(&files, None, OutputFormat::Jpeg, None).unwrap();
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

        let files = collect_raw_files(&[a.clone(), b.clone()], false).unwrap();
        let err = plan_outputs(&files, Some(Path::new("/out")), OutputFormat::Jpeg, None)
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
        let files = collect_raw_files(&inputs, false).unwrap();
        assert!(plan_outputs(&files, None, OutputFormat::Jpeg, None).is_err());
    }

    #[test]
    fn collect_raw_files_empty_inputs() {
        let inputs: Vec<PathBuf> = vec![];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert!(files.is_empty());
    }
}
