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
    pub metadata: exif_utils::MetadataOptions,
}

/// Convert a single ARW file to the specified output format.
pub fn convert_raw(input: &Path, output: &Path, opts: &ConvertOptions) -> Result<()> {
    let ConvertOptions {
        format,
        quality,
        resize,
        fast,
        exposure,
        white_balance,
        ref metadata,
    } = *opts;

    let mut pipeline = Pipeline::new_from_file(input)
        .map_err(|e| anyhow!("Failed to create pipeline for {}: {}", input.display(), e))?;

    // With a size limit, imagepipe demosaics and scales in one pass when the
    // reduction is 2x or more. This is several times faster than a full
    // demosaic followed by a Lanczos resize, and slightly softer.
    // One extra pixel is requested because `trim_fast_edge` removes the last
    // row and column.
    if let (true, Some(max_edge)) = (fast, resize) {
        pipeline.globals.settings.maxwidth = max_edge as usize + 1;
        pipeline.globals.settings.maxheight = max_edge as usize + 1;
    }

    if let Some((kelvin, tint)) = white_balance {
        pipeline.ops.tolab.set_temp(kelvin, tint);
    }
    if let Some(ev) = exposure {
        let curve = &mut pipeline.ops.basecurve;
        curve.points = exposure_curve(ev, &curve.get_spline());
    }

    // Ensure output directory exists
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create output directory {}", parent.display()))?;
    }

    // Write to a temporary file and rename it, so an interrupted or failed
    // run never leaves a partial file that a later run would skip.
    let tmp = temp_path_for(output);
    let result = match format {
        OutputFormat::Jpeg => {
            encode_jpeg(&mut pipeline, input, &tmp, quality, resize, fast, metadata)
        }
        OutputFormat::Tiff => encode_tiff(&mut pipeline, input, &tmp, resize, fast, metadata),
        OutputFormat::Png => encode_png(&mut pipeline, input, &tmp, resize, fast, metadata),
    }
    .and_then(|()| {
        std::fs::rename(&tmp, output)
            .with_context(|| format!("Failed to write output file {}", output.display()))
    });

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
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

fn encode_jpeg(
    pipeline: &mut Pipeline,
    input: &Path,
    output: &Path,
    quality: u8,
    resize: Option<u32>,
    fast: bool,
    metadata: &exif_utils::MetadataOptions,
) -> Result<()> {
    let decoded = pipeline
        .output_8bit(None)
        .map_err(|e| anyhow!("Failed to process {}: {}", input.display(), e))?;

    let width = decoded.width as u32;
    let height = decoded.height as u32;

    let img: RgbImage = ImageBuffer::from_raw(width, height, decoded.data)
        .context("Failed to construct image buffer from decoded data")?;

    let final_img = maybe_resize_8bit(trim_fast_edge(img, resize, fast), resize);

    // Encode JPEG to an in-memory buffer so we can inject EXIF metadata
    // before writing to disk.
    let mut jpeg_buf = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut jpeg_buf, quality);
    final_img
        .write_with_encoder(encoder)
        .with_context(|| format!("Failed to encode JPEG to {}", output.display()))?;

    // Inject EXIF metadata into the JPEG if available. Failures here are
    // non-fatal: we still produce the output image.
    let exif_data = exif_utils::extract_exif(input, final_img.dimensions(), metadata)
        .ok()
        .flatten();
    let final_bytes = match exif_data {
        Some(exif) => exif_utils::inject_exif_into_jpeg(&jpeg_buf, &exif).unwrap_or(jpeg_buf),
        None => jpeg_buf,
    };

    std::fs::write(output, &final_bytes)
        .with_context(|| format!("Failed to write output file {}", output.display()))?;

    Ok(())
}

fn encode_tiff(
    pipeline: &mut Pipeline,
    input: &Path,
    output: &Path,
    resize: Option<u32>,
    fast: bool,
    metadata: &exif_utils::MetadataOptions,
) -> Result<()> {
    let final_img = render_16bit(pipeline, input, resize, fast)?;

    let out_file = std::fs::File::create(output)
        .with_context(|| format!("Failed to create output file {}", output.display()))?;
    let mut out_file = std::io::BufWriter::new(out_file);

    // EXIF failures are non-fatal: fall back to a TIFF without metadata.
    let fields = exif_utils::read_fields_for_output(input, final_img.dimensions(), metadata)
        .ok()
        .flatten();
    match fields {
        Some((fields, _)) => exif_utils::write_tiff_16bit_with_exif(
            &mut out_file,
            final_img.dimensions(),
            final_img.as_raw(),
            &fields,
        ),
        None => final_img
            .write_with_encoder(TiffEncoder::new(&mut out_file))
            .map_err(Into::into),
    }
    .with_context(|| format!("Failed to encode TIFF to {}", output.display()))?;

    out_file
        .flush()
        .with_context(|| format!("Failed to write output file {}", output.display()))
}

fn encode_png(
    pipeline: &mut Pipeline,
    input: &Path,
    output: &Path,
    resize: Option<u32>,
    fast: bool,
    metadata: &exif_utils::MetadataOptions,
) -> Result<()> {
    let final_img = render_16bit(pipeline, input, resize, fast)?;

    // Encode PNG to an in-memory buffer so we can inject EXIF metadata.
    let mut buf = Vec::new();
    let encoder = PngEncoder::new(&mut buf);
    final_img
        .write_with_encoder(encoder)
        .with_context(|| format!("Failed to encode PNG to {}", output.display()))?;

    // Inject EXIF metadata into the PNG if available.
    let exif_data = exif_utils::extract_exif(input, final_img.dimensions(), metadata)
        .ok()
        .flatten();
    let final_bytes = match exif_data {
        Some(exif) => exif_utils::inject_exif_into_png(&buf, &exif).unwrap_or(buf),
        None => buf,
    };

    std::fs::write(output, &final_bytes)
        .with_context(|| format!("Failed to write output file {}", output.display()))?;

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

/// Remove the last row and column of an image that imagepipe scaled in
/// `fast` mode. Its scaled demosaic leaves the red channel empty there.
///
/// The pipeline was asked for a long edge of `resize + 1`, so the result has
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
        Some(max_edge) if fast && width.max(height) == max_edge + 1 && width.min(height) > 1 => {
            image::imageops::crop_imm(&img, 0, 0, width - 1, height - 1).to_image()
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
        let img = make_8bit_image(6000, 4000);
        let result = maybe_resize_8bit(img, Some(3000));
        let (w, h) = result.dimensions();
        assert_eq!(w, 3000);
        assert_eq!(h, 2000);
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
        let img = make_16bit_image(6000, 4000);
        let result = maybe_resize_16bit(img, Some(3000));
        let (w, h) = result.dimensions();
        assert_eq!(w, 3000);
        assert_eq!(h, 2000);
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
    fn trim_fast_edge_removes_last_row_and_column_of_scaled_images() {
        let img = ImageBuffer::from_fn(101, 51, |x, y| Rgb([x as u8, y as u8, 0]));
        let out = trim_fast_edge(img, Some(100), true);
        assert_eq!(out.dimensions(), (100, 50));
        assert_eq!(out.get_pixel(99, 49), &Rgb([99u8, 49, 0]));
    }

    #[test]
    fn trim_fast_edge_leaves_other_images_unchanged() {
        // Not fast.
        assert_eq!(trim_fast_edge(make_8bit_image(101, 51), Some(100), false).dimensions(), (101, 51));
        // Smaller than the limit, so the pipeline did not scale it.
        assert_eq!(trim_fast_edge(make_8bit_image(80, 40), Some(100), true).dimensions(), (80, 40));
        // No limit.
        assert_eq!(trim_fast_edge(make_16bit_image(101, 51), None, true).dimensions(), (101, 51));
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
                metadata: Default::default(),
            },
        );
        assert!(result.is_err());
    }
}
