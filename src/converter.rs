use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use image::codecs::jpeg::JpegEncoder;
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
    pub metadata: exif_utils::MetadataOptions,
}

/// Convert a single ARW file to the specified output format.
pub fn convert_raw(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<()> {
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
        if let Some(ev) = exposure {
            let curve = &mut pipeline.ops.basecurve;
            curve.points = exposure_curve(ev, &curve.get_spline());
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
    let unwritable = || format!("Failed to write output file {}", output.display());
    let result = std::fs::File::create(&tmp)
        .with_context(unwritable)
        .and_then(|file| {
            let mut out = std::io::BufWriter::new(file);
            match format {
                OutputFormat::Jpeg => {
                    let img = source.rgb8(input, opts)?;
                    write_jpeg(&img, input, &mut out, opts).with_context(unwritable)?;
                }
                OutputFormat::Tiff => {
                    let img = source.rgb16(input, opts)?;
                    write_tiff(&img, input, &mut out, opts).with_context(unwritable)?;
                }
                OutputFormat::Png => {
                    let img = source.rgb16(input, opts)?;
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
        let orientation = decoder
            .raw_metadata(&source, &params)
            .ok()
            .and_then(|m| m.exif.orientation)
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
    let raw = match decode_with_rawler(input) {
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

fn write_jpeg(
    img: &RgbImage,
    input: &Path,
    out: &mut impl Write,
    opts: &ConvertOptions,
) -> Result<()> {
    // Encode JPEG to an in-memory buffer so we can inject EXIF metadata
    // before writing to disk.
    let mut jpeg_buf = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut jpeg_buf, opts.quality);
    img.write_with_encoder(encoder)
        .context("Failed to encode JPEG")?;

    // Inject EXIF metadata into the JPEG if available. Failures here are
    // non-fatal: we still produce the output image.
    let exif_data = exif_utils::extract_exif(input, img.dimensions(), &opts.metadata)
        .ok()
        .flatten();
    let final_bytes = match exif_data {
        Some(exif) => match exif_utils::inject_exif_into_jpeg(&jpeg_buf, &exif) {
            Ok(with_exif) => with_exif,
            Err(e) => {
                eprintln!(
                    "Warning: metadata of {} was not copied: {e}",
                    input.display()
                );
                jpeg_buf
            }
        },
        None => jpeg_buf,
    };

    out.write_all(&final_bytes)?;
    Ok(())
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
        Some((fields, _)) => {
            exif_utils::write_tiff_16bit_with_exif(out, img.dimensions(), img.as_raw(), &fields)
        }
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
    img.write_with_encoder(PngEncoder::new(&mut buf))
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
            resize,
            fast: false,
            exposure: None,
            white_balance: None,
            sharpen: None,
            preview: true,
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
                resize: None,
                fast: false,
                exposure: None,
                white_balance: None,
                sharpen: None,
                preview: false,
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
                resize: None,
                fast: false,
                exposure: None,
                white_balance: None,
                sharpen: None,
                preview: false,
                metadata: Default::default(),
            },
        );
        assert!(result.is_err());
    }
}
