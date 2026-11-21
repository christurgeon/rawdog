use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
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
    /// Crop to this aspect ratio (long side, short side), centred, in the
    /// orientation of each image. Applied before `resize`.
    pub aspect: Option<(u32, u32)>,
    /// Scale during RAW decoding instead of after it. Needs `resize`.
    pub fast: bool,
    /// Exposure change in stops (EV).
    pub exposure: Option<f32>,
    /// White balance as (colour temperature in kelvin, tint). Replaces the
    /// as-shot white balance.
    pub white_balance: Option<(f32, f32)>,
    /// White balance multipliers taken from a reference file.
    pub wb_from: Option<std::sync::Arc<WhiteBalanceReference>>,
    /// Strength of output sharpening, applied after the resize.
    pub sharpen: Option<f32>,
    /// Use the JPEG preview that the camera embedded in the RAW file
    /// instead of developing the RAW data.
    pub preview: bool,
    /// With `-f original`, also copy the camera's JPEG or HEIF of each RAW
    /// file.
    pub with_jpeg: bool,
    /// A logo blended into the bottom-right corner of every image.
    pub watermark: Option<std::sync::Arc<image::RgbaImage>>,
    /// The `resize` values that get the watermark; empty for all.
    pub watermark_sizes: Vec<u32>,
    pub metadata: exif_utils::MetadataOptions,
}

/// The as-shot white balance of one RAW file, applied to other files of the
/// same camera model so that a sequence does not flicker.
pub struct WhiteBalanceReference {
    coeffs: [f32; 4],
    model: String,
}

/// Read the white balance of `path`. This decodes the file once.
pub fn white_balance_reference(path: &Path) -> Result<WhiteBalanceReference> {
    let pipeline = open_pipeline(path)
        .with_context(|| format!("Failed to read --wb-from file {}", path.display()))?;
    let ImageSource::Raw(raw) = &pipeline.globals.image else {
        bail!("--wb-from file {} is not a RAW file", path.display());
    };
    Ok(WhiteBalanceReference {
        coeffs: pipeline.ops.tolab.wb_coeffs,
        model: raw.clean_model.clone(),
    })
}

/// Open the pixels of `input`: its embedded preview with `--preview`, or
/// the RAW data with the development options applied. `None` when a
/// preview was asked for but cannot be read.
fn prepare_source(input: &Path, opts: &ConvertOptions) -> Result<Option<Source>> {
    // Name a permission problem as such, not as a corrupt file.
    if let Err(e) = std::fs::File::open(input) {
        bail!("cannot read the file: {e}");
    }
    let ConvertOptions {
        resize,
        fast,
        exposure,
        white_balance,
        ..
    } = *opts;

    let source = if opts.preview {
        // A file whose preview cannot be read is developed too; if the
        // file is broken, developing reports why.
        let Some(preview) = load_preview(input).ok().flatten() else {
            // Phones, Olympus, and some DNG files store no preview. Develop
            // those instead of failing, at --fast speed when resizing.
            return Ok(None);
        };
        warn_if_preview_is_small(&preview, resize);
        Source::Preview(preview)
    } else {
        let mut pipeline = open_pipeline(input)?;

        // With a size limit, imagepipe demosaics and scales in one pass when
        // the reduction is 2x or more. This is several times faster than a
        // full demosaic followed by a Lanczos resize, and slightly softer.
        // Two extra pixels are requested because `trim_fast_edge` removes
        // one pixel from every side. Very small sizes use the normal path.
        if let (true, Some(max_edge)) = (fast, fast_decode_edge(resize, opts.aspect)) {
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
        if let Some(reference) = &opts.wb_from {
            // Multipliers are specific to a sensor.
            let model = match &pipeline.globals.image {
                ImageSource::Raw(raw) => raw.clean_model.as_str(),
                ImageSource::Other(_) => "",
            };
            if model != reference.model {
                bail!(
                    "taken with {}, but the --wb-from file with {}; white balance multipliers \
                     differ between camera models",
                    if model.is_empty() { "no known camera" } else { model },
                    reference.model
                );
            }
            pipeline.ops.tolab.wb_coeffs = reference.coeffs;
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
    Ok(Some(source))
}

/// The options for developing a file whose preview cannot be read.
fn developed_instead_of_preview(input: &Path, opts: &ConvertOptions) -> ConvertOptions {
    eprintln!(
        "Note: {} has no embedded preview that rawdog can read; developing the RAW data instead",
        input.display()
    );
    let mut developed = opts.clone();
    developed.preview = false;
    developed.fast = opts.resize.is_some();
    developed.metadata.srgb = true;
    developed
}

/// Convert a single ARW file to the specified output format.
pub fn convert_raw(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<()> {
    if matches!(opts.format, OutputFormat::Original) {
        return copy_original(input, output, opts.with_jpeg);
    }

    let format = opts.format;
    let Some(mut source) = prepare_source(input, opts)? else {
        return convert_raw(input, output, &developed_instead_of_preview(input, opts));
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
                    let mut img = source.rgb8(opts)?;
                    if let Some(mark) = watermark_for(opts) {
                        apply_watermark(&mut img, mark);
                    }
                    let bytes = encode_jpeg(&img, input, opts)?;
                    out.write_all(&bytes).with_context(unwritable)?;
                }
                OutputFormat::Tiff => {
                    let mut img = source.rgb16(opts)?;
                    if let Some(mark) = watermark_for(opts) {
                        apply_watermark(&mut img, mark);
                    }
                    write_tiff(&img, input, &mut out, opts).with_context(unwritable)?;
                }
                OutputFormat::Original => unreachable!("handled by copy_original"),
                OutputFormat::Png => {
                    let mut img = source.rgb16(opts)?;
                    if let Some(mark) = watermark_for(opts) {
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

/// Convert `input` to several sizes, developing it only once: at the
/// largest size, without sharpening, from which each output is resized,
/// sharpened, and marked. One result per output, in order.
pub fn convert_raw_sizes(
    input: &Path,
    outputs: &[(PathBuf, Option<u32>)],
    opts: &ConvertOptions,
) -> Vec<Result<()>> {
    let all_fail = |e: anyhow::Error| outputs.iter().map(|_| Err(anyhow!("{e:#}"))).collect();
    let largest = if outputs.iter().any(|(_, size)| size.is_none()) {
        None
    } else {
        outputs.iter().filter_map(|(_, size)| *size).max()
    };
    let base = ConvertOptions { resize: largest, sharpen: None, ..opts.clone() };
    let mut source = match prepare_source(input, &base) {
        Ok(Some(source)) => source,
        Ok(None) => {
            return convert_raw_sizes(input, outputs, &developed_instead_of_preview(input, opts))
        }
        Err(e) => return all_fail(e),
    };
    let sized = |size: Option<u32>| ConvertOptions { resize: size, ..opts.clone() };
    match opts.format {
        OutputFormat::Jpeg => {
            let full = match source.rgb8(&base) {
                Ok(img) => img,
                Err(e) => return all_fail(e),
            };
            outputs
                .iter()
                .map(|(output, size)| {
                    guarded(|| {
                        let mut img = maybe_resize_8bit(full.clone(), *size);
                        finish_image(&mut img, &sized(*size));
                        let bytes = encode_jpeg(&img, input, &sized(*size))?;
                        write_atomically(output, |out| Ok(out.write_all(&bytes)?))
                    })
                })
                .collect()
        }
        OutputFormat::Tiff | OutputFormat::Png => {
            // Resize each size in linear light, as a single size is.
            let (full, linear) = match source.rgb16_unencoded(&base) {
                Ok(img) => img,
                Err(e) => return all_fail(e),
            };
            outputs
                .iter()
                .map(|(output, size)| {
                    guarded(|| {
                        let mut img = maybe_resize_16bit(full.clone(), *size);
                        if linear {
                            encode_srgb(&mut img);
                        }
                        finish_image(&mut img, &sized(*size));
                        let opts = sized(*size);
                        write_atomically(output, |out| match opts.format {
                            OutputFormat::Tiff => write_tiff(&img, input, out, &opts),
                            _ => write_png(&img, input, out, &opts),
                        })
                    })
                })
                .collect()
        }
        OutputFormat::Original => all_fail(anyhow!("-f original copies files; it has no sizes")),
    }
}

/// Run the work for one output, turning a panic into an error of that
/// output alone, so outputs already written are still reported.
fn guarded(work: impl FnOnce() -> Result<()>) -> Result<()> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
        .unwrap_or_else(|_| Err(anyhow!("encoding panicked")))
}

/// The watermark for an output of `opts.resize`, if it gets one.
fn watermark_for(opts: &ConvertOptions) -> Option<&image::RgbaImage> {
    let sized = opts.watermark_sizes.is_empty()
        || opts.resize.is_some_and(|r| opts.watermark_sizes.contains(&r));
    opts.watermark.as_deref().filter(|_| sized)
}

/// Output sharpening and the watermark, after the resize.
fn finish_image<S>(img: &mut ImageBuffer<Rgb<S>, Vec<S>>, opts: &ConvertOptions)
where
    S: Sample,
    Rgb<S>: image::Pixel<Subpixel = S> + 'static,
{
    if let Some(amount) = opts.sharpen {
        sharpen(img, amount);
    }
    if let Some(mark) = watermark_for(opts) {
        apply_watermark(img, mark);
    }
}

/// Write `output` through a temporary file that is renamed when complete,
/// so a failed or interrupted write never leaves a partial file.
fn write_atomically(
    output: &Path,
    write: impl FnOnce(&mut std::io::BufWriter<std::fs::File>) -> Result<()>,
) -> Result<()> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create output directory {}", parent.display()))?;
    }
    let tmp = temp_path_for(output);
    let _guard = TempFileGuard::register(&tmp);
    let unwritable = || format!("Failed to write output file {}", output.display());
    let result = std::fs::File::create(&tmp).with_context(unwritable).and_then(|file| {
        let mut out = std::io::BufWriter::new(file);
        write(&mut out).with_context(unwritable)?;
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
    fn rgb8(&mut self, opts: &ConvertOptions) -> Result<RgbImage> {
        match self {
            Source::Developed(pipeline) => render_8bit(pipeline, opts),
            Source::Preview(img) => {
                let img = crop_to_aspect(std::mem::take(img), opts.aspect);
                let mut img = maybe_resize_8bit(img, opts.resize);
                if let Some(amount) = opts.sharpen {
                    sharpen(&mut img, amount);
                }
                Ok(img)
            }
        }
    }

    /// The final 16-bit image: resized and sharpened.
    fn rgb16(&mut self, opts: &ConvertOptions) -> Result<ImageBuffer<Rgb<u16>, Vec<u16>>> {
        match self {
            Source::Developed(pipeline) => render_16bit_sharpened(pipeline, opts),
            Source::Preview(img) => {
                let wide = image::DynamicImage::ImageRgb8(std::mem::take(img)).into_rgb16();
                let mut img = maybe_resize_16bit(crop_to_aspect(wide, opts.aspect), opts.resize);
                if let Some(amount) = opts.sharpen {
                    sharpen(&mut img, amount);
                }
                Ok(img)
            }
        }
    }

    /// The 16-bit image before the sRGB curve and sharpening, and whether
    /// it is linear, so that smaller sizes can be made from it in linear
    /// light.
    fn rgb16_unencoded(
        &mut self,
        opts: &ConvertOptions,
    ) -> Result<(Rgb16Image, bool)> {
        match self {
            Source::Developed(pipeline) => {
                render_16bit_linear(pipeline, opts.resize, opts.fast, opts.aspect)
            }
            Source::Preview(img) => {
                let wide = image::DynamicImage::ImageRgb8(std::mem::take(img)).into_rgb16();
                Ok((maybe_resize_16bit(crop_to_aspect(wide, opts.aspect), opts.resize), false))
            }
        }
    }
}

/// Load the JPEG preview that the camera stored in a RAW file, rotated
/// upright. Its size depends on the camera: about 1616x1080 for Sony, full
/// resolution for many Canon and Nikon bodies.
fn load_preview(input: &Path) -> Result<Option<RgbImage>> {
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
    // The caller names the file in its message.
    match std::panic::catch_unwind(load) {
        Ok(Ok(preview)) => Ok(preview),
        Ok(Err(e)) => Err(anyhow!("cannot read the embedded preview: {}", first_line(e))),
        Err(_) => Err(anyhow!("cannot read the embedded preview")),
    }
}

/// The pixel size of a RAW file's image, from the start of the file and
/// without decoding the image data. `None` when the start is not enough.
pub fn raw_dimensions(input: &Path) -> Option<(u32, u32)> {
    // Most cameras record the image size in EXIF, which is read from the
    // start of the file.
    if let Ok(Some((fields, _))) = exif_utils::read_source_fields(input) {
        let get = |tag| {
            fields
                .iter()
                .find(|f| f.tag == tag && f.ifd_num == exif::In::PRIMARY)
                .and_then(|f| f.value.get_uint(0))
        };
        if let (Some(w), Some(h)) = (get(exif::Tag::PixelXDimension), get(exif::Tag::PixelYDimension)) {
            if w > 160 && h > 120 {
                return Some((w, h));
            }
        }
    }
    let source = exif_utils::head_source(input)?;
    std::panic::catch_unwind(move || {
        let decoder = rawler::get_decoder(&source).ok()?;
        let image = decoder
            .raw_image(&source, &rawler::decoders::RawDecodeParams::default(), true)
            .ok()?;
        Some((image.width as u32, image.height as u32))
    })
    .ok()
    .flatten()
    .filter(|(w, h)| *w > 0 && *h > 0)
}

/// How sharp a frame is, for `--burst-pick sharpest`: the variance of the
/// Laplacian of the embedded preview, scaled to 800 pixels, on the middle
/// half where the subject usually is. Higher is sharper. `None` when the
/// file cannot be read.
pub fn sharpness(input: &Path) -> Option<f64> {
    // A file without a readable preview, such as a JPEG or PNG input, is
    // measured on its own pixels.
    let preview = load_preview(input)
        .ok()
        .flatten()
        .or_else(|| image::open(input).ok().map(|img| img.into_rgb8()))?;
    // DynamicImage::thumbnail keeps the aspect ratio.
    let grey = image::DynamicImage::ImageRgb8(preview).thumbnail(800, 800).into_luma8();
    Some(laplacian_variance(&grey))
}

/// Variance of the 4-neighbour Laplacian over the middle half of `img`.
fn laplacian_variance(img: &image::GrayImage) -> f64 {
    let (w, h) = img.dimensions();
    let (x0, x1, y0, y1) = (w / 4, w * 3 / 4, h / 4, h * 3 / 4);
    let mut values = Vec::new();
    for y in y0.max(1)..y1.min(h.saturating_sub(1)) {
        for x in x0.max(1)..x1.min(w.saturating_sub(1)) {
            let p = |dx: i64, dy: i64| f64::from(img.get_pixel((x as i64 + dx) as u32, (y as i64 + dy) as u32)[0]);
            values.push(p(1, 0) + p(-1, 0) + p(0, 1) + p(0, -1) - 4.0 * p(0, 0));
        }
    }
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64
}

/// Warn once per run when `--resize` asks for more pixels than a preview
/// has, because a preview is never enlarged.
fn warn_if_preview_is_small(preview: &RgbImage, resize: Option<u32>) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let long_edge = preview.width().max(preview.height());
    if resize.is_some_and(|r| r > long_edge)
        && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        eprintln!(
            "Note: embedded previews are smaller than --resize (for example {long_edge} pixels \
             on the long edge) and are not enlarged; convert without --preview for larger images"
        );
    }
}

/// The first line of a decoder error, without the library's wrapping:
/// `RawLoaderError: "Couldn't find …` loses its prefix and its unbalanced
/// quote, and rawler's `Error: ` prefix goes too.
fn decoder_detail(error: &str) -> String {
    let line = error.lines().next().unwrap_or_default().trim();
    let line = line.strip_prefix("RawLoaderError: ").unwrap_or(line);
    let line = line.strip_prefix("Error: ").unwrap_or(line);
    line.trim_matches('"').trim_end_matches('.').trim().to_string()
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
                    anyhow!(
                        "the file is corrupt or its format is not supported ({}; {})",
                        decoder_detail(&new_decoder),
                        decoder_detail(&old_decoder.to_string())
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
            "the decoder returned invalid data ({problem}); this format variant is not supported"
        ));
    }
    let mut pipeline = Pipeline::new_from_source(ImageSource::Raw(raw))
        .map_err(|e| anyhow!("cannot set up processing: {e}"))?;
    let tolab = &mut pipeline.ops.tolab;
    tolab.cam_to_xyz_normalized = cam_to_xyz_d65(tolab.xyz_to_cam);
    Ok(pipeline)
}

/// Camera-to-XYZ matrix that maps camera white (1, 1, 1) to the D65 white
/// point.
///
/// rawloader normalises the matrix so that camera white maps to XYZ
/// (1, 1, 1). imagepipe then converts as if XYZ were relative to D65, where
/// (1, 1, 1) is pink, so neutral greys came out with about 10% too much red.
fn cam_to_xyz_d65(mut xyz_to_cam: [[f32; 3]; 4]) -> [[f32; 4]; 3] {
    const D65_WHITE: [f32; 3] = [0.95047, 1.0, 1.08883];
    for row in xyz_to_cam.iter_mut() {
        let sum: f32 = row.iter().zip(D65_WHITE).map(|(v, w)| v * w).sum();
        if sum != 0.0 {
            row.iter_mut().for_each(|v| *v /= sum);
        }
    }
    rawloader::RawImage::pseudoinverse(xyz_to_cam)
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
        let (same, hash) = compare_copy(input, &tmp, manifest::enabled())?;
        if !same {
            return Err(anyhow!("the copy differs from the original; the disk may be failing"));
        }
        std::fs::rename(&tmp, output).with_context(unwritable)?;
        sync_parent(output);
        manifest::record(output, hash);
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    copy_sidecars(input, output)?;
    if with_jpeg {
        // The RAW file was just copied, so a camera JPEG already at the
        // destination may belong to another image ({seq} renumbers).
        copy_companions(input, output, true)?;
    }
    Ok(())
}

/// For `--manifest`: an archived copy that an earlier run made, and that is
/// not yet in its folder's manifest, is compared with the original and
/// added. A copy that differs is an error.
pub fn add_existing_to_manifest(input: &Path, output: &Path) -> Result<()> {
    if !manifest::enabled() || manifest::listed(output) {
        return Ok(());
    }
    let (same, hash) = compare_copy(input, output, true)?;
    if !same {
        bail!(
            "the archived copy {} differs from the original; replace it with --overwrite",
            output.display()
        );
    }
    manifest::record(output, hash);
    Ok(())
}

/// Copy the JPEG or HEIF files that a camera set to RAW+JPEG wrote next to
/// `input` with the same name, such as `DSC1.JPG` for `DSC1.ARW`. Each copy
/// keeps its modification time and is compared with the original. With
/// `replace`, an existing file at the destination is replaced; otherwise
/// it is kept.
pub fn copy_companions(input: &Path, output: &Path, replace: bool) -> Result<()> {
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
        if destination.exists() && (!replace || same_contents(&source, &destination)?) {
            continue;
        }
        copy_verified(&source, &destination, true).map_err(|e| {
            anyhow!("copied the RAW file but not {}: {e:#}", source.display())
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
        let copy = std::fs::File::options().write(true).open(&tmp)?;
        if keep_time {
            if let Ok(modified) = std::fs::metadata(source).and_then(|m| m.modified()) {
                copy.set_modified(modified)?;
            }
        }
        // On disk before it is called verified: the card may be formatted next.
        copy.sync_all()?;
        drop(copy);
        let (same, hash) = compare_copy(source, &tmp, manifest::enabled())?;
        if !same {
            return Err(anyhow!("the copy differs from the original"));
        }
        std::fs::rename(&tmp, destination)?;
        sync_parent(destination);
        manifest::record(destination, hash);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Flush the directory entry of a renamed copy, so that the new name is on
/// disk too, not only the data. Best effort: some file systems refuse it.
fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(dir) = std::fs::File::open(dir) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
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
            return Err(anyhow!("copied the RAW file but not its sidecar {}: {e:#}", source.display()));
        }
    }
    Ok(())
}

/// True if the two files have the same bytes.
fn same_contents(a: &Path, b: &Path) -> Result<bool> {
    Ok(compare_copy(a, b, false)?.0)
}

/// Compare `copy` with `original` byte for byte. With `hash`, also return
/// the SHA-256 of the copy as hex, computed in the same pass.
fn compare_copy(original: &Path, copy: &Path, hash: bool) -> Result<(bool, Option<String>)> {
    use sha2::Digest;
    use std::io::Read;
    let open = |p: &Path| {
        std::fs::File::open(p).with_context(|| format!("Failed to read {}", p.display()))
    };
    let (mut fa, mut fb) = (open(original)?, open(copy)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok((false, None));
    }
    let mut hasher = hash.then(sha2::Sha256::new);
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            let hex = hasher.map(|h| h.finalize().iter().map(|b| format!("{b:02x}")).collect());
            return Ok((true, hex));
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok((false, None));
        }
        if let Some(h) = hasher.as_mut() {
            h.update(&bb[..n]);
        }
    }
}

/// `--manifest`: the SHA-256 of every verified copy of a run, written to a
/// `SHA256SUMS` file in each destination folder at the end.
pub mod manifest {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    static ENABLED: AtomicBool = AtomicBool::new(false);
    static ENTRIES: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());

    pub const FILE_NAME: &str = "SHA256SUMS";

    pub fn enable() {
        ENABLED.store(true, Ordering::Relaxed);
    }

    pub fn enabled() -> bool {
        ENABLED.load(Ordering::Relaxed)
    }

    /// True if `path` has a line in its folder's `SHA256SUMS`.
    pub fn listed(path: &Path) -> bool {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        let name = name.to_string_lossy();
        parse(&std::fs::read_to_string(dir.join(FILE_NAME)).unwrap_or_default())
            .any(|(listed, _)| listed == name)
    }

    /// (name, hash) of each line, in text (`hash  name`) or binary
    /// (`hash *name`) form.
    fn parse(text: &str) -> impl Iterator<Item = (&str, &str)> {
        text.lines().filter_map(|line| {
            let (hash, rest) = line.split_once(' ')?;
            let name = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('*'))?;
            Some((name, hash))
        })
    }

    pub(super) fn record(path: &Path, hash: Option<String>) {
        if let Some(hash) = hash {
            ENTRIES.lock().unwrap_or_else(|e| e.into_inner()).push((path.to_path_buf(), hash));
        }
    }

    /// Merge the recorded hashes into each folder's `SHA256SUMS`, in the
    /// format that `shasum -a 256 -c` reads. A file copied again replaces
    /// its line. Returns the number of files written.
    pub fn write() -> std::io::Result<usize> {
        let entries = std::mem::take(&mut *ENTRIES.lock().unwrap_or_else(|e| e.into_inner()));
        let mut by_dir: BTreeMap<PathBuf, Vec<(String, String)>> = BTreeMap::new();
        for (path, hash) in entries {
            let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            by_dir.entry(dir).or_default().push((name, hash));
        }
        for (dir, new) in &by_dir {
            let file = dir.join(FILE_NAME);
            let existing = std::fs::read_to_string(&file).unwrap_or_default();
            let mut lines: BTreeMap<String, String> = parse(&existing)
                .map(|(name, hash)| (name.to_string(), hash.to_string()))
                .collect();
            for (name, hash) in new {
                lines.insert(name.clone(), hash.clone());
            }
            let text: String = lines.iter().map(|(name, hash)| format!("{hash}  {name}\n")).collect();
            let tmp = dir.join(format!(".{FILE_NAME}.rawdog-tmp"));
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, &file)?;
        }
        Ok(by_dir.len())
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn write_merges_into_the_existing_manifest() {
            let tmp = tempfile::TempDir::new().unwrap();
            let file = tmp.path().join(super::FILE_NAME);
            std::fs::write(&file, "aaaa *A.ARW\nbbbb  B.ARW\n").unwrap();
            assert!(super::listed(&tmp.path().join("A.ARW")));
            assert!(!super::listed(&tmp.path().join("C.JPG")));
            super::record(&tmp.path().join("B.ARW"), Some("cccc".to_string()));
            super::record(&tmp.path().join("C.JPG"), Some("dddd".to_string()));
            super::record(&tmp.path().join("D.ARW"), None);
            assert_eq!(super::write().unwrap(), 1);
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "aaaa  A.ARW\ncccc  B.ARW\ndddd  C.JPG\n"
            );
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

fn render_8bit(pipeline: &mut Pipeline, opts: &ConvertOptions) -> Result<RgbImage> {
    let decoded = pipeline
        .output_8bit(None)
        .map_err(|e| anyhow!("processing failed: {e}"))?;

    let width = decoded.width as u32;
    let height = decoded.height as u32;

    let img: RgbImage = ImageBuffer::from_raw(width, height, decoded.data)
        .context("Failed to construct image buffer from decoded data")?;

    let decode_edge = fast_decode_edge(opts.resize, opts.aspect);
    let img = crop_to_aspect(trim_fast_edge(img, decode_edge, opts.fast), opts.aspect);
    let mut img = maybe_resize_8bit(img, opts.resize);
    if let Some(amount) = opts.sharpen {
        sharpen(&mut img, amount);
    }
    Ok(img)
}

fn render_16bit_sharpened(pipeline: &mut Pipeline, opts: &ConvertOptions) -> Result<ImageBuffer<Rgb<u16>, Vec<u16>>> {
    let mut img = render_16bit(pipeline, opts.resize, opts.fast, opts.aspect)?;
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
    let xmp = rating_packet(input, opts);
    let mut encode = |quality: u8| -> Result<Vec<u8>> {
        let mut jpeg_buf = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut jpeg_buf, quality);
        if let Some(dpi) = opts.metadata.dpi {
            encoder.set_pixel_density(image::codecs::jpeg::PixelDensity::dpi(dpi));
        }
        if opts.metadata.srgb {
            encoder.set_icc_profile(exif_utils::srgb_icc_profile().to_vec())?;
        }
        img.write_with_encoder(encoder)
            .context("Failed to encode JPEG")?;
        if let Some(exif) = &exif_data {
            match exif_utils::inject_exif_into_jpeg(&jpeg_buf, exif) {
                Ok(with_exif) => jpeg_buf = with_exif,
                Err(e) => {
                    eprintln!(
                        "Warning: metadata of {} was not copied: {e}",
                        input.display()
                    );
                    exif_data = None;
                }
            }
        }
        if let Some(xmp) = &xmp {
            jpeg_buf = exif_utils::inject_xmp_into_jpeg(&jpeg_buf, xmp)?;
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
pub fn format_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} bytes"),
        1_000..1_000_000 => format!("{:.0} KB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

/// The XMP packet for `--keep-rating` (the file's rating and label) and
/// `--keywords`, or `None` without them.
fn rating_packet(input: &Path, opts: &ConvertOptions) -> Option<String> {
    let meta = &opts.metadata;
    let (rating, label) = if meta.keep_rating {
        let info = crate::naming::NameInfo::read(input, true);
        (info.rating, info.labels.into_iter().next())
    } else {
        (None, None)
    };
    exif_utils::rating_xmp(rating, label.as_deref(), &meta.keywords)
}

fn write_tiff(
    img: &ImageBuffer<Rgb<u16>, Vec<u16>>,
    input: &Path,
    out: &mut (impl Write + std::io::Seek),
    opts: &ConvertOptions,
) -> Result<()> {
    // EXIF failures are non-fatal: fall back to a TIFF without metadata.
    let mut fields = exif_utils::read_fields_for_output(input, img.dimensions(), &opts.metadata)
        .ok()
        .flatten();
    if let Some(xmp) = rating_packet(input, opts) {
        // TIFF keeps XMP in tag 700.
        let field = exif::Field {
            tag: exif::Tag(exif::Context::Tiff, 0x2bc),
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Byte(xmp.into_bytes()),
        };
        match &mut fields {
            Some((fields, _)) => fields.push(field),
            None => fields = Some((vec![field], false)),
        }
    }
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
    let mut final_bytes = match exif_data {
        Some(exif) => exif_utils::inject_exif_into_png(&buf, &exif).unwrap_or(buf),
        None => buf,
    };
    if let Some(xmp) = rating_packet(input, opts) {
        final_bytes = exif_utils::inject_xmp_into_png(&final_bytes, &xmp)?;
    }

    out.write_all(&final_bytes)?;
    Ok(())
}

fn render_16bit(
    pipeline: &mut Pipeline,
    resize: Option<u32>,
    fast: bool,
    aspect: Option<(u32, u32)>,
) -> Result<ImageBuffer<Rgb<u16>, Vec<u16>>> {
    // Resize in linear light, then encode.
    let (mut img, is_linear) = render_16bit_linear(pipeline, resize, fast, aspect)?;
    if is_linear {
        encode_srgb(&mut img);
    }
    Ok(img)
}

/// A 16-bit RGB image.
type Rgb16Image = ImageBuffer<Rgb<u16>, Vec<u16>>;

/// Apply the sRGB transfer curve to linear 16-bit samples.
fn encode_srgb(img: &mut ImageBuffer<Rgb<u16>, Vec<u16>>) {
    let lut = srgb_encode_lut();
    for v in img.iter_mut() {
        *v = lut[*v as usize];
    }
}

/// The 16-bit image, cropped and resized, before the sRGB curve, and
/// whether its samples are linear (RAW data) or already encoded.
fn render_16bit_linear(
    pipeline: &mut Pipeline,
    resize: Option<u32>,
    fast: bool,
    aspect: Option<(u32, u32)>,
) -> Result<(Rgb16Image, bool)> {
    // imagepipe's full pipeline (always used for RAW files) returns linear
    // 16-bit data, while its raster fast path returns the source file's
    // already-encoded samples. Mirror the fast-path condition in output_16bit.
    let is_linear = !(matches!(pipeline.globals.image, ImageSource::Other(_))
        && pipeline.globals.settings.use_fastpath
        && pipeline.default_ops());

    let decoded = pipeline
        .output_16bit(None)
        .map_err(|e| anyhow!("processing failed: {e}"))?;

    let width = decoded.width as u32;
    let height = decoded.height as u32;

    let img: ImageBuffer<Rgb<u16>, Vec<u16>> =
        ImageBuffer::from_raw(width, height, decoded.data)
            .context("Failed to construct 16-bit image buffer from decoded data")?;

    let trimmed = trim_fast_edge(img, fast_decode_edge(resize, aspect), fast);
    Ok((maybe_resize_16bit(crop_to_aspect(trimmed, aspect), resize), is_linear))
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

/// The long edge that `fast` asks the decoder for. A crop to another aspect
/// ratio removes up to half of the long edge of a 2:1 image, so the decoder
/// gives twice the size and the crop still fills `resize`.
fn fast_decode_edge(resize: Option<u32>, aspect: Option<(u32, u32)>) -> Option<u32> {
    resize.map(|edge| if aspect.is_some() { edge.saturating_mul(2) } else { edge })
}

/// Crop the centre of `img` to `aspect` (long side, short side), in the
/// image's own orientation: 4:5 makes a portrait 4:5 and a landscape 5:4.
fn crop_to_aspect<P>(
    img: ImageBuffer<P, Vec<P::Subpixel>>,
    aspect: Option<(u32, u32)>,
) -> ImageBuffer<P, Vec<P::Subpixel>>
where
    P: image::Pixel + 'static,
{
    let Some((long_ratio, short_ratio)) = aspect else {
        return img;
    };
    let (width, height) = img.dimensions();
    let (long, short) = (width.max(height) as u64, width.min(height) as u64);
    let (long_ratio, short_ratio) = (long_ratio as u64, short_ratio as u64);
    // Keep the short side and shorten the long side, or the other way round.
    let (new_long, new_short) = if long * short_ratio > short * long_ratio {
        ((short * long_ratio + short_ratio / 2) / short_ratio, short)
    } else {
        (long, (long * short_ratio + long_ratio / 2) / long_ratio)
    };
    let (new_long, new_short) = (new_long.max(1) as u32, new_short.max(1) as u32);
    let (new_w, new_h) = if width >= height {
        (new_long, new_short)
    } else {
        (new_short, new_long)
    };
    if (new_w, new_h) == (width, height) {
        return img;
    }
    let (x, y) = ((width - new_w) / 2, (height - new_h) / 2);
    image::imageops::crop_imm(&img, x, y, new_w, new_h).to_image()
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

    #[test]
    fn crop_to_aspect_follows_the_orientation() {
        let landscape = RgbImage::new(6000, 4000);
        assert_eq!(crop_to_aspect(landscape.clone(), Some((5, 4))).dimensions(), (5000, 4000));
        assert_eq!(crop_to_aspect(landscape.clone(), Some((16, 9))).dimensions(), (6000, 3375));
        assert_eq!(crop_to_aspect(landscape.clone(), Some((1, 1))).dimensions(), (4000, 4000));
        assert_eq!(crop_to_aspect(landscape.clone(), Some((3, 2))).dimensions(), (6000, 4000));
        assert_eq!(crop_to_aspect(landscape, None).dimensions(), (6000, 4000));
        let portrait = RgbImage::new(4000, 6000);
        assert_eq!(crop_to_aspect(portrait, Some((5, 4))).dimensions(), (4000, 5000));
    }

    #[test]
    fn crop_to_aspect_keeps_the_centre() {
        let img = RgbImage::from_fn(30, 10, |x, _| image::Rgb([x as u8, 0, 0]));
        let out = crop_to_aspect(img, Some((1, 1)));
        assert_eq!(out.dimensions(), (10, 10));
        assert_eq!(out.get_pixel(0, 0)[0], 10);
    }

    #[test]
    fn laplacian_variance_is_higher_for_sharp_detail() {
        let sharp = image::GrayImage::from_fn(64, 64, |x, y| image::Luma([if (x + y) % 2 == 0 { 0 } else { 255 }]));
        let blurred = image::imageops::blur(&sharp, 3.0);
        let flat = image::GrayImage::from_pixel(64, 64, image::Luma([128]));
        assert!(laplacian_variance(&sharp) > laplacian_variance(&blurred));
        assert!(laplacian_variance(&blurred) >= laplacian_variance(&flat));
        assert_eq!(laplacian_variance(&flat), 0.0);
        assert_eq!(laplacian_variance(&image::GrayImage::new(2, 2)), 0.0);
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
        let out16 = render_16bit(&mut p16, None, false, None).unwrap();

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
        let out16 = render_16bit(&mut p16, None, false, None).unwrap();
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
        // A JPEG of another image at the destination is replaced.
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("2024_DSC1.JPG"), b"another image").unwrap();
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

        // Without `replace`, a missing companion is added and an existing
        // one is kept.
        std::fs::remove_file(out.join("2024_DSC1.HIF")).unwrap();
        std::fs::write(&copied, b"kept").unwrap();
        copy_companions(&input, &out.join("2024_DSC1.ARW"), false).unwrap();
        assert_eq!(std::fs::read(out.join("2024_DSC1.HIF")).unwrap(), b"heif");
        assert_eq!(std::fs::read(&copied).unwrap(), b"kept");
    }

    #[test]
    fn compare_copy_hashes_the_copy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::write(&a, b"abc").unwrap();
        std::fs::write(&b, b"abc").unwrap();
        let (same, hash) = compare_copy(&a, &b, true).unwrap();
        assert!(same);
        assert_eq!(
            hash.as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(compare_copy(&a, &b, false).unwrap(), (true, None));
    }

    #[test]
    fn decoder_detail_removes_library_wrapping() {
        assert_eq!(
            decoder_detail("RawLoaderError: \"Couldn't find a decoder for this file.\nmore"),
            "Couldn't find a decoder for this file"
        );
        assert_eq!(
            decoder_detail("Error: No decoder found, model '', make: '', mode: ''"),
            "No decoder found, model '', make: '', mode: ''"
        );
        assert_eq!(
            decoder_detail("RawLoaderError: \"Couldn't find camera \"NIKON\" \"Z f\" mode \"14bit\".\""),
            "Couldn't find camera \"NIKON\" \"Z f\" mode \"14bit\""
        );
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

    #[test]
    fn cam_to_xyz_d65_maps_camera_white_to_the_d65_white_point() {
        // An sRGB-like camera: XYZ to linear sRGB, padded to four rows.
        let m = [
            [3.2406, -1.5372, -0.4986],
            [-0.9689, 1.8758, 0.0415],
            [0.0557, -0.2040, 1.0570],
            [0.0, 0.0, 0.0],
        ];
        let cam_to_xyz = cam_to_xyz_d65(m);
        for (i, expected) in [0.95047f32, 1.0, 1.08883].iter().enumerate() {
            let xyz: f32 = cam_to_xyz[i][..3].iter().sum();
            assert!((xyz - expected).abs() < 1e-3, "row {i}: {xyz}");
        }
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
            aspect: None,
            fast: false,
            exposure: None,
            white_balance: None,
            wb_from: None,
            sharpen: None,
            preview: true,
            watermark: None,
            watermark_sizes: Vec::new(),
            with_jpeg: false,
            metadata: Default::default(),
        }
    }

    #[test]
    fn preview_source_is_resized_for_8_bit_output() {
        let mut source = Source::Preview(make_8bit_image(400, 200));
        let img = source.rgb8(&preview_opts(Some(100))).unwrap();
        assert_eq!(img.dimensions(), (100, 50));
    }

    #[test]
    fn preview_source_is_widened_for_16_bit_output() {
        let mut source = Source::Preview(ImageBuffer::from_pixel(8, 4, Rgb([0u8, 128, 255])));
        let img = source.rgb16(&preview_opts(None)).unwrap();
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
                aspect: None,
                fast: false,
                exposure: None,
                white_balance: None,
                wb_from: None,
                sharpen: None,
                preview: false,
                watermark: None,
                watermark_sizes: Vec::new(),
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
                aspect: None,
                fast: false,
                exposure: None,
                white_balance: None,
                wb_from: None,
                sharpen: None,
                preview: false,
                watermark: None,
                watermark_sizes: Vec::new(),
                with_jpeg: false,
                metadata: Default::default(),
            },
        );
        assert!(result.is_err());
    }
}
