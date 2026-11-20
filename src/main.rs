mod converter;
mod exif;
mod gallery;
mod naming;
mod report;
mod timeshift;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

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
    /// Copy the RAW file itself, unchanged. With --name, this files a card
    /// into a dated folder layout
    Original,
}

#[derive(Parser)]
#[command(
    name = "rawdog",
    version,
    about = "Raw files in, images out. No Lightroom, no fuss.",
    // Options from the defaults file come first; a later value wins.
    args_override_self = true,
    after_help = "Default options can be set in ~/.config/rawdog/defaults (or the file named by \
                  RAWDOG_DEFAULTS): one option per line, for example `--copyright (c) {year} J. Doe`."
)]
struct Cli {
    /// One or more RAW files, or directories that contain RAW files.
    /// Sony ARW and most other camera RAW formats are accepted
    #[arg(required_unless_present_any = ["completions", "files_from"])]
    input: Vec<PathBuf>,

    /// Read more inputs from this file, one path per line; "-" reads
    /// standard input, as in `find ... | rawdog --files-from - -o out`.
    /// All inputs form one run, so --name {seq}, --report, and --gallery
    /// cover them all
    #[arg(long, value_name = "FILE")]
    files_from: Option<PathBuf>,

    /// Generate shell completion script and exit
    #[arg(long, value_enum, value_name = "SHELL")]
    completions: Option<Shell>,

    /// Output directory (defaults to same directory as each input file)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Name template for output files, relative to the output directory
    /// and without extension. Fields: {stem} {date} {year} {month} {day}
    /// {time} {model} {lens} {serial} {rating} {seq}. {seq} numbers the files of the run
    /// in capture order: 0001, 0002, ... Example: "{date}/{model}_{stem}"
    #[arg(long, value_name = "TEMPLATE")]
    name: Option<naming::NameTemplate>,

    /// The first {seq} number, to continue the numbering of an earlier run
    #[arg(long, value_name = "N", requires = "name", default_value_t = 1,
          value_parser = clap::value_parser!(u32).range(1..))]
    seq_start: u32,

    /// Convert only files rated at least this many stars (1 to 5). Ratings
    /// come from sidecar XMP files (Lightroom, Capture One, darktable) or
    /// from the camera
    #[arg(long, value_name = "STARS", value_parser = clap::value_parser!(i8).range(1..=5))]
    min_rating: Option<i8>,

    /// Convert only files with one of these colour labels, such as "red"
    /// or "red,green" (not case-sensitive). Labels come from sidecar XMP
    /// files (Lightroom, Capture One, darktable)
    #[arg(long, value_name = "LABELS", value_delimiter = ',', action = clap::ArgAction::Set,
          value_parser = clap::builder::NonEmptyStringValueParser::new())]
    label: Vec<String>,

    /// Convert only files taken on or after this date (YYYY-MM-DD, camera
    /// clock)
    #[arg(long, value_name = "DATE", value_parser = parse_date)]
    since: Option<(u16, u8, u8)>,

    /// Convert only files taken on or before this date (YYYY-MM-DD, camera
    /// clock)
    #[arg(long, value_name = "DATE", value_parser = parse_date)]
    until: Option<(u16, u8, u8)>,

    /// Convert only files from cameras whose model contains this text
    /// (not case-sensitive), for example "7M4" or "Z 6"
    #[arg(long, value_name = "MODEL", value_parser = clap::builder::NonEmptyStringValueParser::new())]
    camera: Option<String>,

    /// Correct a camera clock that was set wrong: "+1h", "-30m", "+3m12s",
    /// or per camera model "7M4=+3m12s,Z 6=-1h". Changes the capture time
    /// used by --name, --since, --until, and --gallery, and the dates in the
    /// metadata of converted images (-f original copies files unchanged)
    #[arg(long, value_name = "OFFSET", allow_hyphen_values = true)]
    time_shift: Option<timeshift::TimeShift>,

    /// Convert only the files named in this file, such as a list a client
    /// sent back from the gallery. Names are separated by lines, commas,
    /// or spaces, and may include an extension (DSC01234 or DSC01234.jpg)
    #[arg(long, value_name = "FILE")]
    picks: Option<PathBuf>,

    /// Write what happened to each file to this file: JSON if the name ends
    /// in .json, otherwise CSV. Columns: input, output, status (converted,
    /// skipped, failed, excluded), message, seconds
    #[arg(long, value_name = "FILE")]
    report: Option<PathBuf>,

    /// Write gallery.html into the output directory: a proof page of the
    /// output images with file name, capture time, camera, and rating.
    /// Needs -o and JPEG or PNG output; replaces an earlier gallery.html
    #[arg(long, requires = "output")]
    gallery: bool,

    /// Output format
    #[arg(short, long, value_enum, default_value_t = OutputFormat::Jpeg)]
    format: OutputFormat,

    /// JPEG quality (1-100, ignored for tiff/png)
    #[arg(short, long, default_value_t = 92, value_parser = clap::value_parser!(u8).range(1..=100))]
    quality: u8,

    /// Largest JPEG file, such as 2MB or 500KB (1 MB is 1,000,000 bytes).
    /// The quality is lowered until each file fits; --quality is the
    /// highest quality used
    #[arg(long, value_name = "SIZE", value_parser = byte_size)]
    max_size: Option<u64>,

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

    /// Set the EXIF Copyright tag in every output file. {year} becomes the
    /// capture year, as in "(c) {year} J. Doe"
    #[arg(long, value_name = "TEXT")]
    copyright: Option<String>,

    /// Tone curve for RAW files. `camera` gives brightness and contrast
    /// close to the camera's own JPEG; `flat` is darker and flatter, with
    /// more room for editing afterwards
    #[arg(long, value_enum, default_value_t = converter::Look::Camera)]
    look: converter::Look,

    /// Use the JPEG preview that the camera embedded in each RAW file,
    /// without developing the RAW data. Very fast, with the camera's own
    /// colours; the size depends on the camera (1616x1080 for Sony)
    #[arg(long, conflicts_with_all = ["fast", "exposure", "wb"])]
    preview: bool,

    /// With -f original, also copy the JPEG or HEIF that the camera wrote
    /// next to each RAW file with the same name (RAW+JPEG)
    #[arg(long)]
    with_jpeg: bool,

    /// Blend this logo (a PNG, which may be transparent) into the
    /// bottom-right corner of every image. Its long side is a fifth of the
    /// image's short edge
    #[arg(long, value_name = "PNG")]
    watermark: Option<PathBuf>,

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

/// Write `dir/gallery.html` for the outputs that exist after the run.
fn write_gallery(
    dir: &Path,
    records: &[report::Record],
    shift: Option<&timeshift::TimeShift>,
    quiet: bool,
) -> Result<()> {
    let mut shown: Vec<&report::Record> = records
        .iter()
        .filter(|r| matches!(r.status, report::Status::Converted | report::Status::Skipped))
        .filter(|r| r.output.as_deref().is_some_and(Path::exists))
        .collect();
    shown.sort_by(|a, b| a.output.cmp(&b.output));
    let entries: Vec<gallery::Entry> = shown
        .par_iter()
        .map(|r| {
            let info = naming::NameInfo::read(&r.input, true).shifted(shift);
            let image = r.output.clone().unwrap_or_default();
            gallery::Entry {
                thumbnail: gallery::thumbnail(dir, &image),
                image,
                caption: gallery::Caption {
                    name: r
                        .input
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                    taken: info.taken.map(|(y, mo, d, h, mi, _)| {
                        format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
                    }),
                    model: info.model,
                    rating: info.rating,
                },
            }
        })
        .collect();
    let title = dir
        .canonicalize()
        .ok()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Gallery".to_string());
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Failed to create output directory {}", dir.display()))?;
    let page = dir.join("gallery.html");
    std::fs::write(&page, gallery::render(&title, dir, &entries))
        .with_context(|| format!("Failed to write {}", page.display()))?;
    if !quiet {
        println!("Gallery: {}", page.display());
    }
    Ok(())
}

/// The defaults file: `$RAWDOG_DEFAULTS`, else `$XDG_CONFIG_HOME/rawdog/defaults`,
/// else `~/.config/rawdog/defaults`.
fn defaults_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("RAWDOG_DEFAULTS") {
        return Some(PathBuf::from(path));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config.join("rawdog").join("defaults"))
}

/// Arguments from a defaults file. Each line holds one option, and optionally
/// its value after the first space; the value needs no quotes. Empty lines
/// and lines that start with `#` are ignored.
fn parse_defaults(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with("--") {
            return Err(format!(
                "line {}: expected an option such as --copyright, found {line:?}",
                number + 1
            ));
        }
        match line.split_once(char::is_whitespace) {
            Some((option, value)) => {
                args.push(option.to_string());
                args.push(value.trim().to_string());
            }
            None if value_is_optional(line) => {
                return Err(format!(
                    "line {}: give {line} a value, for example `{line} 0.5`",
                    number + 1
                ))
            }
            None => args.push(line.to_string()),
        }
    }
    Ok(args)
}

/// True if `option` takes a value that may be left out, like `--sharpen`.
/// In a defaults file such an option would take the next argument, the
/// first input path, as its value, so it must be given a value there.
fn value_is_optional(option: &str) -> bool {
    Cli::command()
        .get_arguments()
        .find(|a| a.get_long() == option.strip_prefix("--"))
        .and_then(|a| a.get_num_args())
        .is_some_and(|n| n.min_values() == 0 && n.max_values() > 0)
}

fn write_report(path: &Path, records: &mut [report::Record]) -> Result<()> {
    report::write(path, records)
        .map_err(|e| anyhow!("Failed to write report {}: {e}", path.display()))
}

fn parse_date(s: &str) -> Result<(u16, u8, u8), String> {
    let err = || "must be a date as YYYY-MM-DD".to_string();
    let mut parts = s.split('-');
    let (Some(y), Some(m), Some(d), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(err());
    };
    let (y, m, d) = (
        y.parse::<u16>().map_err(|_| err())?,
        m.parse::<u8>().map_err(|_| err())?,
        d.parse::<u8>().map_err(|_| err())?,
    );
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(err()),
    };
    if y < 1900 || !(1..=days_in_month).contains(&d) {
        return Err(err());
    }
    Ok((y, m, d))
}

/// The selection options of a run.
struct Selection {
    min_rating: Option<i8>,
    since: Option<(u16, u8, u8)>,
    until: Option<(u16, u8, u8)>,
    camera: Option<String>,
    /// Wanted colour labels in lower case; empty for any.
    labels: Vec<String>,
    picks: Option<Picks>,
}

impl Selection {
    fn is_active(&self) -> bool {
        self.needs_exif() || self.picks.is_some()
    }

    fn needs_exif(&self) -> bool {
        self.min_rating.is_some()
            || self.since.is_some()
            || self.until.is_some()
            || self.camera.is_some()
            || !self.labels.is_empty()
    }

    /// Why a file is excluded, if it is. A file without the metadata that a
    /// filter needs is excluded.
    fn exclusion(&self, info: &naming::NameInfo) -> Option<String> {
        if self.picks.as_ref().is_some_and(|p| !p.contains(&info.stem)) {
            return Some("not in the picks list".to_string());
        }
        if let Some(min) = self.min_rating {
            match info.rating {
                Some(r) if r >= min => {}
                Some(r) => return Some(format!("rating {r}")),
                None => return Some("no rating".to_string()),
            }
        }
        let date = info.taken.map(|(y, m, d, ..)| (y, m, d));
        if self.since.is_some() || self.until.is_some() {
            let Some(date) = date else {
                return Some("no capture date".to_string());
            };
            let shown = format!("taken {:04}-{:02}-{:02}", date.0, date.1, date.2);
            if self.since.is_some_and(|since| date < since)
                || self.until.is_some_and(|until| date > until)
            {
                return Some(shown);
            }
        }
        if !self.labels.is_empty() && !info.labels.iter().any(|l| self.labels.contains(l)) {
            return Some(match info.labels.first() {
                Some(label) => format!("label {label}"),
                None => "no label".to_string(),
            });
        }
        if let Some(wanted) = &self.camera {
            let model = info.model.as_deref().unwrap_or_default();
            if !model.to_lowercase().contains(&wanted.to_lowercase()) {
                return Some(if model.is_empty() {
                    "no camera model".to_string()
                } else {
                    format!("camera {model}")
                });
            }
        }
        None
    }
}

/// The file names of a `--picks` list, compared without letter case.
struct Picks {
    /// Each name as written, in lower case, and in lower case without its
    /// extension. A name is kept whole too, because a stem can contain a dot.
    names: Vec<(String, String, String)>,
}

impl Picks {
    /// The parts of a list separated by lines, commas, or semicolons.
    fn pieces(text: &str) -> impl Iterator<Item = &str> {
        text.split(['\n', '\r', ',', ';']).map(str::trim).filter(|p| !p.is_empty())
    }

    /// Parse a list. A piece that names one of `stems` as a whole is one
    /// name, so a file name with spaces (as the gallery lists it) works;
    /// any other piece is split at spaces, as in "DSC1 DSC2 DSC3".
    fn parse(text: &str, stems: &[String]) -> Picks {
        let stems: HashSet<String> = stems.iter().map(|s| s.to_lowercase()).collect();
        let mut names = Vec::new();
        for piece in Self::pieces(text) {
            let whole = Self::name(piece);
            if stems.contains(&whole.1) || stems.contains(&whole.2) {
                names.push(whole);
            } else {
                names.extend(piece.split_whitespace().map(Self::name));
            }
        }
        Picks { names }
    }

    /// A listed name as written, in lower case, and in lower case without
    /// its extension.
    fn name(n: &str) -> (String, String, String) {
        // A name copied from a path or a URL keeps only its last part.
        let n = n.rsplit(['/', '\\']).next().unwrap_or(n);
        let lower = n.to_lowercase();
        // Only a short extension with a letter is removed, so that
        // "IMG.0001" stays whole.
        let bare = match lower.rsplit_once('.') {
            Some((stem, ext))
                if !stem.is_empty()
                    && ext.len() <= 4
                    && ext.chars().any(|c| c.is_ascii_alphabetic()) =>
            {
                stem.to_string()
            }
            _ => lower.clone(),
        };
        (n.to_string(), lower, bare)
    }

    fn contains(&self, stem: &str) -> bool {
        let stem = stem.to_lowercase();
        self.names.iter().any(|n| n.1 == stem || n.2 == stem)
    }

    /// The listed names that match none of `stems`, in list order.
    fn unmatched(&self, stems: &[String]) -> Vec<&str> {
        let stems: HashSet<String> = stems.iter().map(|s| s.to_lowercase()).collect();
        let mut seen = HashSet::new();
        self.names
            .iter()
            .filter(|n| !stems.contains(&n.1) && !stems.contains(&n.2))
            .filter(|n| seen.insert(&n.2))
            .map(|n| n.0.as_str())
            .collect()
    }
}

/// A file size: a number of bytes, or a number with KB or MB (decimal
/// units, so a limit given in either convention is met). K and M are the
/// same as KB and MB.
fn byte_size(s: &str) -> Result<u64, String> {
    let error = || "must be a size such as 2MB, 500KB, or 800000".to_string();
    let upper = s.trim().to_ascii_uppercase();
    let (number, unit) = match upper.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => upper.split_at(i),
        None => (upper.as_str(), ""),
    };
    let unit = match unit.trim() {
        "" | "B" => 1.0,
        "K" | "KB" => 1e3,
        "M" | "MB" => 1e6,
        _ => return Err(error()),
    };
    match number.trim().parse::<f64>() {
        Ok(n) if n.is_finite() && n * unit >= 1.0 => Ok((n * unit) as u64),
        _ => Err(error()),
    }
}

fn tint_value(s: &str) -> Result<f32, String> {
    match s.parse::<f32>() {
        Ok(v) if (0.5..=2.0).contains(&v) => Ok(v),
        _ => Err("must be a number from 0.5 to 2".to_string()),
    }
}

fn main() -> Result<()> {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let skip_defaults = args.iter().any(|a| a == "--no-defaults");
    args.retain(|a| a != "--no-defaults");
    if !skip_defaults {
        if let Some(path) = defaults_path().filter(|p| p.is_file()) {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read defaults file {}", path.display()))?;
            let defaults = parse_defaults(&text)
                .map_err(|e| anyhow!("In defaults file {}: {e}", path.display()))?;
            args.splice(1..1, defaults.into_iter().map(Into::into));
        }
    }
    let cli = Cli::parse_from(args);

    // Handle shell completion generation
    if let Some(shell) = cli.completions {
        let mut cmd = Cli::command();
        let name = cmd.get_name().to_string();
        generate(shell, &mut cmd, name, &mut std::io::stdout());
        return Ok(());
    }

    if let (Some(since), Some(until)) = (cli.since, cli.until) {
        if since > until {
            bail!("--since must not be later than --until.");
        }
    }

    if cli.gallery && matches!(cli.format, OutputFormat::Tiff | OutputFormat::Original) {
        bail!("--gallery needs JPEG or PNG output; browsers do not show TIFF or RAW files.");
    }

    if cli.max_size.is_some() && matches!(cli.format, OutputFormat::Tiff | OutputFormat::Png) {
        bail!("--max-size needs JPEG output; TIFF and PNG files are lossless.");
    }

    if cli.with_jpeg && !matches!(cli.format, OutputFormat::Original) {
        bail!("--with-jpeg copies the camera's JPEG files, so it needs -f original.");
    }

    if matches!(cli.format, OutputFormat::Original) {
        let image_options = [
            ("--max-size", cli.max_size.is_some()),
            ("--resize", cli.resize.is_some()),
            ("--fast", cli.fast),
            ("--preview", cli.preview),
            ("--exposure", cli.exposure.is_some()),
            ("--wb", cli.wb.is_some()),
            ("--sharpen", cli.sharpen.is_some()),
            ("--strip-gps", cli.strip_gps),
            ("--strip-metadata", cli.strip_metadata),
            ("--artist", cli.artist.is_some()),
            ("--copyright", cli.copyright.is_some()),
            ("--watermark", cli.watermark.is_some()),
        ];
        let used: Vec<&str> = image_options
            .iter()
            .filter(|(_, set)| *set)
            .map(|(name, _)| *name)
            .collect();
        if !used.is_empty() {
            bail!(
                "-f original copies files unchanged, so {} cannot be used with it.",
                used.join(", ")
            );
        }
    }

    if let Some(dir) = cli.output.as_deref().filter(|d| d.exists() && !d.is_dir()) {
        bail!("Output path {} exists and is not a directory.", dir.display());
    }

    let picks_text = match &cli.picks {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read picks list {}", path.display()))?;
            if Picks::pieces(&text).next().is_none() {
                bail!("The picks list {} names no files.", path.display());
            }
            Some(text)
        }
        None => None,
    };

    let watermark = match &cli.watermark {
        Some(path) => Some(std::sync::Arc::new(
            image::open(path)
                .with_context(|| format!("Failed to read watermark {}", path.display()))?
                .into_rgba8(),
        )),
        None => None,
    };

    // Collect all RAW files from inputs
    let mut inputs = cli.input.clone();
    if let Some(list) = &cli.files_from {
        let text = if list.as_os_str() == "-" {
            std::io::read_to_string(std::io::stdin()).context("Failed to read standard input")?
        } else {
            std::fs::read_to_string(list)
                .with_context(|| format!("Failed to read file list {}", list.display()))?
        };
        let listed = input_list(&text);
        if listed.is_empty() && inputs.is_empty() {
            let source = if list.as_os_str() == "-" {
                "Standard input".to_string()
            } else {
                format!("The file list {}", list.display())
            };
            bail!("{source} names no files.");
        }
        inputs.extend(listed);
    }
    let files = collect_raw_files(&inputs, cli.recursive)?;
    // Same condition under which collect_raw_files warns "does not exist".
    let missing_inputs = inputs.iter().any(|p| !p.is_dir() && !p.is_file());

    if files.is_empty() {
        if missing_inputs {
            bail!("No RAW files found: one or more input paths do not exist.");
        }
        bail!("No RAW files found in the provided inputs.");
    }

    let stems: Vec<String> = files
        .iter()
        .map(|f| f.path.file_stem().unwrap_or_default().to_string_lossy().into_owned())
        .collect();
    let selection = Selection {
        min_rating: cli.min_rating,
        since: cli.since,
        until: cli.until,
        camera: cli.camera.clone(),
        labels: cli.label.iter().map(|l| l.trim().to_lowercase()).collect(),
        picks: picks_text.as_deref().map(|text| Picks::parse(text, &stems)),
    };
    if let (Some(picks), Some(path)) = (&selection.picks, &cli.picks) {
        let unmatched = picks.unmatched(&stems);
        if !unmatched.is_empty() {
            eprintln!(
                "Warning: {} name(s) in {} match no RAW file: {}",
                unmatched.len(),
                path.display(),
                unmatched.join(", ")
            );
        }
    }
    let mut excluded_records = Vec::new();
    let files = if selection.is_active() {
        let needs_exif = selection.needs_exif();
        let excluded: Vec<Option<String>> = files
            .par_iter()
            .map(|f| {
                let info = naming::NameInfo::read(&f.path, needs_exif);
                selection.exclusion(&info.shifted(cli.time_shift.as_ref()))
            })
            .collect();
        let (kept, dropped): (Vec<_>, Vec<_>) =
            files.into_iter().zip(excluded).partition(|(_, reason)| reason.is_none());
        excluded_records = dropped
            .iter()
            .map(|(f, reason)| report::Record {
                input: f.path.clone(),
                output: None,
                status: report::Status::Excluded,
                message: reason.clone().unwrap_or_default(),
                seconds: 0.0,
            })
            .collect();
        if cli.dry_run {
            for (f, reason) in &dropped {
                println!("exclude {} ({})", f.path.display(), reason.as_deref().unwrap_or(""));
            }
        } else if !cli.quiet && !dropped.is_empty() {
            println!("{} file(s) excluded by the selection options", dropped.len());
        }
        if kept.is_empty() {
            if !cli.quiet {
                println!("No RAW files match the selection options.");
            }
            if let Some(path) = &cli.report {
                write_report(path, &mut excluded_records)?;
            }
            if missing_inputs {
                std::process::exit(1);
            }
            return Ok(());
        }
        kept.into_iter().map(|(f, _)| f).collect()
    } else {
        files
    };

    let template = cli.name.clone().map(|mut t| {
        t.seq_start = cli.seq_start;
        t
    });
    let jobs = plan_outputs(
        &files,
        cli.output.as_deref(),
        cli.format,
        template.as_ref(),
        cli.time_shift.as_ref(),
    )?;

    // {seq} numbers the files of each run anew, so an existing output may
    // hold another input's image; skipping it would lose a file.
    if cli.name.as_ref().is_some_and(|t| t.uses_seq()) && !cli.overwrite {
        let existing = jobs.iter().filter(|(_, output)| output.exists()).count();
        if existing > 0 {
            bail!(
                "{existing} output file(s) already exist. {{seq}} numbers the files of each run \
                 anew, so an existing file may hold another image. Use --overwrite to replace \
                 them, or write to an empty directory."
            );
        }
    }

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
            if matches!(cli.format, OutputFormat::Original) {
                // Copies need little memory; more of them only compete for
                // the same disk.
                cores.min(4)
            } else {
                default_workers(largest_input, cli.fast || cli.preview, total_memory(), cores)
            }
        }
    };

    if !cli.quiet {
        println!("Found {} RAW file(s)", files.len());
    }

    let opts = converter::ConvertOptions {
        format: cli.format,
        quality: cli.quality,
        max_bytes: cli.max_size,
        resize: cli.resize,
        fast: cli.fast,
        exposure: cli.exposure,
        white_balance: cli.wb.map(|kelvin| (kelvin, cli.tint)),
        sharpen: cli.sharpen,
        preview: cli.preview,
        look: cli.look,
        with_jpeg: cli.with_jpeg,
        watermark,
        metadata: exif::MetadataOptions {
            strip_all: cli.strip_metadata,
            strip_gps: cli.strip_gps,
            artist: cli.artist.clone(),
            copyright: cli.copyright.clone(),
            srgb: !cli.preview,
            time_shift: cli.time_shift.clone(),
        },
    };

    let failures: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let records: Mutex<Vec<report::Record>> = Mutex::new(excluded_records);
    let record = |input: &Path, output: &Path, status, message: String, seconds: f64| {
        if cli.report.is_some() || cli.gallery {
            records.lock().unwrap().push(report::Record {
                input: input.to_path_buf(),
                output: Some(output.to_path_buf()),
                status,
                message,
                seconds,
            });
        }
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
            .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} (ETA {eta}) {msg}")
            .expect("invalid progress bar template")
            .progress_chars("=> "),
    );

    // Decoder panics are reported per file below; the default hook would
    // print a backtrace message for each one.
    std::panic::set_hook(Box::new(|_| {}));

    // On Ctrl-C, remove the partial outputs before exiting. 130 is the
    // usual exit status after SIGINT.
    let _ = ctrlc::set_handler(|| {
        converter::remove_active_temp_files();
        std::process::exit(130);
    });

    let convert_one = |(input_path, output_path): &(PathBuf, PathBuf)| {
        if let Some(reason) = refusal(input_path, output_path, &dir_names) {
            pb.suspend(|| {
                eprintln!("Error converting {}: {reason}", input_path.display());
            });
            record(input_path, output_path, report::Status::Failed, reason, 0.0);
            failures.lock().unwrap().push(input_path.clone());
            failed.fetch_add(1, Ordering::Relaxed);
            pb.inc(1);
            return;
        }

        if should_skip(input_path, output_path, cli.overwrite, cli.update) {
            record(
                input_path,
                output_path,
                report::Status::Skipped,
                "output exists".to_string(),
                0.0,
            );
            skipped.fetch_add(1, Ordering::Relaxed);
            pb.inc(1);
            return;
        }

        // The RAW decoder can panic on corrupt files. Report that as a
        // failure of this file instead of aborting the batch.
        let started = std::time::Instant::now();
        let result = std::panic::catch_unwind(|| {
            converter::convert_raw(input_path, output_path, &opts)
        })
        .unwrap_or_else(|_| Err(anyhow!("decoder panicked; the file is probably corrupt")));

        match result {
            Ok(()) => {
                let seconds = started.elapsed().as_secs_f64();
                record(input_path, output_path, report::Status::Converted, String::new(), seconds);
                succeeded.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                pb.suspend(|| {
                    eprintln!("Error converting {}: {:#}", input_path.display(), e);
                });
                let seconds = started.elapsed().as_secs_f64();
                record(input_path, output_path, report::Status::Failed, format!("{e:#}"), seconds);
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

    let mut records = records.into_inner().unwrap();
    if let Some(path) = &cli.report {
        write_report(path, &mut records)?;
    }
    if let (true, Some(dir)) = (cli.gallery, cli.output.as_deref()) {
        write_gallery(dir, &records, cli.time_shift.as_ref(), cli.quiet)?;
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
/// The paths of a `--files-from` list: one per line. Spaces are kept, as
/// they can be part of a name; empty lines and Windows line ends are not.
fn input_list(text: &str) -> Vec<PathBuf> {
    text.lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The `{seq}` value of each file: its position in capture order, from 1,
/// with at least four digits. Files with the same capture time are ordered
/// by path, and files without one come last.
fn sequence_numbers(files: &[InputFile], infos: &[naming::NameInfo], start: u32) -> Vec<String> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|&i| (infos[i].taken.is_none(), infos[i].taken, &files[i].path));
    let last = u64::from(start) + files.len() as u64 - 1;
    let width = last.to_string().len().max(4);
    let mut seqs = vec![String::new(); files.len()];
    for (n, &i) in order.iter().enumerate() {
        seqs[i] = format!("{:0width$}", u64::from(start) + n as u64);
    }
    seqs
}

/// With a name template, the output goes to the template's path below the
/// output directory (or below the input's directory when there is none), and
/// the scanned subdirectory is not mirrored.
fn plan_outputs(
    files: &[InputFile],
    output_dir: Option<&Path>,
    format: OutputFormat,
    template: Option<&naming::NameTemplate>,
    shift: Option<&timeshift::TimeShift>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    // Reading EXIF for the template opens every file, so do it in parallel.
    let named: Option<Vec<PathBuf>> = template.map(|t| {
        let needs_exif = t.needs_exif();
        let infos: Vec<naming::NameInfo> = files
            .par_iter()
            .map(|f| naming::NameInfo::read(&f.path, needs_exif).shifted(shift))
            .collect();
        let seqs = if t.uses_seq() {
            sequence_numbers(files, &infos, t.seq_start)
        } else {
            vec![String::new(); files.len()]
        };
        infos.iter().zip(&seqs).map(|(info, seq)| t.expand(info, seq)).collect()
    });

    let mut jobs = Vec::with_capacity(files.len());
    let mut claimed: HashMap<PathBuf, &Path> = HashMap::new();
    let mut conflicts = Vec::new();

    for (i, f) in files.iter().enumerate() {
        let output = match &named {
            Some(named) => {
                let mut name = named[i].clone().into_os_string();
                name.push(".");
                name.push(output_extension(&f.path, format));
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

fn output_extension(input: &Path, format: OutputFormat) -> String {
    match format {
        OutputFormat::Jpeg => "jpg".to_string(),
        OutputFormat::Tiff => "tiff".to_string(),
        OutputFormat::Png => "png".to_string(),
        OutputFormat::Original => input
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    }
}

/// File extensions of the RAW formats that the decoder supports.
const RAW_EXTENSIONS: &[&str] = &[
    "arw", "srf", "sr2", "dng", "cr2", "cr3", "crw", "nef", "nrw", "raf", "orf", "rw2", "pef", "srw",
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
    let ext = output_extension(input, format);
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
        assert!(plan_outputs(&files, Some(&out), OutputFormat::Jpeg, None, None).is_err());
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

    #[test]
    fn byte_size_accepts_decimal_units() {
        assert_eq!(byte_size("800000"), Ok(800_000));
        assert_eq!(byte_size("2MB"), Ok(2_000_000));
        assert_eq!(byte_size("1.5m"), Ok(1_500_000));
        assert_eq!(byte_size("500 KB"), Ok(500_000));
        assert_eq!(byte_size("500k"), Ok(500_000));
        assert_eq!(byte_size("10b"), Ok(10));
    }

    #[test]
    fn byte_size_rejects_other_values() {
        for bad in ["", "MB", "2GB", "2 MiB", "-1MB", "0", "0.0001KB", "nan", "inf"] {
            assert!(byte_size(bad).is_err(), "{bad} was accepted");
        }
    }

    // -----------------------------------------------------------------------
    // defaults file tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_defaults_reads_one_option_per_line() {
        let text = "# my defaults\n\n--copyright (c) 2026 J. Doe\n  --strip-gps  \n--quality 90\n";
        assert_eq!(
            parse_defaults(text).unwrap(),
            vec!["--copyright", "(c) 2026 J. Doe", "--strip-gps", "--quality", "90"]
        );
        // An option with an optional value needs one in the file, or it
        // would take the first input path as its value.
        assert!(parse_defaults("--sharpen\n").unwrap_err().contains("give --sharpen a value"));
        assert_eq!(parse_defaults("--sharpen 1\n").unwrap(), vec!["--sharpen", "1"]);
        let err = parse_defaults("photo.ARW\n").unwrap_err();
        assert!(err.contains("line 1"), "{err}");
    }

    #[test]
    fn command_line_overrides_a_default() {
        let cli = Cli::try_parse_from(["rawdog", "--quality", "80", "--quality", "95", "x.arw"]).unwrap();
        assert_eq!(cli.quality, 95);
    }

    // -----------------------------------------------------------------------
    // Selection tests
    // -----------------------------------------------------------------------

    fn info(rating: Option<i8>, date: Option<(u16, u8, u8)>, model: Option<&str>) -> naming::NameInfo {
        naming::NameInfo {
            stem: "x".to_string(),
            taken: date.map(|(y, m, d)| (y, m, d, 12, 0, 0)),
            model: model.map(str::to_string),
            rating,
            ..Default::default()
        }
    }

    fn selection() -> Selection {
        Selection {
            min_rating: None,
            since: None,
            until: None,
            camera: None,
            labels: Vec::new(),
            picks: None,
        }
    }

    #[test]
    fn label_option_splits_on_commas_and_a_later_one_wins() {
        let cli = Cli::try_parse_from(["rawdog", "--label", "red,Green", "x.arw"]).unwrap();
        assert_eq!(cli.label, ["red", "Green"]);
        let cli = Cli::try_parse_from(["rawdog", "--label", "red", "--label", "blue", "x.arw"]).unwrap();
        assert_eq!(cli.label, ["blue"]);
    }

    #[test]
    fn time_shift_option_accepts_negative_offsets() {
        let cli = Cli::try_parse_from(["rawdog", "--time-shift", "-1h", "x.arw"]).unwrap();
        assert_eq!(cli.time_shift.unwrap().seconds_for(None), -3_600);
        assert!(Cli::try_parse_from(["rawdog", "--time-shift", "soon", "x.arw"]).is_err());
    }

    #[test]
    fn selection_by_label() {
        let sel = Selection {
            labels: vec!["red".to_string(), "green".to_string()],
            ..selection()
        };
        let labelled = |labels: &[&str]| naming::NameInfo {
            labels: labels.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        };
        assert!(sel.exclusion(&labelled(&["green"])).is_none());
        assert!(sel.exclusion(&labelled(&["blue", "red"])).is_none());
        assert_eq!(sel.exclusion(&labelled(&["blue"])).unwrap(), "label blue");
        assert_eq!(sel.exclusion(&labelled(&[])).unwrap(), "no label");
    }

    #[test]
    fn sequence_numbers_follow_capture_time_then_path() {
        let file = |p: &str| InputFile { path: PathBuf::from(p), rel_dir: PathBuf::new() };
        let taken = |t: Option<(u16, u8, u8, u8, u8, u8)>| naming::NameInfo {
            taken: t,
            ..Default::default()
        };
        let files = [file("b/2.arw"), file("a/9.arw"), file("c/1.arw"), file("a/1.arw")];
        let infos = [
            taken(Some((2026, 11, 14, 18, 2, 5))),
            taken(Some((2026, 11, 14, 18, 2, 5))),
            taken(None),
            taken(Some((2026, 11, 14, 9, 0, 0))),
        ];
        assert_eq!(sequence_numbers(&files, &infos, 1), ["0003", "0002", "0004", "0001"]);
        assert_eq!(sequence_numbers(&files, &infos, 413), ["0415", "0414", "0416", "0413"]);
        assert_eq!(sequence_numbers(&files, &infos, 9998), ["10000", "09999", "10001", "09998"]);
    }

    #[test]
    fn sequence_numbers_widen_for_large_runs() {
        let files: Vec<InputFile> = (0..10_000)
            .map(|i| InputFile { path: PathBuf::from(format!("{i:05}.arw")), rel_dir: PathBuf::new() })
            .collect();
        let infos: Vec<naming::NameInfo> = (0..10_000).map(|_| Default::default()).collect();
        let seqs = sequence_numbers(&files, &infos, 1);
        assert_eq!((seqs[0].as_str(), seqs[9_999].as_str()), ("00001", "10000"));
    }

    #[test]
    fn input_list_keeps_spaces_and_skips_empty_lines() {
        assert_eq!(
            input_list("a/IMG 1.CR3\r\n\n  \nb.ARW\n"),
            [PathBuf::from("a/IMG 1.CR3"), PathBuf::from("b.ARW")]
        );
    }

    #[test]
    fn picks_parse_names_in_any_list_form() {
        let picks = Picks::parse("DSC01234.jpg, dsc01300\r\nproofs/DSC01400.JPG;  IMG.0001\n\n", &[]);
        for stem in ["DSC01234", "DSC01300", "dsc01400", "IMG.0001"] {
            assert!(picks.contains(stem), "{stem}");
        }
        assert!(!picks.contains("DSC01235"));
        assert!(!picks.contains("IMG"));
    }

    #[test]
    fn picks_keep_a_file_name_with_spaces_whole() {
        let stems = ["IMG 1".to_string(), "DSC2".to_string(), "DSC3".to_string()];
        let picks = Picks::parse("IMG 1.CR3\nDSC2 DSC3\nIMG 9.CR3", &stems);
        assert!(picks.contains("IMG 1") && picks.contains("DSC2") && picks.contains("DSC3"));
        assert!(!picks.contains("IMG 9"));
        assert_eq!(picks.unmatched(&stems), vec!["IMG", "9.CR3"]);
    }

    #[test]
    fn picks_report_unmatched_names_once() {
        let picks = Picks::parse("DSC1.jpg DSC2 dsc2.JPG DSC3", &[]);
        let stems = ["dsc1".to_string(), "DSC9".to_string()];
        assert_eq!(picks.unmatched(&stems), vec!["DSC2", "DSC3"]);
    }

    #[test]
    fn selection_by_picks() {
        let sel = Selection {
            picks: Some(Picks::parse("x.jpg", &[])),
            ..selection()
        };
        assert!(sel.exclusion(&info(None, None, None)).is_none());
        let other = naming::NameInfo { stem: "y".to_string(), ..Default::default() };
        assert_eq!(sel.exclusion(&other).unwrap(), "not in the picks list");
    }

    #[test]
    fn selection_by_rating() {
        let sel = Selection {
            min_rating: Some(3),
            ..selection()
        };
        assert!(sel.exclusion(&info(Some(3), None, None)).is_none());
        assert!(sel.exclusion(&info(Some(5), None, None)).is_none());
        assert_eq!(sel.exclusion(&info(Some(2), None, None)).unwrap(), "rating 2");
        assert_eq!(sel.exclusion(&info(Some(-1), None, None)).unwrap(), "rating -1");
        assert_eq!(sel.exclusion(&info(None, None, None)).unwrap(), "no rating");
    }

    #[test]
    fn selection_by_date_is_inclusive() {
        let sel = Selection {
            since: Some((2026, 11, 14)),
            until: Some((2026, 11, 15)),
            ..selection()
        };
        assert!(sel.exclusion(&info(None, Some((2026, 11, 14)), None)).is_none());
        assert!(sel.exclusion(&info(None, Some((2026, 11, 15)), None)).is_none());
        assert!(sel.exclusion(&info(None, Some((2026, 11, 13)), None)).is_some());
        assert!(sel.exclusion(&info(None, Some((2026, 12, 1)), None)).is_some());
        assert_eq!(sel.exclusion(&info(None, None, None)).unwrap(), "no capture date");
    }

    #[test]
    fn selection_by_camera_ignores_case() {
        let sel = Selection {
            camera: Some("7m4".to_string()),
            ..selection()
        };
        assert!(sel.exclusion(&info(None, None, Some("ILCE-7M4"))).is_none());
        assert_eq!(
            sel.exclusion(&info(None, None, Some("ILCE-7M3"))).unwrap(),
            "camera ILCE-7M3"
        );
        assert!(!selection().is_active());
        assert!(sel.is_active());
    }

    #[test]
    fn parse_date_accepts_only_valid_dates() {
        assert_eq!(parse_date("2026-11-05"), Ok((2026, 11, 5)));
        assert_eq!(parse_date("2024-02-29"), Ok((2024, 2, 29)));
        for bad in [
            "2026-13-01",
            "2026-11",
            "11/05/2026",
            "2026-11-05-1",
            "1800-01-01",
            "2026-02-30",
            "2025-02-29",
            "2026-04-31",
            "2026-00-10",
        ] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
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
    fn make_output_path_original_keeps_the_input_extension() {
        let input = Path::new("/card/DSC00001.ARW");
        let result = make_output_path(input, Some(Path::new("/archive")), OutputFormat::Original);
        assert_eq!(result, PathBuf::from("/archive/DSC00001.ARW"));
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
        let jobs = plan_outputs(&files, Some(out), OutputFormat::Jpeg, None, None).unwrap();
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
            plan_outputs(&files, Some(Path::new("/out")), OutputFormat::Jpeg, Some(&template), None)
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
        let jobs = plan_outputs(&files, None, OutputFormat::Png, Some(&template), None).unwrap();
        assert_eq!(jobs[0].1, input.join("a/unknown/x-web.png"));
    }

    #[test]
    fn plan_outputs_with_template_detects_collisions() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.arw"), b"fake").unwrap();
        fs::write(tmp.path().join("b.arw"), b"fake").unwrap();
        let files = collect_raw_files(&[tmp.path().to_path_buf()], false).unwrap();
        let template: naming::NameTemplate = "{date}".parse().unwrap();
        assert!(plan_outputs(&files, None, OutputFormat::Jpeg, Some(&template), None).is_err());
    }

    #[test]
    fn plan_outputs_explicit_files_go_directly_into_output_dir() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("x.arw"), b"fake").unwrap();

        let files = collect_raw_files(&[nested.join("x.arw")], true).unwrap();
        let jobs = plan_outputs(&files, Some(Path::new("/out")), OutputFormat::Png, None, None).unwrap();
        assert_eq!(jobs[0].1, PathBuf::from("/out/x.png"));
    }

    #[test]
    fn plan_outputs_without_output_dir_writes_next_to_input() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("a/x.arw"), b"fake").unwrap();

        let files = collect_raw_files(&[tmp.path().to_path_buf()], true).unwrap();
        let jobs = plan_outputs(&files, None, OutputFormat::Jpeg, None, None).unwrap();
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
        let err = plan_outputs(&files, Some(Path::new("/out")), OutputFormat::Jpeg, None, None)
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
        assert!(plan_outputs(&files, None, OutputFormat::Jpeg, None, None).is_err());
    }

    #[test]
    fn collect_raw_files_empty_inputs() {
        let inputs: Vec<PathBuf> = vec![];
        let files = collect_raw_files(&inputs, false).unwrap();
        assert!(files.is_empty());
    }
}
