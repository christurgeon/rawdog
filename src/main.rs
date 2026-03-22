mod converter;
mod exif;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::{generate, Shell};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use walkdir::WalkDir;

#[derive(Clone, Copy, ValueEnum)]
pub enum OutputFormat {
    Jpeg,
    Tiff,
    Png,
}

#[derive(Parser)]
#[command(name = "rawdog", about = "Raw files in, images out. No Lightroom, no fuss.")]
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
    #[arg(short, long)]
    resize: Option<u32>,

    /// Overwrite existing output files (default: skip if exists)
    #[arg(long)]
    overwrite: bool,

    /// Recurse into subdirectories when scanning for ARW files
    #[arg(short = 'R', long)]
    recursive: bool,
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

    if files.is_empty() {
        bail!("No ARW files found in the provided inputs.");
    }

    println!("Found {} ARW file(s)", files.len());

    let succeeded = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);

    let pb = ProgressBar::new(files.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .expect("invalid progress bar template")
            .progress_chars("=> "),
    );

    files.par_iter().for_each(|input_path| {
        let output_path = make_output_path(input_path, cli.output.as_deref(), cli.format);

        // Skip if exists and overwrite is not set
        if !cli.overwrite && output_path.exists() {
            skipped.fetch_add(1, Ordering::Relaxed);
            pb.inc(1);
            return;
        }

        match converter::convert_arw(input_path, &output_path, cli.format, cli.quality, cli.resize)
        {
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

    println!("{s} succeeded, {f} failed, {sk} skipped");

    Ok(())
}

/// Collect all `.arw`/`.ARW` files from the given paths.
/// Paths can be individual files or directories.
/// When `recursive` is true, directories are scanned recursively.
fn collect_arw_files(inputs: &[PathBuf], recursive: bool) -> Result<Vec<PathBuf>> {
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
                        files.push(p);
                    }
                }
            } else {
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    let p = entry.path();
                    if is_arw(&p) {
                        files.push(p);
                    }
                }
            }
        } else if path.is_file() {
            files.push(path.clone());
        } else {
            eprintln!("Warning: {} does not exist, skipping", path.display());
        }
    }

    files.sort();
    Ok(files)
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
        assert!(files[0].to_string_lossy().contains("photo.arw"));
    }

    #[test]
    fn collect_arw_files_accepts_individual_files() {
        let tmp = TempDir::new().unwrap();
        let arw_path = tmp.path().join("single.arw");
        fs::write(&arw_path, b"fake").unwrap();

        let inputs = vec![arw_path.clone()];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], arw_path);
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
        assert_eq!(files[0], jpg_path);
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
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
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
        assert!(files[0].to_string_lossy().contains("top.arw"));
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
    fn collect_arw_files_empty_inputs() {
        let inputs: Vec<PathBuf> = vec![];
        let files = collect_arw_files(&inputs, false).unwrap();
        assert!(files.is_empty());
    }
}
