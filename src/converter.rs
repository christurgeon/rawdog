use std::path::Path;

use anyhow::{anyhow, Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::tiff::TiffEncoder;
use image::{ImageBuffer, Rgb, RgbImage};
use imagepipe::{ImageSource, Pipeline};

use crate::exif as exif_utils;
use crate::OutputFormat;

/// Convert a single ARW file to the specified output format.
pub fn convert_arw(
    input: &Path,
    output: &Path,
    format: OutputFormat,
    quality: u8,
    resize: Option<u32>,
) -> Result<()> {
    let mut pipeline = Pipeline::new_from_file(input)
        .map_err(|e| anyhow!("Failed to create pipeline for {}: {}", input.display(), e))?;

    // Extract EXIF metadata from the source ARW before conversion.
    // Failures here are non-fatal: we still produce the output image.
    let exif_data = exif_utils::extract_exif_from_arw(input).ok().flatten();

    // Ensure output directory exists
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create output directory {}", parent.display()))?;
    }

    match format {
        OutputFormat::Jpeg => {
            encode_jpeg(&mut pipeline, input, output, quality, resize, exif_data.as_deref())
        }
        OutputFormat::Tiff => encode_16bit(&mut pipeline, input, output, resize, Format16::Tiff),
        OutputFormat::Png => {
            encode_16bit_with_exif(&mut pipeline, input, output, resize, Format16::Png, exif_data.as_deref())
        }
    }
}

fn encode_jpeg(
    pipeline: &mut Pipeline,
    input: &Path,
    output: &Path,
    quality: u8,
    resize: Option<u32>,
    exif_data: Option<&[u8]>,
) -> Result<()> {
    let decoded = pipeline
        .output_8bit(None)
        .map_err(|e| anyhow!("Failed to process {}: {}", input.display(), e))?;

    let width = decoded.width as u32;
    let height = decoded.height as u32;

    let img: RgbImage = ImageBuffer::from_raw(width, height, decoded.data)
        .context("Failed to construct image buffer from decoded data")?;

    let final_img = maybe_resize_8bit(img, resize);

    // Encode JPEG to an in-memory buffer so we can inject EXIF metadata
    // before writing to disk.
    let mut jpeg_buf = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut jpeg_buf, quality);
    final_img
        .write_with_encoder(encoder)
        .with_context(|| format!("Failed to encode JPEG to {}", output.display()))?;

    // Inject EXIF metadata into the JPEG if available.
    let final_bytes = match exif_data {
        Some(exif) => exif_utils::inject_exif_into_jpeg(&jpeg_buf, exif)
            .unwrap_or(jpeg_buf),
        None => jpeg_buf,
    };

    std::fs::write(output, &final_bytes)
        .with_context(|| format!("Failed to write output file {}", output.display()))?;

    Ok(())
}

enum Format16 {
    Tiff,
    Png,
}

fn encode_16bit(
    pipeline: &mut Pipeline,
    input: &Path,
    output: &Path,
    resize: Option<u32>,
    fmt: Format16,
) -> Result<()> {
    let final_img = render_16bit(pipeline, input, resize)?;

    let mut out_file = std::fs::File::create(output)
        .with_context(|| format!("Failed to create output file {}", output.display()))?;

    let label = match fmt {
        Format16::Tiff => "TIFF",
        Format16::Png => "PNG",
    };

    match fmt {
        Format16::Tiff => {
            let encoder = TiffEncoder::new(&mut out_file);
            final_img
                .write_with_encoder(encoder)
                .with_context(|| format!("Failed to encode {} to {}", label, output.display()))?;
        }
        Format16::Png => {
            let encoder = PngEncoder::new(&mut out_file);
            final_img
                .write_with_encoder(encoder)
                .with_context(|| format!("Failed to encode {} to {}", label, output.display()))?;
        }
    }

    Ok(())
}

fn encode_16bit_with_exif(
    pipeline: &mut Pipeline,
    input: &Path,
    output: &Path,
    resize: Option<u32>,
    _fmt: Format16,
    exif_data: Option<&[u8]>,
) -> Result<()> {
    let final_img = render_16bit(pipeline, input, resize)?;

    // Encode PNG to an in-memory buffer so we can inject EXIF metadata.
    let mut buf = Vec::new();
    let encoder = PngEncoder::new(&mut buf);
    final_img
        .write_with_encoder(encoder)
        .with_context(|| format!("Failed to encode PNG to {}", output.display()))?;

    // Inject EXIF metadata into the PNG if available.
    let final_bytes = match exif_data {
        Some(exif) => exif_utils::inject_exif_into_png(&buf, exif)
            .unwrap_or(buf),
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
    let mut img = maybe_resize_16bit(img, resize);
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

fn maybe_resize_8bit(
    img: ImageBuffer<Rgb<u8>, Vec<u8>>,
    resize: Option<u32>,
) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
    if let Some(max_edge) = resize {
        let (width, height) = img.dimensions();
        let long_edge = width.max(height);
        if long_edge > max_edge {
            let scale = max_edge as f64 / long_edge as f64;
            let new_w = (width as f64 * scale).round() as u32;
            let new_h = (height as f64 * scale).round() as u32;
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
            let new_w = (width as f64 * scale).round() as u32;
            let new_h = (height as f64 * scale).round() as u32;
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
        let out16 = render_16bit(&mut p16, &png, None).unwrap();

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
        let out16 = render_16bit(&mut p16, &png, None).unwrap();
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

    // -----------------------------------------------------------------------
    // convert_arw error handling tests
    // -----------------------------------------------------------------------

    #[test]
    fn convert_arw_rejects_nonexistent_input() {
        let result = convert_arw(
            Path::new("/nonexistent/photo.arw"),
            Path::new("/tmp/out.jpg"),
            crate::OutputFormat::Jpeg,
            92,
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn convert_arw_rejects_non_arw_file() {
        // Create a temp file that is not a valid ARW
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not a raw file").unwrap();

        let result = convert_arw(
            tmp.path(),
            Path::new("/tmp/out.jpg"),
            crate::OutputFormat::Jpeg,
            92,
            None,
        );
        assert!(result.is_err());
    }
}
