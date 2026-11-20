use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::ImageEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::tiff::TiffEncoder;
use image::{ImageBuffer, Rgb, RgbImage};
use imagepipe::{ImageSource, Pipeline, SplineFunc};

use crate::exif as exif_utils;
use crate::OutputFormat;

/// Smallest `resize` value for which `fast` scales in the decoder.
const FAST_MIN_EDGE: u32 = 16;

/// Settings that apply to every file of a run.
#[derive(Clone)]
pub struct ConvertOptions {
    pub format: OutputFormat,
    /// JPEG quality, 1-100. Ignored for TIFF and PNG.
    pub quality: u8,
    /// Largest JPEG file in bytes. The quality is lowered until the file
    /// fits; `quality` is the highest quality tried.
    pub max_bytes: Option<u64>,
    /// Maximum length of the long edge in pixels.
    pub resize: Option<u32>,
    /// Scale during RAW decoding instead of after it. Needs `resize`.
    pub fast: bool,
    /// Exposure change in stops (EV).
    pub exposure: Option<f32>,
    /// White balance as (colour temperature in kelvin, tint). Replaces the
    /// as-shot white balance.
    pub white_balance: Option<(f32, f32)>,
    /// Strength of output sharpening, applied after the resize.
    pub sharpen: Option<f32>,
    /// Use the JPEG preview that the camera embedded in the RAW file
    /// instead of developing the RAW data.
    pub preview: bool,
    /// Tone curve for RAW development.
    pub look: Look,
    /// With `-f original`, also copy the camera's JPEG or HEIF of each RAW
    /// file.
    pub with_jpeg: bool,
    /// A logo blended into the bottom-right corner of every image.
    pub watermark: Option<std::sync::Arc<image::RgbaImage>>,
    pub metadata: exif_utils::MetadataOptions,
}

/// Tone curve for RAW development.
#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum Look {
    /// Brightness and contrast close to the camera's own JPEG.
    Camera,
    /// imagepipe's base curve alone: darker and flatter, with more room for
    /// editing afterwards.
    Flat,
}

/// Convert a single ARW file to the specified output format.
pub fn convert_raw(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<()> {
    if matches!(opts.format, OutputFormat::Original) {
        return copy_original(input, output, opts.with_jpeg);
    }

    let ConvertOptions {
        format,
        resize,
        fast,
        exposure,
        white_balance,
        ..
    } = *opts;

    let mut source = if opts.preview {
        Source::Preview(load_preview(input)?)
    } else {
        let mut pipeline = open_pipeline(input)?;

        // With a size limit, imagepipe demosaics and scales in one pass when
        // the reduction is 2x or more. This is several times faster than a
        // full demosaic followed by a Lanczos resize, and slightly softer.
        // Two extra pixels are requested because `trim_fast_edge` removes
        // one pixel from every side. Very small sizes use the normal path.
        if let (true, Some(max_edge)) = (fast, resize) {
            if max_edge >= FAST_MIN_EDGE {
                pipeline.globals.settings.maxwidth = max_edge as usize + 2;
                pipeline.globals.settings.maxheight = max_edge as usize + 2;
            }
        }

        if let Some((kelvin, tint)) = white_balance {
            // imagepipe divides the green reference by its tint value, so a
            // higher value gives a greener image. `--tint` is defined the
            // other way: above 1 is more magenta.
            pipeline.ops.tolab.set_temp(kelvin, 1.0 / tint);
        }
        // Raster input (JPEG, PNG, TIFF) is already rendered.
        let raw_input = matches!(pipeline.globals.image, ImageSource::Raw(_));
        if raw_input && opts.look == Look::Camera {
            let curve = &mut pipeline.ops.basecurve;
            curve.points = camera_look_curve(&curve.get_spline());
        }
        match (exposure, &mut pipeline.globals.image) {
            // Darkening is exact as a gain on the RAW samples: colour and
            // lightness scale together. The curve changes lightness only,
            // which left dark areas too saturated at large negative values.
            (Some(ev), ImageSource::Raw(raw)) if ev < 0.0 => scale_raw_samples(raw, ev.exp2()),
            // Brightening uses the curve, which rolls off highlights where a
            // gain would clip them.
            (Some(ev), _) => {
                let curve = &mut pipeline.ops.basecurve;
                curve.points = exposure_curve(ev, &curve.get_spline());
            }
            (None, _) => {}
        }
        Source::Developed(Box::new(pipeline))
    };

    // Ensure output directory exists
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create output directory {}", parent.display()))?;
    }

    // Write to a temporary file and rename it, so an interrupted or failed
    // run never leaves a partial file that a later run would skip. The file
    // is created before decoding, so an unwritable directory fails quickly.
    let tmp = temp_path_for(output);
    let _guard = TempFileGuard::register(&tmp);
    let unwritable = || format!("Failed to write output file {}", output.display());
    let result = std::fs::File::create(&tmp)
        .with_context(unwritable)
        .and_then(|file| {
            let mut out = std::io::BufWriter::new(file);
            match format {
                OutputFormat::Jpeg => {
                    let mut img = source.rgb8(input, opts)?;
                    if let Some(mark) = &opts.watermark {
                        apply_watermark(&mut img, mark);
                    }
                    let bytes = encode_jpeg(&img, input, opts)?;
                    out.write_all(&bytes).with_context(unwritable)?;
                }
                OutputFormat::Tiff => {
                    let mut img = source.rgb16(input, opts)?;
                    if let Some(mark) = &opts.watermark {
                        apply_watermark(&mut img, mark);
                    }
                    write_tiff(&img, input, &mut out, opts).with_context(unwritable)?;
                }
                OutputFormat::Original => unreachable!("handled by copy_original"),
                OutputFormat::Png => {
                    let mut img = source.rgb16(input, opts)?;
                    if let Some(mark) = &opts.watermark {
                        apply_watermark(&mut img, mark);
                    }
                    write_png(&img, input, &mut out, opts).with_context(unwritable)?;
                }
            }
            out.flush().with_context(unwritable)?;
            drop(out);
            std::fs::rename(&tmp, output).with_context(unwritable)
        });

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Where the pixels of one conversion come from.
enum Source {
    /// The RAW data, developed by imagepipe.
    Developed(Box<Pipeline>),
    /// The camera's embedded JPEG preview, already upright.
    Preview(RgbImage),
}

impl Source {
    /// The final 8-bit image: resized and sharpened.
    fn rgb8(&mut self, input: &Path, opts: &ConvertOptions) -> Result<RgbImage> {
        match self {
            Source::Developed(pipeline) => render_8bit(pipeline, input, opts),
            Source::Preview(img) => {
                let mut img = maybe_resize_8bit(std::mem::take(img), opts.resize);
                if let Some(amount) = opts.sharpen {
                    sharpen(&mut img, amount);
                }
                Ok(img)
            }
        }
    }

    /// The final 16-bit image: resized and sharpened.
    fn rgb16(
        &mut self,
        input: &Path,
        opts: &ConvertOptions,
    ) -> Result<ImageBuffer<Rgb<u16>, Vec<u16>>> {
        match self {
            Source::Developed(pipeline) => render_16bit_sharpened(pipeline, input, opts),
            Source::Preview(img) => {
                let wide = image::DynamicImage::ImageRgb8(std::mem::take(img)).into_rgb16();
                let mut img = maybe_resize_16bit(wide, opts.resize);
                if let Some(amount) = opts.sharpen {
                    sharpen(&mut img, amount);
                }
                Ok(img)
            }
        }
    }
}

/// Load the JPEG preview that the camera stored in a RAW file, rotated
/// upright. Its size depends on the camera: about 1616x1080 for Sony, full
/// resolution for many Canon and Nikon bodies.
fn load_preview(input: &Path) -> Result<RgbImage> {
    use rawler::decoders::RawDecodeParams;
    use rawler::rawsource::RawSource;

    let load = || -> std::result::Result<Option<RgbImage>, String> {
        let source = RawSource::new(input).map_err(|e| e.to_string())?;
        let decoder = rawler::get_decoder(&source).map_err(|e| e.to_string())?;
        let params = RawDecodeParams::default();
        let Some(mut preview) = decoder
            .preview_image(&source, &params)
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        // The preview is stored as the sensor recorded it.
        let orientation = crate::naming::sidecar_orientation(input)
            .or_else(|| decoder.raw_metadata(&source, &params).ok()?.exif.orientation)
            .and_then(|n| u8::try_from(n).ok())
            .and_then(image::metadata::Orientation::from_exif);
        if let Some(orientation) = orientation {
            preview.apply_orientation(orientation);
        }
        Ok(Some(preview.into_rgb8()))
    };

    // rawler can panic on files that are not RAW files.
    let first_line = |e: String| e.lines().next().unwrap_or_default().to_string();
    match std::panic::catch_unwind(load) {
        Ok(Ok(Some(preview))) => Ok(preview),
        Ok(Ok(None)) => Err(anyhow!(
            "{} has no embedded preview; convert it without --preview",
            input.display()
        )),
        Ok(Err(e)) => Err(anyhow!(
            "Cannot read a preview from {}: {}",
            input.display(),
            first_line(e)
        )),
        Err(_) => Err(anyhow!("Cannot read a preview from {}", input.display())),
    }
}

/// Decode `input` and build the processing pipeline for it.
///
/// Decoders are tried in this order:
/// 1. rawler. It knows cameras up to now, Canon CR3, and the sensor crop of
///    many older cameras that rawloader outputs with masked border pixels.
/// 2. rawloader, which imagepipe is built on. Its camera list ends in 2018,
///    but it reads a few files that rawler does not.
/// 3. imagepipe's raster loader, for JPEG, PNG, and TIFF input.
///
/// The RAW decoders come first because most RAW files are TIFF containers,
/// and a raster loader could open one and return only its preview.
fn open_pipeline(input: &Path) -> Result<Pipeline> {
    let mut raw = match decode_with_rawler(input) {
        Ok(raw) => raw,
        Err(new_decoder) => match rawloader::decode_file(input) {
            Ok(raw) => raw,
            // imagepipe opens raster files itself, with its own version of
            // the `image` crate.
            Err(old_decoder) => {
                return Pipeline::new_from_file(input).map_err(|_| {
                    // Both decoders give long messages; one line is enough.
                    let detail = |e: &str| e.lines().next().unwrap_or_default().to_string();
                    anyhow!(
                        "Cannot decode {}: the file is corrupt or its format is not supported \
                         ({}; {})",
                        input.display(),
                        detail(&new_decoder),
                        detail(&old_decoder.to_string())
                    )
                })
            }
        },
    };
    if let Some(orientation) = crate::naming::sidecar_orientation(input) {
        raw.orientation = rawloader::Orientation::from_u16(orientation);
    }
    if let Some(problem) = implausible_samples(&raw) {
        return Err(anyhow!(
            "Cannot decode {}: the decoder returned invalid data ({problem}); \
             this format variant is not supported",
            input.display()
        ));
    }
    Pipeline::new_from_source(ImageSource::Raw(raw))
        .map_err(|e| anyhow!("Failed to create pipeline for {}: {}", input.display(), e))
}

/// A reason to reject the decoded samples, if many of them are far above the
/// white level. That happens when a decoder misreads a compression variant,
/// and the result is an image of noise.
fn implausible_samples(raw: &rawloader::RawImage) -> Option<String> {
    let rawloader::RawImageData::Integer(data) = &raw.data else {
        return None;
    };
    let white = *raw.whitelevels.iter().max()? as u32;
    if data.is_empty() || white == 0 {
        return None;
    }
    let limit = white.saturating_mul(2);
    let over = data.iter().filter(|&&v| v as u32 > limit).count();
    // A few hot pixels are normal; one sample in a hundred is not.
    (over * 100 > data.len()).then(|| {
        format!(
            "{:.0}% of the samples are above twice the white level {white}",
            over as f64 * 100.0 / data.len() as f64
        )
    })
}

/// Decode with rawler and convert the result to the type that imagepipe
/// takes. The two crates share an ancestor, so the fields map one to one.
fn decode_with_rawler(input: &Path) -> Result<rawloader::RawImage, String> {
    use rawler::rawimage::{RawImageData, RawPhotometricInterpretation};

    // rawler can panic on files that are not RAW files. It sets
    // `RawImage::orientation` only for DNG; for the other formats the
    // orientation comes from the metadata.
    let (img, exif_orientation) = std::panic::catch_unwind(|| {
        let source = rawler::rawsource::RawSource::new(input).map_err(|e| e.to_string())?;
        let decoder = rawler::get_decoder(&source).map_err(|e| e.to_string())?;
        let params = rawler::decoders::RawDecodeParams::default();
        let img = decoder
            .raw_image(&source, &params, false)
            .map_err(|e| e.to_string())?;
        let orientation = decoder
            .raw_metadata(&source, &params)
            .ok()
            .and_then(|m| m.exif.orientation);
        Ok::<_, String>((img, orientation))
    })
    .map_err(|_| "decoder panicked".to_string())??;

    let cfa = match &img.photometric {
        RawPhotometricInterpretation::Cfa(config) => rawloader::CFA::new(&config.cfa.name),
        _ => rawloader::CFA::new(""),
    };

    // rawler gives levels per position of a 2x2 pattern; imagepipe wants
    // them per colour.
    let per_colour = |levels: [f32; 4]| {
        let mut out = levels.map(|l| l.round() as u16);
        if cfa.width == 2 && cfa.height == 2 {
            for (i, level) in levels.iter().enumerate() {
                out[cfa.color_at(i / 2, i % 2)] = level.round() as u16;
            }
        }
        out
    };

    // Top, right, bottom, left.
    let crops = match img.crop_area.or(img.active_area) {
        Some(r) => [
            r.p.y,
            img.width.saturating_sub(r.p.x + r.d.w),
            img.height.saturating_sub(r.p.y + r.d.h),
            r.p.x,
        ],
        None => [0; 4],
    };

    // rawler leaves `xyz_to_cam` empty and gives matrices per illuminant.
    let mut xyz_to_cam = img.xyz_to_cam;
    if xyz_to_cam.iter().flatten().all(|v| *v == 0.0) {
        use rawler::imgop::xyz::Illuminant;
        // Daylight illuminants first, D65 best. The matrices are in a hash
        // map, so the fallback must not depend on its iteration order.
        let matrix = [Illuminant::D65, Illuminant::D75, Illuminant::D55, Illuminant::D50]
            .iter()
            .find_map(|i| img.color_matrix.get(i))
            .or_else(|| {
                img.color_matrix
                    .iter()
                    .min_by_key(|(illuminant, _)| **illuminant)
                    .map(|(_, m)| m)
            });
        let monochrome = img.cpp == 1 && !cfa.is_valid();
        match matrix {
            Some(matrix) => {
                for (i, v) in matrix.iter().take(12).enumerate() {
                    xyz_to_cam[i / 3][i % 3] = *v;
                }
            }
            // imagepipe does not use the matrix of a monochrome image.
            None if monochrome => {}
            None => return Err("no colour matrix for this camera".to_string()),
        }
    }

    use rawler::Orientation as New;
    use rawloader::Orientation as Old;
    let source_orientation = match (img.orientation, exif_orientation) {
        (New::Normal | New::Unknown, Some(tag)) => New::from_u16(tag),
        (orientation, _) => orientation,
    };
    let orientation = match source_orientation {
        New::Normal => Old::Normal,
        New::HorizontalFlip => Old::HorizontalFlip,
        New::Rotate180 => Old::Rotate180,
        New::VerticalFlip => Old::VerticalFlip,
        New::Transpose => Old::Transpose,
        New::Rotate90 => Old::Rotate90,
        New::Transverse => Old::Transverse,
        New::Rotate270 => Old::Rotate270,
        New::Unknown => Old::Unknown,
    };

    Ok(rawloader::RawImage {
        make: img.make,
        model: img.model,
        clean_make: img.clean_make,
        clean_model: img.clean_model,
        width: img.width,
        height: img.height,
        cpp: img.cpp,
        wb_coeffs: img.wb_coeffs,
        whitelevels: per_colour(img.whitelevel.as_bayer_array()),
        blacklevels: per_colour(img.blacklevel.as_bayer_array()),
        xyz_to_cam,
        cfa,
        crops,
        blackareas: Vec::new(),
        orientation,
        data: match img.data {
            RawImageData::Integer(data) => rawloader::RawImageData::Integer(data),
            RawImageData::Float(data) => rawloader::RawImageData::Float(data),
        },
    })
}

/// How the camera's own JPEG maps the lightness of imagepipe's output, as
/// pairs of normalised CIE L* (imagepipe output, camera JPEG).
///
/// Measured on 38 cameras from 11 makers: the median over cameras of the
/// median camera-JPEG lightness for each output lightness, in areas without
/// texture. Values above 0.85 are extended smoothly to (1, 1).
const CAMERA_LOOK: [(f32, f32); 10] = [
    (0.05, 0.036),
    (0.10, 0.08),
    (0.20, 0.20),
    (0.30, 0.33),
    (0.40, 0.46),
    (0.50, 0.59),
    (0.60, 0.715),
    (0.70, 0.815),
    (0.80, 0.90),
    (0.90, 0.955),
];

/// Tone curve points for the camera look: the pipeline's own base curve,
/// followed by `CAMERA_LOOK`.
fn camera_look_curve(base: &SplineFunc) -> Vec<(f32, f32)> {
    const STEPS: usize = 32;
    let look = SplineFunc::new(&CAMERA_LOOK);
    (1..STEPS)
        .map(|i| {
            let x = i as f32 / STEPS as f32;
            (x, look.interpolate(base.interpolate(x)).clamp(0.0, 1.0))
        })
        .collect()
}

/// Multiply the light in RAW samples by `gain`: the part above the black
/// level is scaled, and the black level stays.
fn scale_raw_samples(raw: &mut rawloader::RawImage, gain: f32) {
    // imagepipe subtracts one black level for every sample (the first).
    let black = raw.blacklevels[0] as f32;
    match &mut raw.data {
        rawloader::RawImageData::Integer(data) => {
            for v in data.iter_mut() {
                let light = (*v as f32 - black).max(0.0);
                *v = (black + light * gain).round() as u16;
            }
        }
        rawloader::RawImageData::Float(data) => {
            for v in data.iter_mut() {
                *v = black + (*v - black).max(0.0) * gain;
            }
        }
    }
}

/// Tone curve points that apply an exposure change of `ev` stops, followed
/// by the pipeline's own base curve.
///
/// imagepipe's `basecurve.exposure` setting multiplies the curve output, which
/// clips highlights from about +0.7 EV. This curve applies the gain in linear
/// light with a highlight roll-off, so white stays white and nothing clips.
/// The points are in normalised CIE L*, which is what the curve operates on.
fn exposure_curve(ev: f32, base: &SplineFunc) -> Vec<(f32, f32)> {
    const STEPS: usize = 32;
    let gain = ev.exp2();
    let l_to_y = |l: f32| {
        let l = l * 100.0;
        if l > 8.0 {
            ((l + 16.0) / 116.0).powi(3)
        } else {
            l * 27.0 / 24389.0
        }
    };
    let y_to_l = |y: f32| {
        let l = if y > 216.0 / 24389.0 {
            116.0 * y.cbrt() - 16.0
        } else {
            y * 24389.0 / 27.0
        };
        l / 100.0
    };
    (1..STEPS)
        .map(|i| {
            let l = i as f32 / STEPS as f32;
            let y = l_to_y(l);
            // Slope `gain` in the shadows; maps 0 to 0 and 1 to 1.
            let y = gain * y / (1.0 + (gain - 1.0) * y);
            (l, base.interpolate(y_to_l(y)).clamp(0.0, 1.0))
        })
        .collect()
}

/// Copy `input` to `output` unchanged, with its modification time.
///
/// The copy goes to a temporary file, is compared byte for byte with the
/// source, and only then gets its final name. The source is never changed.
fn copy_original(input: &Path, output: &Path, with_jpeg: bool) -> Result<()> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create output directory {}", parent.display()))?;
    }
    let tmp = temp_path_for(output);
    let _guard = TempFileGuard::register(&tmp);
    let unwritable = || format!("Failed to write output file {}", output.display());

    let result = (|| -> Result<()> {
        std::fs::copy(input, &tmp).with_context(unwritable)?;
        let copy = std::fs::File::options()
            .write(true)
            .open(&tmp)
            .with_context(unwritable)?;
        copy.sync_all().with_context(unwritable)?;
        if let Ok(modified) = std::fs::metadata(input).and_then(|m| m.modified()) {
            copy.set_modified(modified).with_context(unwritable)?;
        }
        drop(copy);
        if !same_contents(input, &tmp)? {
            return Err(anyhow!(
                "The copy of {} differs from the original; the disk may be failing",
                input.display()
            ));
        }
        std::fs::rename(&tmp, output).with_context(unwritable)
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    copy_sidecars(input, output)?;
    if with_jpeg {
        copy_companions(input, output)?;
    }
    Ok(())
}

/// Copy the JPEG or HEIF files that a camera set to RAW+JPEG wrote next to
/// `input` with the same name, such as `DSC1.JPG` for `DSC1.ARW`. Each copy
/// keeps its modification time and is compared with the original; an
/// existing file at the destination is kept.
fn copy_companions(input: &Path, output: &Path) -> Result<()> {
    const EXTENSIONS: [&str; 4] = ["jpg", "jpeg", "hif", "heic"];
    let (Some(dir), Some(stem)) = (input.parent(), input.file_stem()) else {
        return Ok(());
    };
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let source = entry.path();
        let Some(ext) = source.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if source.file_stem() != Some(stem)
            || !EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
            || !source.is_file()
        {
            continue;
        }
        let destination = output.with_extension(ext);
        if destination.exists() {
            continue;
        }
        copy_verified(&source, &destination, true).map_err(|e| {
            anyhow!("Copied {} but not {}: {e:#}", input.display(), source.display())
        })?;
    }
    Ok(())
}

/// Copy `source` to `destination` through a temporary file, compare the
/// bytes, and rename. With `keep_time`, the copy gets the modification time
/// of the source.
fn copy_verified(source: &Path, destination: &Path, keep_time: bool) -> Result<()> {
    let tmp = temp_path_for(destination);
    let _guard = TempFileGuard::register(&tmp);
    let result = (|| -> Result<()> {
        std::fs::copy(source, &tmp)?;
        if keep_time {
            if let Ok(modified) = std::fs::metadata(source).and_then(|m| m.modified()) {
                std::fs::File::options().write(true).open(&tmp)?.set_modified(modified)?;
            }
        }
        if !same_contents(source, &tmp)? {
            return Err(anyhow!("the copy differs from the original"));
        }
        std::fs::rename(&tmp, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Copy the XMP sidecars of `input` next to `output`, in the same naming
/// style: `NAME.xmp` (Lightroom, Capture One) or `NAME.EXT.xmp` (darktable).
/// They hold ratings and edits, which an archive must keep. An existing
/// sidecar at the destination is kept, because it may hold newer edits.
fn copy_sidecars(input: &Path, output: &Path) -> Result<()> {
    let mut pairs = Vec::new();
    for ext in ["xmp", "XMP"] {
        pairs.push((input.with_extension(ext), output.with_extension(ext)));
    }
    let with_suffix = |p: &Path| {
        let mut name = p.as_os_str().to_owned();
        name.push(".xmp");
        PathBuf::from(name)
    };
    pairs.push((with_suffix(input), with_suffix(output)));

    for (source, destination) in pairs {
        if !source.is_file() || destination.exists() {
            continue;
        }
        if let Err(e) = copy_verified(&source, &destination, false) {
            return Err(anyhow!(
                "Copied {} but not its sidecar {}: {e:#}",
                input.display(),
                source.display()
            ));
        }
    }
    Ok(())
}

/// True if the two files have the same bytes.
fn same_contents(a: &Path, b: &Path) -> Result<bool> {
    use std::io::Read;
    let open = |p: &Path| {
        std::fs::File::open(p).with_context(|| format!("Failed to read {}", p.display()))
    };
    let (mut fa, mut fb) = (open(a)?, open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// Temporary files that are being written, so that an interrupt can remove
/// them. Without this, Ctrl-C leaves hidden `.NAME.rawdog-tmp` files.
static ACTIVE_TEMP_FILES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Keeps a temporary file in `ACTIVE_TEMP_FILES` while it is in use.
struct TempFileGuard(PathBuf);

impl TempFileGuard {
    fn register(path: &Path) -> TempFileGuard {
        if let Ok(mut active) = ACTIVE_TEMP_FILES.lock() {
            active.push(path.to_path_buf());
        }
        TempFileGuard(path.to_path_buf())
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_TEMP_FILES.lock() {
            active.retain(|p| p != &self.0);
        }
    }
}

/// Remove every temporary file that is being written. For use when the
/// process is interrupted.
pub fn remove_active_temp_files() {
    // A conversion thread can hold the lock; do not wait for it.
    if let Ok(active) = ACTIVE_TEMP_FILES.try_lock() {
        remove_files(&active);
    }
}

fn remove_files(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

fn temp_path_for(output: &Path) -> PathBuf {
    let name = output.file_name().unwrap_or_default().to_string_lossy();
    output.with_file_name(format!(".{name}.rawdog-tmp"))
}

fn render_8bit(pipeline: &mut Pipeline, input: &Path, opts: &ConvertOptions) -> Result<RgbImage> {
    let decoded = pipeline
        .output_8bit(None)
        .map_err(|e| anyhow!("Failed to process {}: {}", input.display(), e))?;

    let width = decoded.width as u32;
    let height = decoded.height as u32;

    let img: RgbImage = ImageBuffer::from_raw(width, height, decoded.data)
        .context("Failed to construct image buffer from decoded data")?;

    let mut img = maybe_resize_8bit(trim_fast_edge(img, opts.resize, opts.fast), opts.resize);
    if let Some(amount) = opts.sharpen {
        sharpen(&mut img, amount);
    }
    Ok(img)
}

fn render_16bit_sharpened(
    pipeline: &mut Pipeline,
    input: &Path,
    opts: &ConvertOptions,
) -> Result<ImageBuffer<Rgb<u16>, Vec<u16>>> {
    let mut img = render_16bit(pipeline, input, opts.resize, opts.fast)?;
    if let Some(amount) = opts.sharpen {
        sharpen(&mut img, amount);
    }
    Ok(img)
}

/// The complete JPEG file, with metadata, at `opts.quality` or, with
/// `opts.max_bytes`, at the highest quality that fits.
fn encode_jpeg(img: &RgbImage, input: &Path, opts: &ConvertOptions) -> Result<Vec<u8>> {
    // Metadata failures are non-fatal: we still produce the output image.
    let mut exif_data = exif_utils::extract_exif(input, img.dimensions(), &opts.metadata)
        .ok()
        .flatten();
    let mut encode = |quality: u8| -> Result<Vec<u8>> {
        let mut jpeg_buf = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut jpeg_buf, quality);
        if opts.metadata.srgb {
            encoder.set_icc_profile(exif_utils::srgb_icc_profile().to_vec())?;
        }
        img.write_with_encoder(encoder)
            .context("Failed to encode JPEG")?;
        if let Some(exif) = &exif_data {
            match exif_utils::inject_exif_into_jpeg(&jpeg_buf, exif) {
                Ok(with_exif) => return Ok(with_exif),
                Err(e) => {
                    eprintln!(
                        "Warning: metadata of {} was not copied: {e}",
                        input.display()
                    );
                    exif_data = None;
                }
            }
        }
        Ok(jpeg_buf)
    };
    match opts.max_bytes {
        None => encode(opts.quality),
        Some(limit) => fit_jpeg(limit, opts.quality, &mut encode),
    }
}

/// The output of `encode` at the highest quality, up to `max_quality`,
/// whose size is at most `limit` bytes. JPEG size grows with quality, so
/// a binary search finds it.
fn fit_jpeg(
    limit: u64,
    max_quality: u8,
    encode: &mut impl FnMut(u8) -> Result<Vec<u8>>,
) -> Result<Vec<u8>> {
    let fits = |bytes: &Vec<u8>| bytes.len() as u64 <= limit;
    let first = encode(max_quality)?;
    if fits(&first) {
        return Ok(first);
    }
    let mut smallest = first.len();
    let mut best = None;
    let (mut low, mut high) = (1, max_quality - 1);
    while low <= high {
        let quality = low + (high - low) / 2;
        let bytes = encode(quality)?;
        if fits(&bytes) {
            best = Some(bytes);
            low = quality + 1;
        } else {
            smallest = smallest.min(bytes.len());
            if quality == 1 {
                break;
            }
            high = quality - 1;
        }
    }
    best.ok_or_else(|| {
        anyhow!(
            "the JPEG is {} at the lowest quality, larger than --max-size {}; use --resize for a smaller image",
            format_size(smallest as u64),
            format_size(limit)
        )
    })
}

/// A file size in the decimal units that `--max-size` accepts.
fn format_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} bytes"),
        1_000..1_000_000 => format!("{:.0} KB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

fn write_tiff(
    img: &ImageBuffer<Rgb<u16>, Vec<u16>>,
    input: &Path,
    out: &mut (impl Write + std::io::Seek),
    opts: &ConvertOptions,
) -> Result<()> {
    // EXIF failures are non-fatal: fall back to a TIFF without metadata.
    let fields = exif_utils::read_fields_for_output(input, img.dimensions(), &opts.metadata)
        .ok()
        .flatten();
    match fields {
        Some((fields, _)) => exif_utils::write_tiff_16bit_with_exif(
            out,
            img.dimensions(),
            img.as_raw(),
            &fields,
            opts.metadata.srgb,
        ),
        None if opts.metadata.srgb => exif_utils::write_tiff_16bit_with_exif(
            out,
            img.dimensions(),
            img.as_raw(),
            &[],
            true,
        ),
        None => img
            .write_with_encoder(TiffEncoder::new(out))
            .map_err(Into::into),
    }
    .context("Failed to encode TIFF")
}

fn write_png(
    img: &ImageBuffer<Rgb<u16>, Vec<u16>>,
    input: &Path,
    out: &mut impl Write,
    opts: &ConvertOptions,
) -> Result<()> {
    // Encode PNG to an in-memory buffer so we can inject EXIF metadata.
    let mut buf = Vec::new();
    let mut encoder = PngEncoder::new(&mut buf);
    if opts.metadata.srgb {
        encoder.set_icc_profile(exif_utils::srgb_icc_profile().to_vec())?;
    }
    img.write_with_encoder(encoder)
        .context("Failed to encode PNG")?;

    // Inject EXIF metadata into the PNG if available.
    let exif_data = exif_utils::extract_exif(input, img.dimensions(), &opts.metadata)
        .ok()
        .flatten();
    let final_bytes = match exif_data {
        Some(exif) => exif_utils::inject_exif_into_png(&buf, &exif).unwrap_or(buf),
        None => buf,
    };

    out.write_all(&final_bytes)?;
    Ok(())
}

fn render_16bit(
    pipeline: &mut Pipeline,
    input: &Path,
    resize: Option<u32>,
    fast: bool,
) -> Result<ImageBuffer<Rgb<u16>, Vec<u16>>> {
    // imagepipe's full pipeline (always used for RAW files) returns linear
    // 16-bit data, while its raster fast path returns the source file's
    // already-encoded samples. Mirror the fast-path condition in output_16bit.
    let is_linear = !(matches!(pipeline.globals.image, ImageSource::Other(_))
        && pipeline.globals.settings.use_fastpath
        && pipeline.default_ops());

    let decoded = pipeline
        .output_16bit(None)
        .map_err(|e| anyhow!("Failed to process {}: {}", input.display(), e))?;

    let width = decoded.width as u32;
    let height = decoded.height as u32;

    let img: ImageBuffer<Rgb<u16>, Vec<u16>> =
        ImageBuffer::from_raw(width, height, decoded.data)
            .context("Failed to construct 16-bit image buffer from decoded data")?;

    // Resize in linear light, then encode.
    let mut img = maybe_resize_16bit(trim_fast_edge(img, resize, fast), resize);
    if is_linear {
        let lut = srgb_encode_lut();
        for v in img.iter_mut() {
            *v = lut[*v as usize];
        }
    }
    Ok(img)
}

/// A sample type that `sharpen` can process.
trait Sample: image::Primitive {
    fn as_f32(self) -> f32;
    fn from_f32(v: f32) -> Self;
}

impl Sample for u8 {
    fn as_f32(self) -> f32 {
        self as f32
    }
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, u8::MAX as f32) as u8
    }
}

impl Sample for u16 {
    fn as_f32(self) -> f32 {
        self as f32
    }
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, u16::MAX as f32) as u16
    }
}

/// Unsharp mask: add `amount` times the difference between the image and a
/// blurred copy of it. The radius suits images that were resized for screens.
fn sharpen<S>(img: &mut ImageBuffer<Rgb<S>, Vec<S>>, amount: f32)
where
    S: Sample,
    Rgb<S>: image::Pixel<Subpixel = S> + 'static,
{
    const SIGMA: f32 = 1.0;
    let blurred = image::imageops::blur(img, SIGMA);
    for (v, b) in img.iter_mut().zip(blurred.iter()) {
        let (v_f, b_f) = (v.as_f32(), b.as_f32());
        *v = S::from_f32(v_f + amount * (v_f - b_f));
    }
}

/// Blend `mark` into the bottom-right corner of `img`. The long side of the
/// mark is a fifth of the image's short edge, so the mark has the same size
/// on landscape and portrait images; the margin is 3% of the short edge.
fn apply_watermark<S>(img: &mut ImageBuffer<Rgb<S>, Vec<S>>, mark: &image::RgbaImage)
where
    S: Sample,
    Rgb<S>: image::Pixel<Subpixel = S> + 'static,
{
    let (width, height) = img.dimensions();
    let short = width.min(height) as f32;
    let (mark_w, mark_h) = mark.dimensions();
    let scale = short * 0.2 / mark_w.max(mark_h) as f32;
    let new_w = ((mark_w as f32 * scale).round() as u32).max(1);
    let new_h = ((mark_h as f32 * scale).round() as u32).max(1);
    let margin = (short * 0.03).round() as u32;
    if new_w + margin > width || new_h + margin > height {
        return;
    }

    // Scale with premultiplied alpha, so the colour of transparent pixels
    // does not leave a dark or light fringe around the mark.
    let premultiplied = image::Rgba32FImage::from_fn(mark_w, mark_h, |x, y| {
        let p = mark.get_pixel(x, y);
        let a = p[3] as f32 / 255.0;
        image::Rgba([
            p[0] as f32 / 255.0 * a,
            p[1] as f32 / 255.0 * a,
            p[2] as f32 / 255.0 * a,
            a,
        ])
    });
    let scaled = image::imageops::resize(
        &premultiplied,
        new_w,
        new_h,
        image::imageops::FilterType::Lanczos3,
    );

    let max = S::DEFAULT_MAX_VALUE.as_f32();
    let (left, top) = (width - margin - new_w, height - margin - new_h);
    for (x, y, p) in scaled.enumerate_pixels() {
        let a = p[3].clamp(0.0, 1.0);
        if a == 0.0 {
            continue;
        }
        let dst = img.get_pixel_mut(left + x, top + y);
        for c in 0..3 {
            let fg = p[c].clamp(0.0, a) * max;
            dst[c] = S::from_f32(dst[c].as_f32() * (1.0 - a) + fg);
        }
    }
}

/// Lookup table that maps linear 16-bit samples to sRGB-encoded 16-bit samples.
fn srgb_encode_lut() -> Vec<u16> {
    (0..=u16::MAX)
        .map(|i| {
            let v = i as f64 / 65535.0;
            let encoded = if v <= 0.0031308 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            };
            (encoded * 65535.0).round().clamp(0.0, 65535.0) as u16
        })
        .collect()
}

/// Remove one pixel from every side of an image that imagepipe scaled in
/// `fast` mode. Its scaled demosaic leaves the red channel empty in the last
/// sensor row and column, and the image rotation can put those on any side.
///
/// The pipeline was asked for a long edge of `resize + 2`, so the result has
/// a long edge of `resize`. An image that was not scaled is returned as it is.
fn trim_fast_edge<P>(
    img: ImageBuffer<P, Vec<P::Subpixel>>,
    resize: Option<u32>,
    fast: bool,
) -> ImageBuffer<P, Vec<P::Subpixel>>
where
    P: image::Pixel + 'static,
{
    let (width, height) = img.dimensions();
    match resize {
        Some(max_edge)
            if fast
                && max_edge >= FAST_MIN_EDGE
                && width.max(height) == max_edge + 2
                && width.min(height) > 2 =>
        {
            image::imageops::crop_imm(&img, 1, 1, width - 2, height - 2).to_image()
        }
        _ => img,
    }
}

fn maybe_resize_8bit(
    img: ImageBuffer<Rgb<u8>, Vec<u8>>,
    resize: Option<u32>,
) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
    if let Some(max_edge) = resize {
        let (width, height) = img.dimensions();
        let long_edge = width.max(height);
        if long_edge > max_edge {
            let scale = max_edge as f64 / long_edge as f64;
            let new_w = ((width as f64 * scale).round() as u32).max(1);
            let new_h = ((height as f64 * scale).round() as u32).max(1);
            return image::imageops::resize(&img, new_w, new_h, image::imageops::FilterType::Lanczos3);
        }
    }
    img
}

fn maybe_resize_16bit(
    img: ImageBuffer<Rgb<u16>, Vec<u16>>,
    resize: Option<u32>,
) -> ImageBuffer<Rgb<u16>, Vec<u16>> {
    if let Some(max_edge) = resize {
        let (width, height) = img.dimensions();
        let long_edge = width.max(height);
        if long_edge > max_edge {
            let scale = max_edge as f64 / long_edge as f64;
            let new_w = ((width as f64 * scale).round() as u32).max(1);
            let new_h = ((height as f64 * scale).round() as u32).max(1);
            return image::imageops::resize(&img, new_w, new_h, image::imageops::FilterType::Lanczos3);
        }
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageBuffer;

    // Helper to create a solid-color 8-bit test image
    fn make_8bit_image(width: u32, height: u32) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
        ImageBuffer::from_fn(width, height, |_x, _y| Rgb([128u8, 64, 32]))
    }

    // Helper to create a solid-color 16-bit test image
    fn make_16bit_image(width: u32, height: u32) -> ImageBuffer<Rgb<u16>, Vec<u16>> {
        ImageBuffer::from_fn(width, height, |_x, _y| Rgb([32768u16, 16384, 8192]))
    }

    // -----------------------------------------------------------------------
    // apply_watermark tests
    // -----------------------------------------------------------------------

    #[test]
    fn watermark_goes_into_the_bottom_right_corner() {
        // An opaque white mark, 10x5, on a black 600x400 image: the mark
        // becomes 80x40 (a fifth of 400) with a margin of 12.
        let mark = image::RgbaImage::from_pixel(10, 5, image::Rgba([255, 255, 255, 255]));
        let mut img = RgbImage::new(600, 400);
        apply_watermark(&mut img, &mark);
        assert_eq!(img.get_pixel(600 - 12 - 40, 400 - 12 - 20)[0], 255);
        assert_eq!(img.get_pixel(600 - 12 - 79, 400 - 12 - 39)[1], 255);
        assert_eq!(img.get_pixel(600 - 12 - 82, 400 - 12 - 20)[0], 0);
        assert_eq!(img.get_pixel(600 - 5, 400 - 5)[0], 0);
        assert_eq!(img.get_pixel(10, 10)[0], 0);
    }

    #[test]
    fn watermark_blends_with_alpha_in_16_bit() {
        let mark = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 255, 255, 128]));
        let mut img: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::new(500, 500);
        apply_watermark(&mut img, &mark);
        let v = img.get_pixel(500 - 15 - 50, 500 - 15 - 50)[0];
        assert!((32_000..=33_500).contains(&v), "{v}");
    }

    // -----------------------------------------------------------------------
    // fit_jpeg tests
    // -----------------------------------------------------------------------

    /// An encoder whose output has `10 * quality` bytes, which counts calls.
    fn fake_encoder(calls: &mut u32) -> impl FnMut(u8) -> Result<Vec<u8>> + '_ {
        move |quality| {
            *calls += 1;
            Ok(vec![0; 10 * quality as usize])
        }
    }

    #[test]
    fn fit_jpeg_keeps_max_quality_when_it_fits() {
        let mut calls = 0;
        let bytes = fit_jpeg(2000, 92, &mut fake_encoder(&mut calls)).unwrap();
        assert_eq!(bytes.len(), 920);
        assert_eq!(calls, 1);
    }

    #[test]
    fn fit_jpeg_finds_highest_quality_that_fits() {
        for limit in [10, 11, 455, 500, 919] {
            let mut calls = 0;
            let bytes = fit_jpeg(limit, 92, &mut fake_encoder(&mut calls)).unwrap();
            assert_eq!(bytes.len() as u64, limit / 10 * 10, "limit {limit}");
            assert!(calls <= 8, "{calls} encodes for limit {limit}");
        }
    }

    #[test]
    fn fit_jpeg_reports_size_at_lowest_quality() {
        let mut calls = 0;
        let err = fit_jpeg(9, 92, &mut fake_encoder(&mut calls)).unwrap_err();
        assert!(err.to_string().contains("10 bytes at the lowest quality"), "{err}");
        let err = fit_jpeg(9, 1, &mut fake_encoder(&mut calls)).unwrap_err();
        assert!(err.to_string().contains("10 bytes"), "{err}");
    }

    #[test]
    fn format_size_uses_decimal_units() {
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(512_000), "512 KB");
        assert_eq!(format_size(2_340_000), "2.3 MB");
    }

    // -----------------------------------------------------------------------
    // maybe_resize_8bit tests
    // -----------------------------------------------------------------------

    #[test]
    fn resize_8bit_none_returns_original() {
        let img = make_8bit_image(200, 100);
        let result = maybe_resize_8bit(img, None);
        assert_eq!(result.dimensions(), (200, 100));
    }

    #[test]
    fn resize_8bit_larger_max_returns_original() {
        // max_edge (1000) is larger than long edge (200), so no resize
        let img = make_8bit_image(200, 100);
        let result = maybe_resize_8bit(img, Some(1000));
        assert_eq!(result.dimensions(), (200, 100));
    }

    #[test]
    fn resize_8bit_equal_max_returns_original() {
        // max_edge equals the long edge -- no resize needed
        let img = make_8bit_image(200, 100);
        let result = maybe_resize_8bit(img, Some(200));
        assert_eq!(result.dimensions(), (200, 100));
    }

    #[test]
    fn resize_8bit_landscape_scales_down() {
        let img = make_8bit_image(1000, 500);
        let result = maybe_resize_8bit(img, Some(500));
        // long edge 1000 -> 500, short edge 500 -> 250
        assert_eq!(result.dimensions(), (500, 250));
    }

    #[test]
    fn resize_8bit_portrait_scales_down() {
        let img = make_8bit_image(500, 1000);
        let result = maybe_resize_8bit(img, Some(500));
        assert_eq!(result.dimensions(), (250, 500));
    }

    #[test]
    fn resize_8bit_square_scales_down() {
        let img = make_8bit_image(800, 800);
        let result = maybe_resize_8bit(img, Some(400));
        assert_eq!(result.dimensions(), (400, 400));
    }

    #[test]
    fn resize_8bit_preserves_aspect_ratio() {
        let img = make_8bit_image(600, 400);
        let result = maybe_resize_8bit(img, Some(300));
        let (w, h) = result.dimensions();
        assert_eq!(w, 300);
        assert_eq!(h, 200);
    }

    #[test]
    fn resize_extreme_aspect_ratio_keeps_short_edge_at_least_one() {
        let result = maybe_resize_8bit(make_8bit_image(1000, 2), Some(100));
        assert_eq!(result.dimensions(), (100, 1));
        let result = maybe_resize_16bit(make_16bit_image(2, 1000), Some(100));
        assert_eq!(result.dimensions(), (1, 100));
    }

    // -----------------------------------------------------------------------
    // maybe_resize_16bit tests
    // -----------------------------------------------------------------------

    #[test]
    fn resize_16bit_none_returns_original() {
        let img = make_16bit_image(200, 100);
        let result = maybe_resize_16bit(img, None);
        assert_eq!(result.dimensions(), (200, 100));
    }

    #[test]
    fn resize_16bit_larger_max_returns_original() {
        let img = make_16bit_image(200, 100);
        let result = maybe_resize_16bit(img, Some(1000));
        assert_eq!(result.dimensions(), (200, 100));
    }

    #[test]
    fn resize_16bit_equal_max_returns_original() {
        let img = make_16bit_image(200, 100);
        let result = maybe_resize_16bit(img, Some(200));
        assert_eq!(result.dimensions(), (200, 100));
    }

    #[test]
    fn resize_16bit_landscape_scales_down() {
        let img = make_16bit_image(1000, 500);
        let result = maybe_resize_16bit(img, Some(500));
        assert_eq!(result.dimensions(), (500, 250));
    }

    #[test]
    fn resize_16bit_portrait_scales_down() {
        let img = make_16bit_image(500, 1000);
        let result = maybe_resize_16bit(img, Some(500));
        assert_eq!(result.dimensions(), (250, 500));
    }

    #[test]
    fn resize_16bit_square_scales_down() {
        let img = make_16bit_image(800, 800);
        let result = maybe_resize_16bit(img, Some(400));
        assert_eq!(result.dimensions(), (400, 400));
    }

    #[test]
    fn resize_16bit_preserves_aspect_ratio() {
        let img = make_16bit_image(600, 400);
        let result = maybe_resize_16bit(img, Some(300));
        let (w, h) = result.dimensions();
        assert_eq!(w, 300);
        assert_eq!(h, 200);
    }

    // -----------------------------------------------------------------------
    // 16-bit transfer curve tests
    // -----------------------------------------------------------------------

    fn mean_u8(data: &[u8]) -> f64 {
        data.iter().map(|&v| v as f64).sum::<f64>() / data.len() as f64
    }

    fn mean_u16_as_u8(data: &[u16]) -> f64 {
        data.iter().map(|&v| v as f64 / 257.0).sum::<f64>() / data.len() as f64
    }

    fn write_gradient_png(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("gradient.png");
        let img: RgbImage = ImageBuffer::from_fn(64, 16, |x, _| {
            Rgb([(x * 4) as u8, (x * 3) as u8, (x * 2) as u8])
        });
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn render_16bit_full_pipeline_matches_8bit_brightness() {
        // RAW files always take imagepipe's full pipeline, where output_16bit
        // returns linear samples. Force that path with a raster source.
        let tmp = tempfile::TempDir::new().unwrap();
        let png = write_gradient_png(tmp.path());

        let mut p8 = Pipeline::new_from_file(&png).unwrap();
        p8.globals.settings.use_fastpath = false;
        let out8 = p8.output_8bit(None).unwrap();

        let mut p16 = Pipeline::new_from_file(&png).unwrap();
        p16.globals.settings.use_fastpath = false;
        let out16 = render_16bit(&mut p16, &png, None, false).unwrap();

        let (m8, m16) = (mean_u8(&out8.data), mean_u16_as_u8(out16.as_raw()));
        assert!(
            (m8 - m16).abs() < 2.0,
            "8-bit mean {m8:.1} vs 16-bit mean {m16:.1}"
        );
    }

    #[test]
    fn render_16bit_fast_path_is_not_encoded_twice() {
        // For unmodified raster sources imagepipe returns the file's samples
        // unchanged, which are already sRGB-encoded.
        let tmp = tempfile::TempDir::new().unwrap();
        let png = write_gradient_png(tmp.path());

        let mut p16 = Pipeline::new_from_file(&png).unwrap();
        let out16 = render_16bit(&mut p16, &png, None, false).unwrap();
        let expected = image::open(&png).unwrap().to_rgb16();
        assert_eq!(out16.as_raw(), expected.as_raw());
    }

    #[test]
    fn srgb_encode_lut_endpoints_and_midpoint() {
        let lut = srgb_encode_lut();
        assert_eq!(lut[0], 0);
        assert_eq!(lut[65535], 65535);
        // Linear 0.5 encodes to about 0.7354 in sRGB.
        assert!((lut[32768] as f64 / 65535.0 - 0.7354).abs() < 0.001);
        // The linear segment below 0.0031308 has a slope of 12.92.
        assert_eq!(lut[100], (100.0f64 * 12.92).round() as u16);
    }

    #[test]
    fn trim_fast_edge_removes_one_pixel_from_every_side_of_scaled_images() {
        let img = ImageBuffer::from_fn(102, 52, |x, y| Rgb([x as u8, y as u8, 0]));
        let out = trim_fast_edge(img, Some(100), true);
        assert_eq!(out.dimensions(), (100, 50));
        assert_eq!(out.get_pixel(0, 0), &Rgb([1u8, 1, 0]));
        assert_eq!(out.get_pixel(99, 49), &Rgb([100u8, 50, 0]));
    }

    #[test]
    fn trim_fast_edge_leaves_other_images_unchanged() {
        // Not fast.
        assert_eq!(trim_fast_edge(make_8bit_image(102, 52), Some(100), false).dimensions(), (102, 52));
        // Smaller than the limit, so the pipeline did not scale it.
        assert_eq!(trim_fast_edge(make_8bit_image(80, 40), Some(100), true).dimensions(), (80, 40));
        // No limit.
        assert_eq!(trim_fast_edge(make_16bit_image(102, 52), None, true).dimensions(), (102, 52));
        // Below the smallest size that uses decoder scaling.
        assert_eq!(trim_fast_edge(make_8bit_image(10, 6), Some(8), true).dimensions(), (10, 6));
    }

    #[test]
    fn temp_file_guard_registers_only_files_in_use() {
        // Other tests run in parallel and register their own files, so this
        // test checks its own paths only and never removes the whole list.
        let tmp = tempfile::TempDir::new().unwrap();
        let in_use = tmp.path().join(".a.jpg.rawdog-tmp");
        let finished = tmp.path().join(".b.jpg.rawdog-tmp");
        let active = || ACTIVE_TEMP_FILES.lock().unwrap().clone();

        let _guard = TempFileGuard::register(&in_use);
        drop(TempFileGuard::register(&finished));
        assert!(active().contains(&in_use));
        assert!(!active().contains(&finished));

        std::fs::write(&in_use, b"x").unwrap();
        remove_files(std::slice::from_ref(&in_use));
        assert!(!in_use.exists());
    }

    #[test]
    fn copy_original_copies_bytes_and_modification_time() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("DSC1.ARW");
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&input, &data).unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options().write(true).open(&input).unwrap().set_modified(old).unwrap();

        let output = tmp.path().join("2023/11/DSC1.ARW");
        copy_original(&input, &output, false).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), data);
        assert_eq!(std::fs::metadata(&output).unwrap().modified().unwrap(), old);
        assert_eq!(std::fs::read(&input).unwrap(), data, "source changed");
        assert!(!temp_path_for(&output).exists());
    }

    #[test]
    fn copy_original_copies_sidecars_in_both_naming_styles() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("DSC1.ARW");
        std::fs::write(&input, b"raw").unwrap();
        std::fs::write(tmp.path().join("DSC1.xmp"), b"lightroom").unwrap();
        std::fs::write(tmp.path().join("DSC1.ARW.xmp"), b"darktable").unwrap();

        let out_dir = tmp.path().join("archive");
        let output = out_dir.join("2024-01-01_DSC1.ARW");
        // A sidecar that already exists at the destination is kept.
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("2024-01-01_DSC1.ARW.xmp"), b"newer edits").unwrap();

        copy_original(&input, &output, false).unwrap();
        assert_eq!(std::fs::read(out_dir.join("2024-01-01_DSC1.xmp")).unwrap(), b"lightroom");
        assert_eq!(
            std::fs::read(out_dir.join("2024-01-01_DSC1.ARW.xmp")).unwrap(),
            b"newer edits"
        );
    }

    #[test]
    fn copy_original_copies_camera_jpegs_only_when_asked() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("DSC1.ARW");
        std::fs::write(&input, b"raw").unwrap();
        std::fs::write(tmp.path().join("DSC1.JPG"), b"jpeg").unwrap();
        std::fs::write(tmp.path().join("DSC1.HIF"), b"heif").unwrap();
        std::fs::write(tmp.path().join("DSC10.JPG"), b"other frame").unwrap();
        std::fs::write(tmp.path().join("DSC1.txt"), b"not an image").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(tmp.path().join("DSC1.JPG"))
            .unwrap()
            .set_modified(old)
            .unwrap();

        let out = tmp.path().join("plain");
        copy_original(&input, &out.join("x.ARW"), false).unwrap();
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 1);

        let out = tmp.path().join("archive");
        copy_original(&input, &out.join("2024_DSC1.ARW"), true).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["2024_DSC1.ARW", "2024_DSC1.HIF", "2024_DSC1.JPG"]);
        let copied = out.join("2024_DSC1.JPG");
        assert_eq!(std::fs::read(&copied).unwrap(), b"jpeg");
        assert_eq!(std::fs::metadata(&copied).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn same_contents_detects_a_difference() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (a, b, c) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"));
        std::fs::write(&a, vec![7u8; 2_500_000]).unwrap();
        std::fs::write(&b, vec![7u8; 2_500_000]).unwrap();
        let mut changed = vec![7u8; 2_500_000];
        changed[2_400_000] = 8;
        std::fs::write(&c, changed).unwrap();
        assert!(same_contents(&a, &b).unwrap());
        assert!(!same_contents(&a, &c).unwrap());
        std::fs::write(&c, vec![7u8; 10]).unwrap();
        assert!(!same_contents(&a, &c).unwrap());
    }

    // -----------------------------------------------------------------------
    // decoder result checks
    // -----------------------------------------------------------------------

    fn raw_image(data: Vec<u16>, white: u16) -> rawloader::RawImage {
        rawloader::RawImage {
            make: String::new(),
            model: String::new(),
            clean_make: String::new(),
            clean_model: String::new(),
            width: data.len(),
            height: 1,
            cpp: 1,
            wb_coeffs: [2.0, 1.0, 1.5, f32::NAN],
            whitelevels: [white; 4],
            blacklevels: [0; 4],
            xyz_to_cam: [[0.0; 3]; 4],
            cfa: rawloader::CFA::new("RGGB"),
            crops: [1, 1, 1, 1],
            blackareas: Vec::new(),
            orientation: rawloader::Orientation::Normal,
            data: rawloader::RawImageData::Integer(data),
        }
    }

    #[test]
    fn scale_raw_samples_scales_the_light_above_the_black_level() {
        let mut raw = raw_image(vec![512, 1512, 4512, 100], 15360);
        raw.blacklevels = [512; 4];
        scale_raw_samples(&mut raw, 0.25);
        let rawloader::RawImageData::Integer(data) = raw.data else {
            unreachable!()
        };
        // Black stays black; light is quartered; below black stays at black.
        assert_eq!(data, vec![512, 762, 1512, 512]);
    }

    #[test]
    fn implausible_samples_rejects_data_far_above_the_white_level() {
        let mut data = vec![1000u16; 1000];
        assert!(implausible_samples(&raw_image(data.clone(), 4095)).is_none());
        // Hot pixels: 0.5% above the limit is accepted.
        data[..5].fill(65535);
        assert!(implausible_samples(&raw_image(data.clone(), 4095)).is_none());
        data[..50].fill(65535);
        assert!(implausible_samples(&raw_image(data, 4095)).is_some());
    }

    // -----------------------------------------------------------------------
    // Source::Preview tests
    // -----------------------------------------------------------------------

    fn preview_opts(resize: Option<u32>) -> ConvertOptions {
        ConvertOptions {
            format: crate::OutputFormat::Jpeg,
            quality: 92,
            max_bytes: None,
            resize,
            fast: false,
            exposure: None,
            white_balance: None,
            sharpen: None,
            preview: true,
            look: Look::Flat,
            watermark: None,
            with_jpeg: false,
            metadata: Default::default(),
        }
    }

    #[test]
    fn preview_source_is_resized_for_8_bit_output() {
        let mut source = Source::Preview(make_8bit_image(400, 200));
        let img = source.rgb8(Path::new("x.arw"), &preview_opts(Some(100))).unwrap();
        assert_eq!(img.dimensions(), (100, 50));
    }

    #[test]
    fn preview_source_is_widened_for_16_bit_output() {
        let mut source = Source::Preview(ImageBuffer::from_pixel(8, 4, Rgb([0u8, 128, 255])));
        let img = source.rgb16(Path::new("x.arw"), &preview_opts(None)).unwrap();
        assert_eq!(img.dimensions(), (8, 4));
        assert_eq!(img.get_pixel(0, 0), &Rgb([0u16, 128 * 257, 65535]));
    }

    #[test]
    fn load_preview_reports_a_file_that_is_not_raw() {
        let tmp = tempfile::Builder::new().suffix(".arw").tempfile().unwrap();
        std::fs::write(tmp.path(), b"not a raw file").unwrap();
        let err = load_preview(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("preview"), "{err}");
    }

    // -----------------------------------------------------------------------
    // sharpen tests
    // -----------------------------------------------------------------------

    /// Left half dark, right half bright.
    fn edge_image<S>(dark: S, bright: S) -> ImageBuffer<Rgb<S>, Vec<S>>
    where
        S: Sample,
        Rgb<S>: image::Pixel<Subpixel = S>,
    {
        ImageBuffer::from_fn(16, 4, |x, _| Rgb([if x < 8 { dark } else { bright }; 3]))
    }

    #[test]
    fn sharpen_increases_contrast_at_an_edge_only() {
        let mut img = edge_image(100u8, 150u8);
        sharpen(&mut img, 1.0);
        // Next to the edge, the dark side gets darker and the bright side brighter.
        assert!(img.get_pixel(7, 1)[0] < 100);
        assert!(img.get_pixel(8, 1)[0] > 150);
        // Far from the edge, nothing changes.
        assert_eq!(img.get_pixel(0, 1)[0], 100);
        assert_eq!(img.get_pixel(15, 1)[0], 150);
    }

    #[test]
    fn sharpen_clamps_at_the_sample_limits() {
        let mut img = edge_image(0u16, u16::MAX);
        sharpen(&mut img, 3.0);
        assert_eq!(img.get_pixel(7, 1)[0], 0);
        assert_eq!(img.get_pixel(8, 1)[0], u16::MAX);
    }

    #[test]
    fn sharpen_zero_amount_changes_nothing() {
        let mut img = edge_image(100u8, 150u8);
        let before = img.clone();
        sharpen(&mut img, 0.0);
        assert_eq!(img, before);
    }

    // -----------------------------------------------------------------------
    // exposure_curve tests
    // -----------------------------------------------------------------------

    #[test]
    fn camera_look_curve_brightens_midtones_and_keeps_the_ends() {
        let base = SplineFunc::new(&[(0.5, 0.6)]);
        let points = camera_look_curve(&base);
        for pair in points.windows(2) {
            assert!(pair[1].1 > pair[0].1, "not increasing at {}", pair[1].0);
        }
        let curve = SplineFunc::new(&points);
        assert!(curve.interpolate(0.0).abs() < 1e-6);
        assert!((curve.interpolate(1.0) - 1.0).abs() < 1e-6);
        // Midtones: brighter than the base curve, as in camera JPEGs.
        assert!(curve.interpolate(0.5) > base.interpolate(0.5) + 0.05);
        // Deep shadows: not lifted.
        assert!(curve.interpolate(0.04) <= base.interpolate(0.04) + 0.01);
    }

    #[test]
    fn exposure_curve_zero_ev_equals_base_curve() {
        let base = SplineFunc::new(&[(0.5, 0.6)]);
        for (x, y) in exposure_curve(0.0, &base) {
            assert!((y - base.interpolate(x)).abs() < 1e-4, "at {x}");
        }
    }

    #[test]
    fn exposure_curve_brightens_or_darkens_without_clipping() {
        let base = SplineFunc::new(&[(0.5, 0.6)]);
        for ev in [-3.0f32, -1.0, 1.0, 3.0] {
            let points = exposure_curve(ev, &base);
            for pair in points.windows(2) {
                assert!(pair[1].1 > pair[0].1, "ev {ev}: not increasing at {}", pair[1].0);
            }
            for &(x, y) in &points {
                assert!(y > 0.0 && y < 1.0, "ev {ev}: {y} at {x}");
                if ev > 0.0 {
                    assert!(y > base.interpolate(x), "ev {ev} at {x}");
                } else {
                    assert!(y < base.interpolate(x), "ev {ev} at {x}");
                }
            }
        }
    }

    #[test]
    fn exposure_curve_one_stop_doubles_linear_shadows() {
        // Identity base curve. In deep shadows the roll-off is negligible,
        // so +1 EV must double the linear value. Below L* = 8, L* is linear.
        let identity = SplineFunc::new(&[]);
        let (x, y) = exposure_curve(1.0, &identity)[0];
        assert!((y / x - 2.0).abs() < 0.02, "{x} -> {y}");
    }

    // -----------------------------------------------------------------------
    // convert_raw error handling tests
    // -----------------------------------------------------------------------

    #[test]
    fn convert_raw_rejects_nonexistent_input() {
        let result = convert_raw(
            Path::new("/nonexistent/photo.arw"),
            Path::new("/tmp/out.jpg"),
            &ConvertOptions {
                format: crate::OutputFormat::Jpeg,
                quality: 92,
                max_bytes: None,
                resize: None,
                fast: false,
                exposure: None,
                white_balance: None,
                sharpen: None,
                preview: false,
                look: Look::Flat,
                watermark: None,
                with_jpeg: false,
                metadata: Default::default(),
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn convert_raw_rejects_non_arw_file() {
        // Create a temp file that is not a valid ARW
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not a raw file").unwrap();

        let result = convert_raw(
            tmp.path(),
            Path::new("/tmp/out.jpg"),
            &ConvertOptions {
                format: crate::OutputFormat::Jpeg,
                quality: 92,
                max_bytes: None,
                resize: None,
                fast: false,
                exposure: None,
                white_balance: None,
                sharpen: None,
                preview: false,
                look: Look::Flat,
                watermark: None,
                with_jpeg: false,
                metadata: Default::default(),
            },
        );
        assert!(result.is_err());
    }
}
