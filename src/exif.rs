use std::io::{BufReader, Cursor, Seek, Write};
use std::path::Path;

use anyhow::{Context, Result};

/// What to do with metadata when it is copied to the output.
#[derive(Clone, Default)]
pub struct MetadataOptions {
    /// Copy no source metadata.
    pub strip_all: bool,
    /// Do not copy GPS fields.
    pub strip_gps: bool,
    /// Value for the Artist tag. Replaces the source value.
    pub artist: Option<String>,
    /// Value for the Copyright tag. Replaces the source value.
    pub copyright: Option<String>,
}

/// Read the EXIF fields of a source file, prepared for an output image with
/// the given pixel dimensions.
///
/// Returns the fields and the source byte order (`true` = little endian), or
/// `None` if the file has no usable EXIF data.
pub fn read_fields_for_output(
    path: &Path,
    dims: (u32, u32),
    meta: &MetadataOptions,
) -> Result<Option<(Vec<exif::Field>, bool)>> {
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
        // No source EXIF. Artist and copyright can still be written.
        Err(_) => {
            let fields = fields_for_output(std::iter::empty(), dims, meta);
            return Ok((!fields.is_empty()).then_some((fields, true)));
        }
    };

    let fields = fields_for_output(exif.fields(), dims, meta);
    if fields.is_empty() {
        return Ok(None);
    }
    Ok(Some((fields, exif.little_endian())))
}

/// Extract EXIF from a source ARW (TIFF-based) file as a TIFF byte blob.
///
/// The blob is suitable for injection into JPEG (via img-parts `set_exif`)
/// or PNG (via eXIf chunk). Returns `None` if no EXIF data could be extracted.
pub fn extract_exif(
    path: &Path,
    dims: (u32, u32),
    meta: &MetadataOptions,
) -> Result<Option<Vec<u8>>> {
    let Some((fields, little_endian)) = read_fields_for_output(path, dims, meta)? else {
        return Ok(None);
    };

    // Use the experimental Writer to serialize the parsed fields into a
    // minimal TIFF blob containing only the metadata IFDs.
    let mut writer = exif::experimental::Writer::new();
    for field in &fields {
        writer.push_field(field);
    }

    let mut buf = Cursor::new(Vec::new());
    writer
        .write(&mut buf, little_endian)
        .map_err(|e| anyhow::anyhow!("Failed to serialize EXIF data: {}", e))?;

    let data = buf.into_inner();
    if data.is_empty() {
        return Ok(None);
    }

    Ok(Some(data))
}

/// Tags that describe how the source file stores its data. They are wrong
/// for the converted image, and some hold offsets into the source file.
fn describes_source_layout(tag: exif::Tag) -> bool {
    use exif::{Context, Tag};
    const SUB_IFDS: Tag = Tag(Context::Tiff, 0x14a);
    const DNG_PRIVATE_DATA: Tag = Tag(Context::Tiff, 0xc634);
    [
        Tag::ImageWidth,
        Tag::ImageLength,
        Tag::BitsPerSample,
        Tag::Compression,
        Tag::PhotometricInterpretation,
        Tag::SamplesPerPixel,
        Tag::RowsPerStrip,
        Tag::PlanarConfiguration,
        Tag::YCbCrCoefficients,
        Tag::YCbCrSubSampling,
        Tag::YCbCrPositioning,
        Tag::ReferenceBlackWhite,
        // The Sony MakerNote uses offsets from the start of the ARW file.
        Tag::MakerNote,
        SUB_IFDS,
        DNG_PRIVATE_DATA,
    ]
    .contains(&tag)
}

/// Prepare source EXIF fields for the converted image.
///
/// - Only the primary image's fields are kept. The source thumbnail is not
///   copied, so its IFD would describe data that does not exist.
/// - Tags that describe the source file layout are dropped.
/// - imagepipe rotates the pixels according to the RAW orientation, so any
///   Orientation tag is reset to 1 (normal). A missing tag is not added.
/// - PixelXDimension and PixelYDimension are set to the output size.
/// - `meta` can remove GPS fields or all source fields, and can set the
///   Artist and Copyright tags.
fn fields_for_output<'a>(
    fields: impl Iterator<Item = &'a exif::Field>,
    (width, height): (u32, u32),
    meta: &MetadataOptions,
) -> Vec<exif::Field> {
    use exif::{Context, Tag};

    let replaced = |tag: Tag| {
        (tag == Tag::Artist && meta.artist.is_some())
            || (tag == Tag::Copyright && meta.copyright.is_some())
    };
    let text_field = |tag: Tag, text: &String| exif::Field {
        tag,
        ifd_num: exif::In::PRIMARY,
        value: exif::Value::Ascii(vec![text.as_bytes().to_vec()]),
    };
    let added = [
        meta.artist.as_ref().map(|t| text_field(Tag::Artist, t)),
        meta.copyright.as_ref().map(|t| text_field(Tag::Copyright, t)),
    ];

    fields
        .filter(|_| !meta.strip_all)
        .filter(|f| f.ifd_num == exif::In::PRIMARY && !describes_source_layout(f.tag))
        .filter(|f| !(meta.strip_gps && f.tag.context() == Context::Gps))
        .filter(|f| !replaced(f.tag))
        .map(|field| {
            let mut field = field.clone();
            match field.tag {
                exif::Tag::Orientation => field.value = exif::Value::Short(vec![1]),
                exif::Tag::PixelXDimension => field.value = exif::Value::Long(vec![width]),
                exif::Tag::PixelYDimension => field.value = exif::Value::Long(vec![height]),
                _ => {}
            }
            field
        })
        .chain(added.into_iter().flatten())
        .collect()
}

/// Write a 16-bit RGB TIFF file that carries the given EXIF fields.
///
/// `fields` must come from `read_fields_for_output`, which removes the
/// layout tags that this function sets.
pub fn write_tiff_16bit_with_exif<W: Write + Seek>(
    w: &mut W,
    (width, height): (u32, u32),
    samples: &[u16],
    fields: &[exif::Field],
) -> Result<()> {
    use exif::{Field, In, Tag, Value};

    let layout: Vec<Field> = [
        (Tag::ImageWidth, Value::Long(vec![width])),
        (Tag::ImageLength, Value::Long(vec![height])),
        (Tag::BitsPerSample, Value::Short(vec![16, 16, 16])),
        (Tag::Compression, Value::Short(vec![1])),
        (Tag::PhotometricInterpretation, Value::Short(vec![2])),
        (Tag::SamplesPerPixel, Value::Short(vec![3])),
        (Tag::RowsPerStrip, Value::Long(vec![height])),
        (Tag::PlanarConfiguration, Value::Short(vec![1])),
    ]
    .into_iter()
    .map(|(tag, value)| Field {
        tag,
        ifd_num: In::PRIMARY,
        value,
    })
    .collect();

    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let strips = [bytes.as_slice()];

    let mut writer = exif::experimental::Writer::new();
    for field in layout.iter().chain(fields) {
        writer.push_field(field);
    }
    writer.set_strips(&strips, In::PRIMARY);
    writer
        .write(w, true)
        .map_err(|e| anyhow::anyhow!("Failed to write TIFF with EXIF: {}", e))
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

    // img-parts appends the eXIf chunk after the image data. Some readers,
    // including macOS ImageIO, only read it when it comes before IDAT.
    let chunks = png.chunks_mut();
    if let (Some(exif_pos), Some(idat_pos)) = (
        chunks.iter().position(|c| c.kind() == *b"eXIf"),
        chunks.iter().position(|c| c.kind() == *b"IDAT"),
    ) {
        if exif_pos > idat_pos {
            let chunk = chunks.remove(exif_pos);
            chunks.insert(idat_pos, chunk);
        }
    }

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
        let result = extract_exif(Path::new("/nonexistent/photo.arw"), (300, 200), &MetadataOptions::default());
        assert!(result.is_err());
    }

    #[test]
    fn extract_exif_returns_none_for_non_arw_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not a raw file").unwrap();
        let result = extract_exif(tmp.path(), (300, 200), &MetadataOptions::default()).unwrap();
        assert!(result.is_none());
    }

    fn orientation_field(value: u16) -> exif::Field {
        exif::Field {
            tag: exif::Tag::Orientation,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Short(vec![value]),
        }
    }

    fn description_field() -> exif::Field {
        exif::Field {
            tag: exif::Tag::ImageDescription,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![b"RawDog Test".to_vec()]),
        }
    }

    /// Serialize fields as a TIFF file, which is the container format of ARW.
    fn write_tiff_with_fields(fields: &[exif::Field]) -> tempfile::NamedTempFile {
        let mut writer = exif::experimental::Writer::new();
        for f in fields {
            writer.push_field(f);
        }
        let mut buf = Cursor::new(Vec::new());
        writer.write(&mut buf, true).unwrap();
        let tmp = tempfile::Builder::new().suffix(".arw").tempfile().unwrap();
        std::fs::write(tmp.path(), buf.into_inner()).unwrap();
        tmp
    }

    fn orientation_of(exif_data: &[u8]) -> Option<u32> {
        let parsed = exif::Reader::new().read_raw(exif_data.to_vec()).unwrap();
        parsed
            .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
    }

    #[test]
    fn fields_for_output_resets_orientation() {
        let source = [orientation_field(8), description_field()];
        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert_eq!(out.len(), 2);
        let orientation = out
            .iter()
            .find(|f| f.tag == exif::Tag::Orientation)
            .unwrap();
        assert_eq!(orientation.value.get_uint(0), Some(1));
        let desc = out
            .iter()
            .find(|f| f.tag == exif::Tag::ImageDescription)
            .unwrap();
        assert_eq!(desc.display_value().to_string(), "\"RawDog Test\"");
    }

    #[test]
    fn fields_for_output_does_not_add_orientation() {
        let source = [description_field()];
        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert!(out.iter().all(|f| f.tag != exif::Tag::Orientation));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn extract_exif_resets_orientation_to_normal() {
        // imagepipe already rotates the pixels, so a copied Orientation of 6
        // would make viewers rotate the image a second time.
        let tmp = write_tiff_with_fields(&[orientation_field(6), description_field()]);
        let data = extract_exif(tmp.path(), (300, 200), &MetadataOptions::default()).unwrap().unwrap();
        assert_eq!(orientation_of(&data), Some(1));
    }

    #[test]
    fn extract_exif_does_not_add_missing_orientation() {
        let tmp = write_tiff_with_fields(&[description_field()]);
        let data = extract_exif(tmp.path(), (300, 200), &MetadataOptions::default()).unwrap().unwrap();
        assert_eq!(orientation_of(&data), None);
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

    fn field(tag: exif::Tag, ifd_num: exif::In, value: exif::Value) -> exif::Field {
        exif::Field {
            tag,
            ifd_num,
            value,
        }
    }

    #[test]
    fn fields_for_output_drops_source_layout_and_thumbnail_fields() {
        use exif::{Context, In, Tag, Value};
        let source = [
            description_field(),
            field(Tag::Compression, In::PRIMARY, Value::Short(vec![6])),
            field(Tag::MakerNote, In::PRIMARY, Value::Undefined(vec![1, 2, 3], 0)),
            field(Tag(Context::Tiff, 0x14a), In::PRIMARY, Value::Long(vec![143014])),
            field(Tag::Compression, In::THUMBNAIL, Value::Short(vec![6])),
            field(Tag::Model, In::THUMBNAIL, Value::Ascii(vec![b"X".to_vec()])),
        ];
        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tag, Tag::ImageDescription);
    }

    #[test]
    fn fields_for_output_sets_pixel_dimensions_to_output_size() {
        use exif::{In, Tag, Value};
        let source = [
            field(Tag::PixelXDimension, In::PRIMARY, Value::Long(vec![2768])),
            field(Tag::PixelYDimension, In::PRIMARY, Value::Long(vec![1848])),
        ];
        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert_eq!(out[0].value.get_uint(0), Some(300));
        assert_eq!(out[1].value.get_uint(0), Some(200));
    }

    #[test]
    fn inject_exif_into_png_places_chunk_before_image_data() {
        let img: image::RgbImage = image::ImageBuffer::from_pixel(2, 2, image::Rgb([1u8, 2, 3]));
        let mut png_buf = Vec::new();
        img.write_with_encoder(image::codecs::png::PngEncoder::new(&mut png_buf))
            .unwrap();
        let tmp = write_tiff_with_fields(&[description_field()]);
        let exif_data = std::fs::read(tmp.path()).unwrap();

        let result = inject_exif_into_png(&png_buf, &exif_data).unwrap();
        let png = img_parts::png::Png::from_bytes(result.into()).unwrap();
        let kinds: Vec<[u8; 4]> = png.chunks().iter().map(|c| c.kind()).collect();
        let exif_pos = kinds.iter().position(|k| k == b"eXIf").unwrap();
        let idat_pos = kinds.iter().position(|k| k == b"IDAT").unwrap();
        assert!(exif_pos < idat_pos, "chunk order: {kinds:?}");
    }

    #[test]
    fn write_tiff_16bit_with_exif_roundtrips_pixels_and_fields() {
        let samples: Vec<u16> = (0..4 * 3 * 3).map(|i| i * 1000).collect();
        let mut buf = Cursor::new(Vec::new());
        write_tiff_16bit_with_exif(&mut buf, (4, 3), &samples, &[description_field()]).unwrap();
        let bytes = buf.into_inner();

        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Tiff)
            .unwrap()
            .into_rgb16();
        assert_eq!(decoded.dimensions(), (4, 3));
        assert_eq!(decoded.as_raw(), &samples);

        let parsed = exif::Reader::new()
            .read_from_container(&mut Cursor::new(&bytes))
            .unwrap();
        let desc = parsed
            .get_field(exif::Tag::ImageDescription, exif::In::PRIMARY)
            .unwrap();
        assert_eq!(desc.display_value().to_string(), "\"RawDog Test\"");
    }

    fn gps_field() -> exif::Field {
        field(
            exif::Tag::GPSLatitudeRef,
            exif::In::PRIMARY,
            exif::Value::Ascii(vec![b"N".to_vec()]),
        )
    }

    fn tags(fields: &[exif::Field]) -> Vec<exif::Tag> {
        fields.iter().map(|f| f.tag).collect()
    }

    #[test]
    fn fields_for_output_strip_gps_removes_only_gps() {
        let source = [description_field(), gps_field()];
        let meta = MetadataOptions {
            strip_gps: true,
            ..Default::default()
        };
        let out = fields_for_output(source.iter(), (300, 200), &meta);
        assert_eq!(tags(&out), vec![exif::Tag::ImageDescription]);

        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn fields_for_output_strip_all_keeps_only_requested_attribution() {
        let source = [description_field(), gps_field()];
        let meta = MetadataOptions {
            strip_all: true,
            copyright: Some("(c) Test".to_string()),
            ..Default::default()
        };
        let out = fields_for_output(source.iter(), (300, 200), &meta);
        assert_eq!(tags(&out), vec![exif::Tag::Copyright]);

        let meta = MetadataOptions {
            strip_all: true,
            ..Default::default()
        };
        assert!(fields_for_output(source.iter(), (300, 200), &meta).is_empty());
    }

    #[test]
    fn fields_for_output_artist_replaces_source_value() {
        let source = [field(
            exif::Tag::Artist,
            exif::In::PRIMARY,
            exif::Value::Ascii(vec![b"Old".to_vec()]),
        )];
        let meta = MetadataOptions {
            artist: Some("New".to_string()),
            ..Default::default()
        };
        let out = fields_for_output(source.iter(), (300, 200), &meta);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].display_value().to_string(), "\"New\"");
    }

    #[test]
    fn extract_exif_adds_copyright_when_source_has_no_exif() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not a raw file").unwrap();
        let meta = MetadataOptions {
            copyright: Some("(c) Test".to_string()),
            ..Default::default()
        };
        let data = extract_exif(tmp.path(), (300, 200), &meta)
            .unwrap()
            .unwrap();
        let parsed = exif::Reader::new().read_raw(data).unwrap();
        let c = parsed
            .get_field(exif::Tag::Copyright, exif::In::PRIMARY)
            .unwrap();
        assert_eq!(c.display_value().to_string(), "\"(c) Test\"");
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
