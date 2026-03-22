use std::io::{BufReader, Cursor};
use std::path::Path;

use anyhow::{Context, Result};

/// Extract raw EXIF bytes from a source ARW (TIFF-based) file.
///
/// Returns the EXIF data as a TIFF byte blob suitable for injection into
/// JPEG (via img-parts `set_exif`) or PNG (via eXIf chunk). Returns `None`
/// if no EXIF data could be extracted.
pub fn extract_exif_from_arw(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {} for EXIF extraction", path.display()))?;
    let mut reader = BufReader::new(file);

    let exif = match exif::Reader::new()
        .continue_on_error(true)
        .read_from_container(&mut reader)
    {
        Ok(exif) => exif,
        Err(exif::Error::PartialResult(partial)) => {
            // Accept partial results -- some fields are better than none.
            let (exif, _errors) = partial.into_inner();
            exif
        }
        Err(_) => return Ok(None),
    };

    let fields: Vec<&exif::Field> = exif.fields().collect();
    if fields.is_empty() {
        return Ok(None);
    }

    // Use the experimental Writer to serialize the parsed fields into a
    // minimal TIFF blob containing only the metadata IFDs.
    let mut writer = exif::experimental::Writer::new();
    for field in &fields {
        writer.push_field(field);
    }

    let mut buf = Cursor::new(Vec::new());
    writer
        .write(&mut buf, exif.little_endian())
        .map_err(|e| anyhow::anyhow!("Failed to serialize EXIF data: {}", e))?;

    let data = buf.into_inner();
    if data.is_empty() {
        return Ok(None);
    }

    Ok(Some(data))
}

/// Inject raw EXIF bytes (a TIFF blob) into an already-encoded JPEG buffer.
///
/// Returns the new JPEG bytes with the EXIF data embedded in an APP1 segment.
pub fn inject_exif_into_jpeg(jpeg_bytes: &[u8], exif_data: &[u8]) -> Result<Vec<u8>> {
    use img_parts::jpeg::Jpeg;
    use img_parts::ImageEXIF;

    let mut jpeg = Jpeg::from_bytes(jpeg_bytes.to_vec().into())
        .map_err(|e| anyhow::anyhow!("Failed to parse JPEG for EXIF injection: {}", e))?;

    jpeg.set_exif(Some(img_parts::Bytes::from(exif_data.to_vec())));

    let mut output = Vec::with_capacity(jpeg.len());
    jpeg.encoder()
        .write_to(&mut output)
        .map_err(|e| anyhow::anyhow!("Failed to write JPEG with EXIF: {}", e))?;

    Ok(output)
}

/// Inject raw EXIF bytes (a TIFF blob) into an already-encoded PNG buffer.
///
/// Returns the new PNG bytes with the EXIF data embedded in an eXIf chunk.
pub fn inject_exif_into_png(png_bytes: &[u8], exif_data: &[u8]) -> Result<Vec<u8>> {
    use img_parts::png::Png;
    use img_parts::ImageEXIF;

    let mut png = Png::from_bytes(png_bytes.to_vec().into())
        .map_err(|e| anyhow::anyhow!("Failed to parse PNG for EXIF injection: {}", e))?;

    png.set_exif(Some(img_parts::Bytes::from(exif_data.to_vec())));

    let mut output = Vec::with_capacity(png.len());
    png.encoder()
        .write_to(&mut output)
        .map_err(|e| anyhow::anyhow!("Failed to write PNG with EXIF: {}", e))?;

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_exif_returns_none_for_nonexistent_file() {
        let result = extract_exif_from_arw(Path::new("/nonexistent/photo.arw"));
        assert!(result.is_err());
    }

    #[test]
    fn extract_exif_returns_none_for_non_arw_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not a raw file").unwrap();
        let result = extract_exif_from_arw(tmp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn inject_exif_into_jpeg_roundtrip() {
        // Create a minimal valid JPEG in memory
        let img: image::RgbImage = image::ImageBuffer::from_fn(2, 2, |_, _| {
            image::Rgb([128u8, 64, 32])
        });
        let mut jpeg_buf = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, 90);
        img.write_with_encoder(encoder).unwrap();

        // Create a minimal EXIF blob using the exif writer
        let field = exif::Field {
            tag: exif::Tag::ImageDescription,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![b"RawDog Test".to_vec()]),
        };
        let mut writer = exif::experimental::Writer::new();
        writer.push_field(&field);
        let mut exif_buf = std::io::Cursor::new(Vec::new());
        writer.write(&mut exif_buf, false).unwrap();
        let exif_data = exif_buf.into_inner();

        // Inject EXIF into the JPEG
        let result = inject_exif_into_jpeg(&jpeg_buf, &exif_data).unwrap();
        assert!(result.len() > jpeg_buf.len());

        // Verify the EXIF can be read back
        let parsed = img_parts::jpeg::Jpeg::from_bytes(result.into()).unwrap();
        let extracted = img_parts::ImageEXIF::exif(&parsed);
        assert!(extracted.is_some());

        // Parse the EXIF data and verify the field is present
        let exif_bytes = extracted.unwrap();
        let parsed_exif = exif::Reader::new()
            .read_raw(exif_bytes.to_vec())
            .unwrap();
        let desc = parsed_exif
            .get_field(exif::Tag::ImageDescription, exif::In::PRIMARY)
            .unwrap();
        assert_eq!(
            desc.display_value().to_string(),
            "\"RawDog Test\""
        );
    }

    #[test]
    fn inject_exif_into_png_roundtrip() {
        // Create a minimal valid PNG in memory
        let img: image::RgbImage = image::ImageBuffer::from_fn(2, 2, |_, _| {
            image::Rgb([128u8, 64, 32])
        });
        let mut png_buf = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png_buf);
        img.write_with_encoder(encoder).unwrap();

        // Create a minimal EXIF blob
        let field = exif::Field {
            tag: exif::Tag::ImageDescription,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![b"PNG EXIF Test".to_vec()]),
        };
        let mut writer = exif::experimental::Writer::new();
        writer.push_field(&field);
        let mut exif_buf = std::io::Cursor::new(Vec::new());
        writer.write(&mut exif_buf, false).unwrap();
        let exif_data = exif_buf.into_inner();

        // Inject EXIF into the PNG
        let result = inject_exif_into_png(&png_buf, &exif_data).unwrap();
        assert!(result.len() > png_buf.len());

        // Verify the EXIF can be read back
        let parsed = img_parts::png::Png::from_bytes(result.into()).unwrap();
        let extracted = img_parts::ImageEXIF::exif(&parsed);
        assert!(extracted.is_some());
    }

    #[test]
    fn inject_exif_rejects_invalid_jpeg() {
        let result = inject_exif_into_jpeg(b"not a jpeg", b"fake exif");
        assert!(result.is_err());
    }

    #[test]
    fn inject_exif_rejects_invalid_png() {
        let result = inject_exif_into_png(b"not a png", b"fake exif");
        assert!(result.is_err());
    }
}
