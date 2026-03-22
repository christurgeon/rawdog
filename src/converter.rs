use std::path::Path;

use anyhow::{anyhow, Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::tiff::TiffEncoder;
use image::{ImageBuffer, Rgb, RgbImage};
use imagepipe::Pipeline;

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
    let decoded = pipeline
        .output_16bit(None)
        .map_err(|e| anyhow!("Failed to process {}: {}", input.display(), e))?;

    let width = decoded.width as u32;
    let height = decoded.height as u32;

    let img: ImageBuffer<Rgb<u16>, Vec<u16>> =
        ImageBuffer::from_raw(width, height, decoded.data)
            .context("Failed to construct 16-bit image buffer from decoded data")?;

    let final_img = maybe_resize_16bit(img, resize);

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
    let decoded = pipeline
        .output_16bit(None)
        .map_err(|e| anyhow!("Failed to process {}: {}", input.display(), e))?;

    let width = decoded.width as u32;
    let height = decoded.height as u32;

    let img: ImageBuffer<Rgb<u16>, Vec<u16>> =
        ImageBuffer::from_raw(width, height, decoded.data)
            .context("Failed to construct 16-bit image buffer from decoded data")?;

    let final_img = maybe_resize_16bit(img, resize);

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
