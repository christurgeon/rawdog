use std::io::{Cursor, Read, Seek, Write};
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
    /// The output pixels are sRGB, whatever colour space the camera was set
    /// to. False for `--preview`, which keeps the camera's own encoding.
    pub srgb: bool,
    /// Write the rating and colour label into the output as XMP.
    pub keep_rating: bool,
    /// Keywords written into the output as XMP `dc:subject`.
    pub keywords: Vec<String>,
    /// A GPS track for files without a position, and the camera clock's UTC
    /// offset in seconds when the files do not record it.
    pub gpx: Option<std::sync::Arc<crate::gpx::Track>>,
    pub gpx_tz: Option<i64>,
    /// Print resolution in pixels per inch. Replaces the source value.
    pub dpi: Option<u16>,
    /// Correction of the camera clock, applied to the capture dates.
    pub time_shift: Option<crate::timeshift::TimeShift>,
}

/// An sRGB ICC profile, generated once.
pub fn srgb_icc_profile() -> &'static [u8] {
    static PROFILE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    PROFILE.get_or_init(|| {
        let mut icc = moxcms::ColorProfile::new_srgb().encode().unwrap_or_default();
        // The encoder stamps the current time into the header, which made
        // two conversions of the same file differ. Use a fixed date. The
        // profile ID, which would cover the date, is left empty (zero).
        const DATE: [u8; 12] = [0x07, 0xe8, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0]; // 2024-01-01 00:00:00
        if icc.len() >= 100 && icc[84..100].iter().all(|b| *b == 0) {
            icc[24..36].copy_from_slice(&DATE);
        }
        icc
    })
}

/// TIFF tag that holds an ICC profile.
const ICC_PROFILE_TAG: exif::Tag = exif::Tag(exif::Context::Tiff, 0x8773);

/// Read the EXIF data of a source file.
///
/// `Ok(None)` means the file has no EXIF data that can be read. Besides
/// standard TIFF and JPEG containers this reads:
/// - Fujifilm RAF, which stores its metadata in an embedded JPEG preview;
/// - Panasonic RW2 and Olympus ORF, which are TIFF files with their own
///   magic number in the header.
pub fn read_source_exif(path: &Path) -> Result<Option<exif::Exif>> {
    // The metadata of most RAW files is in their first few hundred
    // kilobytes, and reading whole files is slow from a memory card. Read
    // the start first; read the whole file only if the start does not hold
    // complete metadata.
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Failed to open {} for EXIF extraction", path.display()))?;
    let mut head = Vec::new();
    (&mut file)
        .take(HEAD_BYTES)
        .read_to_end(&mut head)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let whole_file = (head.len() as u64) < HEAD_BYTES;

    if head.starts_with(b"FUJIFILMCCD-RAW") {
        return Ok(read_raf_exif(&mut file, &head));
    }
    // Canon CR3 is an ISO media file, which the EXIF reader cannot parse;
    // its metadata comes from the RAW decoder instead.
    if head.get(4..12) == Some(b"ftypcrx ") {
        return Ok(None);
    }
    match parse_tiff_exif(head) {
        Parsed::Complete(exif) => Ok(Some(exif)),
        Parsed::Partial(exif) if whole_file => Ok(Some(exif)),
        Parsed::None if whole_file => Ok(None),
        _ => {
            let data = std::fs::read(path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            Ok(match parse_tiff_exif(data) {
                Parsed::Complete(exif) | Parsed::Partial(exif) => Some(exif),
                Parsed::None => None,
            })
        }
    }
}

/// How much of a file `read_source_exif` reads first.
const HEAD_BYTES: u64 = 1 << 20;

/// The first `HEAD_BYTES` of a file as a decoder source. A CR3 file keeps
/// its metadata boxes at the start, and `RawSource::new` would read the
/// whole file (it maps and loads it), which is slow from a memory card.
/// Callers fall back to the whole file when the start is not enough.
pub fn head_source(path: &Path) -> Option<rawler::rawsource::RawSource> {
    let mut head = Vec::new();
    std::fs::File::open(path).ok()?.take(HEAD_BYTES).read_to_end(&mut head).ok()?;
    Some(rawler::rawsource::RawSource::new_from_slice(&head))
}

enum Parsed {
    Complete(exif::Exif),
    /// Some fields could not be read; in a truncated buffer that can mean
    /// that they are further on in the file.
    Partial(exif::Exif),
    None,
}

/// Parse the EXIF data of a TIFF-based RAW file.
fn parse_tiff_exif(mut data: Vec<u8>) -> Parsed {
    match data.get(..4) {
        // RW2 ("IIU\0") and ORF ("IIRO", "IIRS").
        Some([b'I', b'I', 0x55, 0x00] | [b'I', b'I', b'R', b'O' | b'S']) => {
            data[2..4].copy_from_slice(&[0x2a, 0x00]);
        }
        // Big-endian ORF ("MMOR").
        Some([b'M', b'M', b'O', b'R']) => data[2..4].copy_from_slice(&[0x00, 0x2a]),
        _ => {}
    }
    let mut reader = exif::Reader::new();
    reader.continue_on_error(true);
    match reader.read_from_container(&mut Cursor::new(&data)) {
        Ok(exif) => Parsed::Complete(exif),
        Err(exif::Error::PartialResult(partial)) => Parsed::Partial(partial.into_inner().0),
        Err(_) => Parsed::None,
    }
}

/// Read the EXIF data of a Fujifilm RAF file, which is in its embedded JPEG
/// preview. `head` is the start of the file.
fn read_raf_exif(file: &mut std::fs::File, head: &[u8]) -> Option<exif::Exif> {
    // Big-endian offset and length of the JPEG preview at bytes 84 and 88.
    let u32_at = |i: usize| {
        head.get(i..i + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64)
    };
    let (offset, len) = (u32_at(84)?, u32_at(88)?);
    // The EXIF segment is at the start of the JPEG and at most 64 KB long.
    let mut jpeg = Vec::new();
    file.seek(std::io::SeekFrom::Start(offset)).ok()?;
    file.take(len.min(1 << 17)).read_to_end(&mut jpeg).ok()?;
    let mut reader = exif::Reader::new();
    reader.continue_on_error(true);
    match reader.read_from_container(&mut Cursor::new(&jpeg)) {
        Ok(exif) => Some(exif),
        Err(exif::Error::PartialResult(partial)) => Some(partial.into_inner().0),
        Err(_) => None,
    }
}

/// Read the EXIF fields of a source file, prepared for an output image with
/// the given pixel dimensions.
///
/// Returns the fields and the source byte order (`true` = little endian), or
/// `None` if there is nothing to write.
pub fn read_fields_for_output(
    path: &Path,
    dims: (u32, u32),
    meta: &MetadataOptions,
) -> Result<Option<(Vec<exif::Field>, bool)>> {
    // Without source EXIF, artist and copyright can still be written.
    let (source, little_endian) = read_source_fields(path)?.unwrap_or_default();
    let fields = fields_for_output(source.iter(), dims, meta);
    Ok((!fields.is_empty()).then_some((fields, little_endian || source.is_empty())))
}

/// Read the metadata fields of a source file, with the source byte order.
///
/// Files whose EXIF container cannot be parsed (Canon CR3) get the main
/// fields from the RAW decoder instead.
pub fn read_source_fields(path: &Path) -> Result<Option<(Vec<exif::Field>, bool)>> {
    if let Some(exif) = read_source_exif(path)? {
        return Ok(Some((exif.fields().cloned().collect(), exif.little_endian())));
    }
    Ok(decoder_fields(path)
        .filter(|fields| !fields.is_empty())
        .map(|fields| (fields, true)))
}

/// Build EXIF fields from the metadata that the rawler decoder reads.
fn decoder_fields(path: &Path) -> Option<Vec<exif::Field>> {
    use exif::{Field, In, Tag, Value};
    use rawler::decoders::RawDecodeParams;
    use rawler::rawsource::RawSource;

    // rawler can panic on files that are not RAW files.
    let read = |source: RawSource| {
        std::panic::catch_unwind(move || {
            let decoder = rawler::get_decoder(&source).ok()?;
            decoder.raw_metadata(&source, &RawDecodeParams::default()).ok()
        })
        .ok()
        .flatten()
    };
    let meta = head_source(path)
        .and_then(read)
        .or_else(|| read(RawSource::new(path).ok()?))?;
    let e = meta.exif;

    let mut fields = Vec::new();
    let mut push = |tag: Tag, value: Option<Value>| {
        if let Some(value) = value {
            fields.push(Field {
                tag,
                ifd_num: In::PRIMARY,
                value,
            });
        }
    };
    let text = |s: Option<String>| {
        s.map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| Value::Ascii(vec![s.into_bytes()]))
    };
    let short = |v: Option<u16>| v.map(|v| Value::Short(vec![v]));
    let rational = |v: Option<rawler::formats::tiff::Rational>| {
        v.map(|r| Value::Rational(vec![exif::Rational { num: r.n, denom: r.d }]))
    };
    let srational = |v: Option<rawler::formats::tiff::SRational>| {
        v.map(|r| Value::SRational(vec![exif::SRational { num: r.n, denom: r.d }]))
    };
    let triple = |v: Option<[rawler::formats::tiff::Rational; 3]>| {
        v.map(|v| {
            Value::Rational(
                v.iter()
                    .map(|r| exif::Rational { num: r.n, denom: r.d })
                    .collect(),
            )
        })
    };

    push(Tag::Make, text(Some(meta.make)));
    push(Tag::Model, text(Some(meta.model)));
    push(Tag::Artist, text(e.artist));
    push(Tag::Copyright, text(e.copyright));
    push(Tag::DateTimeOriginal, text(e.date_time_original));
    push(Tag::DateTimeDigitized, text(e.create_date));
    push(Tag::DateTime, text(e.modify_date));
    push(Tag::ExposureTime, rational(e.exposure_time));
    push(Tag::FNumber, rational(e.fnumber));
    push(Tag::ExposureProgram, short(e.exposure_program));
    push(Tag::PhotographicSensitivity, short(e.iso_speed_ratings));
    push(Tag::ExposureBiasValue, srational(e.exposure_bias));
    push(Tag::MeteringMode, short(e.metering_mode));
    push(Tag::Flash, short(e.flash));
    push(Tag::FocalLength, rational(e.focal_length));
    push(Tag::WhiteBalance, short(e.white_balance));
    push(Tag::BodySerialNumber, text(e.serial_number));
    push(Tag::LensMake, text(e.lens_make));
    push(Tag::LensModel, text(e.lens_model));
    push(Tag::LensSerialNumber, text(e.lens_serial_number));
    if let Some(gps) = e.gps {
        push(Tag::GPSLatitudeRef, text(gps.gps_latitude_ref));
        push(Tag::GPSLatitude, triple(gps.gps_latitude));
        push(Tag::GPSLongitudeRef, text(gps.gps_longitude_ref));
        push(Tag::GPSLongitude, triple(gps.gps_longitude));
        push(Tag::GPSAltitudeRef, gps.gps_altitude_ref.map(|v| Value::Byte(vec![v])));
        push(Tag::GPSAltitude, rational(gps.gps_altitude));
    }
    Some(fields)
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
    match tag {
        // Below the first baseline TIFF tag: maker-specific RAW layout tags
        // (Panasonic RW2). From PrintIM upward: printer hints, and DNG tags
        // that would make software treat the output as a DNG file.
        Tag(Context::Tiff, n) if !(0xfe..0xc4a5).contains(&n) => true,
        Tag(Context::Tiff, n) => [
            0xfe,   // NewSubfileType: can mark the image as a preview
            0xff,   // SubfileType
            0x14a,  // SubIFDs: offsets into the source file
            0x2bc,  // XMP packet: large, and can repeat stripped fields
            0x828d, // CFARepeatPatternDim
            0x828e, // CFAPattern
            0x9216, // TIFF/EP standard ID
            0x9217, // TIFF/EP SensingMethod
        ]
        .contains(&n)
            || [
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
            ]
            .contains(&tag),
        _ => [
            // The Sony MakerNote uses offsets from the start of the ARW file.
            Tag::MakerNote,
            Tag::CFAPattern,
            Tag::SensingMethod,
            Tag::CompressedBitsPerPixel,
            Tag::ComponentsConfiguration,
        ]
        .contains(&tag),
    }
}

/// Largest field that is copied. Some cameras store megabytes of private data
/// in one tag (Panasonic S9: 5.7 MB), which would not fit in a JPEG.
const MAX_FIELD_BYTES: usize = 16 * 1024;

/// Approximate size in bytes of a field value.
fn value_size(value: &exif::Value) -> usize {
    use exif::Value;
    match value {
        Value::Byte(v) | Value::Undefined(v, _) => v.len(),
        Value::SByte(v) => v.len(),
        Value::Ascii(v) => v.iter().map(|s| s.len() + 1).sum(),
        Value::Short(v) => v.len() * 2,
        Value::SShort(v) => v.len() * 2,
        Value::Long(v) => v.len() * 4,
        Value::SLong(v) => v.len() * 4,
        Value::Float(v) => v.len() * 4,
        Value::Rational(v) => v.len() * 8,
        Value::SRational(v) => v.len() * 8,
        Value::Double(v) => v.len() * 8,
        Value::Unknown(_, count, _) => *count as usize,
    }
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
    fields: impl Iterator<Item = &'a exif::Field> + Clone,
    (width, height): (u32, u32),
    meta: &MetadataOptions,
) -> Vec<exif::Field> {
    use exif::{Context, Tag};

    let shift_seconds = meta.time_shift.as_ref().map_or(0, |shift| {
        let model = fields
            .clone()
            .find(|f| f.tag == Tag::Model && f.ifd_num == exif::In::PRIMARY)
            .map(|f| match &f.value {
                exif::Value::Ascii(v) => {
                    String::from_utf8_lossy(v.first().map_or(&[][..], |v| v)).trim().to_string()
                }
                _ => String::new(),
            });
        shift.seconds_for(model.as_deref())
    });

    let replaced = |tag: Tag| {
        (tag == Tag::Artist && meta.artist.is_some())
            || (tag == Tag::Copyright && meta.copyright.is_some())
            || (matches!(tag, Tag::XResolution | Tag::YResolution | Tag::ResolutionUnit)
                && meta.dpi.is_some())
    };
    let text_field = |tag: Tag, text: &String| exif::Field {
        tag,
        ifd_num: exif::In::PRIMARY,
        value: exif::Value::Ascii(vec![text.as_bytes().to_vec()]),
    };
    // `{year}` in the copyright is the capture year, or the current year
    // for a file without a capture date.
    let copyright = meta.copyright.as_ref().map(|text| {
        if !text.contains("{year}") {
            return text.clone();
        }
        let year = [Tag::DateTimeOriginal, Tag::DateTime]
            .iter()
            .find_map(|tag| {
                let field = fields.clone().find(|f| f.tag == *tag && f.ifd_num == exif::In::PRIMARY)?;
                let exif::Value::Ascii(texts) = &field.value else {
                    return None;
                };
                let d = exif::DateTime::from_ascii(texts.first()?).ok()?;
                let taken = (d.year, d.month, d.day, d.hour, d.minute, d.second);
                Some(crate::timeshift::add_seconds(taken, shift_seconds).0)
            })
            .unwrap_or_else(crate::timeshift::current_year);
        text.replace("{year}", &year.to_string())
    });
    let resolution = |tag: Tag| {
        meta.dpi.map(|dpi| exif::Field {
            tag,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Rational(vec![exif::Rational { num: dpi.into(), denom: 1 }]),
        })
    };
    let position = gpx_position(fields.clone(), meta, shift_seconds);
    let gps_fields = position.into_iter().flat_map(|(lat, lon)| {
        let gps = |tag, value| exif::Field { tag, ifd_num: exif::In::PRIMARY, value };
        let dms = |v: f64| {
            exif::Value::Rational(
                crate::gpx::to_dms(v).iter().map(|&(num, denom)| exif::Rational { num, denom }).collect(),
            )
        };
        let reference = |r: &str| exif::Value::Ascii(vec![r.as_bytes().to_vec()]);
        [
            gps(Tag::GPSVersionID, exif::Value::Byte(vec![2, 3, 0, 0])),
            gps(Tag::GPSLatitudeRef, reference(if lat < 0.0 { "S" } else { "N" })),
            gps(Tag::GPSLatitude, dms(lat)),
            gps(Tag::GPSLongitudeRef, reference(if lon < 0.0 { "W" } else { "E" })),
            gps(Tag::GPSLongitude, dms(lon)),
        ]
    });
    let added = [
        meta.artist.as_ref().map(|t| text_field(Tag::Artist, t)),
        copyright.as_ref().map(|t| text_field(Tag::Copyright, t)),
        resolution(Tag::XResolution),
        resolution(Tag::YResolution),
        meta.dpi.map(|_| exif::Field {
            tag: Tag::ResolutionUnit,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Short(vec![2]), // inches
        }),
    ];

    fields
        .filter(|_| !meta.strip_all)
        .filter(|f| f.ifd_num == exif::In::PRIMARY && !describes_source_layout(f.tag))
        .filter(|f| value_size(&f.value) <= MAX_FIELD_BYTES)
        .filter(|f| !(meta.strip_gps && f.tag.context() == Context::Gps))
        .filter(|f| !replaced(f.tag))
        // A camera set to Adobe RGB marks its files with ColorSpace 0xFFFF
        // and interoperability index "R03". The developed output is sRGB.
        .filter(|f| !(meta.srgb && f.tag.context() == exif::Context::Interop))
        .map(|field| {
            let mut field = field.clone();
            match field.tag {
                exif::Tag::Orientation => field.value = exif::Value::Short(vec![1]),
                exif::Tag::PixelXDimension => field.value = exif::Value::Long(vec![width]),
                exif::Tag::PixelYDimension => field.value = exif::Value::Long(vec![height]),
                exif::Tag::ColorSpace if meta.srgb => field.value = exif::Value::Short(vec![1]),
                Tag::DateTimeOriginal | Tag::DateTimeDigitized | Tag::DateTime
                    if shift_seconds != 0 =>
                {
                    if let Some(shifted) = shift_date_value(&field.value, shift_seconds) {
                        field.value = shifted;
                    }
                }
                _ => {}
            }
            field
        })
        .chain(added.into_iter().flatten())
        .chain(gps_fields)
        .collect()
}

/// The position for `--gpx`: from the track at the capture time, for a
/// file without GPS data of its own and unless GPS is stripped.
fn gpx_position<'a>(
    fields: impl Iterator<Item = &'a exif::Field> + Clone,
    meta: &MetadataOptions,
    shift_seconds: i64,
) -> Option<(f64, f64)> {
    use exif::Tag;
    let track = meta.gpx.as_ref().filter(|_| !meta.strip_gps)?;
    if fields.clone().any(|f| f.tag == Tag::GPSLatitude) {
        return None;
    }
    let ascii = |tag| {
        fields.clone().find(|f| f.tag == tag && f.ifd_num == exif::In::PRIMARY).and_then(|f| {
            match &f.value {
                exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).trim().to_string()),
                _ => None,
            }
        })
    };
    let d = exif::DateTime::from_ascii(ascii(Tag::DateTimeOriginal)?.as_bytes()).ok()?;
    let local = crate::timeshift::to_seconds((d.year, d.month, d.day, d.hour, d.minute, d.second));
    let offset = meta.gpx_tz.or_else(|| {
        ascii(Tag::OffsetTimeOriginal).and_then(|o| crate::gpx::parse_offset(&o))
    });
    let Some(offset) = offset else {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "Warning: some files do not record their time zone; give the camera clock's \
                 offset with --gpx-tz, such as --gpx-tz +02:00, to geotag them"
            );
        }
        return None;
    };
    track.position(local + shift_seconds - offset)
}

/// An EXIF date value ("YYYY:MM:DD HH:MM:SS") moved by `seconds`.
fn shift_date_value(value: &exif::Value, seconds: i64) -> Option<exif::Value> {
    let exif::Value::Ascii(texts) = value else {
        return None;
    };
    let d = exif::DateTime::from_ascii(texts.first()?).ok()?;
    let (y, mo, day, h, mi, s) = crate::timeshift::add_seconds(
        (d.year, d.month, d.day, d.hour, d.minute, d.second),
        seconds,
    );
    let text = format!("{y:04}:{mo:02}:{day:02} {h:02}:{mi:02}:{s:02}");
    Some(exif::Value::Ascii(vec![text.into_bytes()]))
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
    icc_profile: bool,
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
    let icc = Field {
        tag: ICC_PROFILE_TAG,
        ifd_num: In::PRIMARY,
        value: Value::Undefined(srgb_icc_profile().to_vec(), 0),
    };
    let profile = icc_profile.then_some(&icc);
    for field in layout.iter().chain(fields).chain(profile) {
        writer.push_field(field);
    }
    writer.set_strips(&strips, In::PRIMARY);
    writer
        .write(w, true)
        .map_err(|e| anyhow::anyhow!("Failed to write TIFF with EXIF: {}", e))
}

const JPEG_EXIF_MAX: usize = 65_533 - 6;

/// Inject raw EXIF bytes (a TIFF blob) into an already-encoded JPEG buffer.
///
/// Returns the new JPEG bytes with the EXIF data embedded in an APP1 segment.
pub fn inject_exif_into_jpeg(jpeg_bytes: &[u8], exif_data: &[u8]) -> Result<Vec<u8>> {
    use img_parts::jpeg::Jpeg;
    use img_parts::ImageEXIF;

    // A JPEG segment holds at most 65,533 bytes, 6 of them for the "Exif"
    // header. img-parts panics above that size.
    if exif_data.len() > JPEG_EXIF_MAX {
        anyhow::bail!(
            "EXIF data of {} bytes does not fit in a JPEG segment",
            exif_data.len()
        );
    }

    let mut jpeg = Jpeg::from_bytes(jpeg_bytes.to_vec().into())
        .map_err(|e| anyhow::anyhow!("Failed to parse JPEG for EXIF injection: {}", e))?;

    jpeg.set_exif(Some(img_parts::Bytes::from(exif_data.to_vec())));

    let mut output = Vec::with_capacity(jpeg.len());
    jpeg.encoder()
        .write_to(&mut output)
        .map_err(|e| anyhow::anyhow!("Failed to write JPEG with EXIF: {}", e))?;

    Ok(output)
}

/// An XMP packet with `xmp:Rating`, `xmp:Label`, and keywords
/// (`dc:subject`), which Lightroom and other catalogs and stock sites read
/// on import. `None` when there is none of them.
pub fn rating_xmp(rating: Option<i8>, label: Option<&str>, keywords: &[String]) -> Option<String> {
    if rating.is_none() && label.is_none() && keywords.is_empty() {
        return None;
    }
    let escape = |s: &str| {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    };
    let mut attributes = String::new();
    if let Some(rating) = rating {
        attributes.push_str(&format!(" xmp:Rating=\"{rating}\""));
    }
    if let Some(label) = label {
        // Lightroom's own labels start with a capital: Red, Green.
        let mut chars = label.chars();
        let label: String = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
        attributes.push_str(&format!(" xmp:Label=\"{}\"", escape(&label)));
    }
    let description = if keywords.is_empty() {
        format!("<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"{attributes}/>")
    } else {
        let items: String =
            keywords.iter().map(|k| format!("<rdf:li>{}</rdf:li>", escape(k))).collect();
        format!(
            "<rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\"{attributes}>\
             <dc:subject><rdf:Bag>{items}</rdf:Bag></dc:subject></rdf:Description>"
        )
    };
    Some(format!(
        "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\
         <x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
         {description}</rdf:RDF></x:xmpmeta><?xpacket end=\"w\"?>"
    ))
}

/// Add an XMP packet to a JPEG, in the APP1 segment that XMP readers look
/// for, after the other application segments.
pub fn inject_xmp_into_jpeg(jpeg_bytes: &[u8], xmp: &str) -> Result<Vec<u8>> {
    use img_parts::jpeg::{markers, Jpeg, JpegSegment};
    let mut jpeg = Jpeg::from_bytes(jpeg_bytes.to_vec().into())
        .map_err(|e| anyhow::anyhow!("Failed to parse JPEG for XMP: {}", e))?;
    let mut contents = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    contents.extend_from_slice(xmp.as_bytes());
    let segments = jpeg.segments_mut();
    let at = segments
        .iter()
        .position(|s| !(markers::APP0..=markers::APP15).contains(&s.marker()))
        .unwrap_or(segments.len());
    segments.insert(at, JpegSegment::new_with_contents(markers::APP1, contents.into()));
    let mut output = Vec::with_capacity(jpeg.len());
    jpeg.encoder()
        .write_to(&mut output)
        .map_err(|e| anyhow::anyhow!("Failed to write JPEG with XMP: {}", e))?;
    Ok(output)
}

/// Add an XMP packet to a PNG, in the iTXt chunk that XMP readers look for,
/// before the image data.
pub fn inject_xmp_into_png(png_bytes: &[u8], xmp: &str) -> Result<Vec<u8>> {
    use img_parts::png::{Png, PngChunk};
    let mut png = Png::from_bytes(png_bytes.to_vec().into())
        .map_err(|e| anyhow::anyhow!("Failed to parse PNG for XMP: {}", e))?;
    // Keyword, no compression, empty language tag and translated keyword.
    let mut contents = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
    contents.extend_from_slice(xmp.as_bytes());
    let chunks = png.chunks_mut();
    let at = chunks.iter().position(|c| c.kind() == *b"IDAT").unwrap_or(chunks.len());
    chunks.insert(at, PngChunk::new(*b"iTXt", contents.into()));
    let mut output = Vec::with_capacity(png.len());
    png.encoder()
        .write_to(&mut output)
        .map_err(|e| anyhow::anyhow!("Failed to write PNG with XMP: {}", e))?;
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
    fn rating_xmp_is_read_back_by_the_rating_parser() {
        let xmp = rating_xmp(Some(4), Some("red"), &[]).unwrap();
        assert!(xmp.contains(r#"xmp:Rating="4""#) && xmp.contains(r#"xmp:Label="Red""#), "{xmp}");
        assert_eq!(rating_xmp(None, None, &[]), None);
        assert!(rating_xmp(None, Some("a<b"), &[]).unwrap().contains("A&lt;b"));
        let tagged = rating_xmp(None, None, &["wedding".to_string(), "Smith & Co".to_string()]).unwrap();
        assert!(
            tagged.contains("<dc:subject><rdf:Bag><rdf:li>wedding</rdf:li><rdf:li>Smith &amp; Co</rdf:li></rdf:Bag></dc:subject>"),
            "{tagged}"
        );

        let img: image::RgbImage = image::ImageBuffer::from_pixel(2, 2, image::Rgb([1u8, 2, 3]));
        let mut jpeg = Vec::new();
        img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new(&mut jpeg)).unwrap();
        let with = inject_xmp_into_jpeg(&jpeg, &xmp).unwrap();
        assert!(with.windows(29).any(|w| w == b"http://ns.adobe.com/xap/1.0/\0"));
        assert!(image::load_from_memory(&with).is_ok());

        let mut png = Vec::new();
        img.write_with_encoder(image::codecs::png::PngEncoder::new(&mut png)).unwrap();
        let with = inject_xmp_into_png(&png, &xmp).unwrap();
        let parsed = img_parts::png::Png::from_bytes(with.clone().into()).unwrap();
        let kinds: Vec<[u8; 4]> = parsed.chunks().iter().map(|c| c.kind()).collect();
        let itxt = kinds.iter().position(|k| k == b"iTXt").unwrap();
        assert!(itxt < kinds.iter().position(|k| k == b"IDAT").unwrap());
        assert!(image::load_from_memory(&with).is_ok());
    }

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
        write_tiff_16bit_with_exif(&mut buf, (4, 3), &samples, &[description_field()], true)
            .unwrap();
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
    fn dpi_replaces_the_source_resolution() {
        use exif::{In, Tag, Value};
        let r = |v| Value::Rational(vec![exif::Rational { num: v, denom: 1 }]);
        let source = [
            field(Tag::XResolution, In::PRIMARY, r(350)),
            field(Tag::YResolution, In::PRIMARY, r(350)),
            field(Tag::ResolutionUnit, In::PRIMARY, Value::Short(vec![2])),
        ];
        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert_eq!(out.len(), 3);
        let meta = MetadataOptions { dpi: Some(300), strip_all: true, ..Default::default() };
        let out = fields_for_output(source.iter(), (300, 200), &meta);
        assert_eq!(tags(&out), vec![Tag::XResolution, Tag::YResolution, Tag::ResolutionUnit]);
        assert!(matches!(&out[0].value, Value::Rational(v) if v[0].num == 300 && v[0].denom == 1));
        assert_eq!(out[2].value.get_uint(0), Some(2));
    }

    #[test]
    fn gpx_adds_a_position_only_to_files_without_one() {
        use exif::{In, Tag, Value};
        let track = crate::gpx::Track::parse(
            r#"<trkpt lat="-33.5" lon="151.25"><time>2026-11-14T08:00:00Z</time></trkpt>"#,
        )
        .unwrap();
        let date = field(Tag::DateTimeOriginal, In::PRIMARY, Value::Ascii(vec![b"2026:11:14 10:00:00".to_vec()]));
        let meta = MetadataOptions {
            gpx: Some(std::sync::Arc::new(track)),
            gpx_tz: Some(7200),
            ..Default::default()
        };
        let out = fields_for_output([date.clone()].iter(), (300, 200), &meta);
        let get = |tag| out.iter().find(|f| f.tag == tag).map(|f| f.display_value().to_string());
        assert_eq!(get(Tag::GPSLatitudeRef).as_deref(), Some("S"));
        assert_eq!(get(Tag::GPSLongitudeRef).as_deref(), Some("E"));
        assert!(get(Tag::GPSLatitude).is_some());

        // A file with its own position keeps it; --strip-gps adds nothing.
        let own = field(Tag::GPSLatitude, In::PRIMARY, Value::Rational(vec![exif::Rational { num: 1, denom: 1 }]));
        let out = fields_for_output([date.clone(), own].iter(), (300, 200), &meta);
        assert_eq!(out.iter().filter(|f| f.tag == Tag::GPSLatitude).count(), 1);
        assert!(out.iter().all(|f| f.tag != Tag::GPSLongitude));
        let stripped = MetadataOptions { strip_gps: true, ..meta };
        let out = fields_for_output([date].iter(), (300, 200), &stripped);
        assert!(out.iter().all(|f| f.tag.context() != exif::Context::Gps));
    }

    #[test]
    fn copyright_year_is_the_shifted_capture_year() {
        use exif::{In, Tag, Value};
        let source = [field(
            Tag::DateTimeOriginal,
            In::PRIMARY,
            Value::Ascii(vec![b"2025:12:31 23:30:00".to_vec()]),
        )];
        let meta = |shift: Option<&str>| MetadataOptions {
            copyright: Some("(c) {year} J. Doe, {stem}".to_string()),
            time_shift: shift.map(|s| s.parse().unwrap()),
            ..Default::default()
        };
        let copyright = |out: Vec<exif::Field>| {
            out.into_iter().find(|f| f.tag == Tag::Copyright).unwrap().display_value().to_string()
        };
        let out = fields_for_output(source.iter(), (300, 200), &meta(None));
        assert_eq!(copyright(out), "\"(c) 2025 J. Doe, {stem}\"");
        let out = fields_for_output(source.iter(), (300, 200), &meta(Some("+1h")));
        assert_eq!(copyright(out), "\"(c) 2026 J. Doe, {stem}\"");
        let none: [exif::Field; 0] = [];
        let out = fields_for_output(none.iter(), (300, 200), &meta(None));
        let year = crate::timeshift::current_year();
        assert_eq!(copyright(out), format!("\"(c) {year} J. Doe, {{stem}}\""));
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
    fn fields_for_output_drops_tags_that_describe_the_raw_data() {
        use exif::{Context, In, Tag, Value};
        let short = |tag| field(tag, In::PRIMARY, Value::Short(vec![1]));
        let source = [
            description_field(),
            short(Tag(Context::Tiff, 0xfe)),   // NewSubfileType
            short(Tag(Context::Tiff, 0x2bc)),  // XMP
            short(Tag(Context::Tiff, 0x828e)), // CFAPattern (TIFF/EP)
            short(Tag(Context::Tiff, 0xc4a5)), // PrintIM
            short(Tag(Context::Tiff, 0xc612)), // DNGVersion
            short(Tag(Context::Tiff, 0x02)),   // Panasonic RW2 sensor width
            short(Tag::CFAPattern),
            short(Tag::SensingMethod),
            short(Tag::ComponentsConfiguration),
            field(Tag::Model, In::PRIMARY, Value::Ascii(vec![b"Cam".to_vec()])),
            short(Tag::ISOSpeed),
        ];
        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert_eq!(
            tags(&out),
            vec![Tag::ImageDescription, Tag::Model, Tag::ISOSpeed]
        );
    }

    #[test]
    fn fields_for_output_shifts_capture_dates_for_the_camera() {
        use exif::{In, Tag, Value};
        let date = |tag| field(tag, In::PRIMARY, Value::Ascii(vec![b"2026:11:14 23:30:00".to_vec()]));
        let source = [
            field(Tag::Model, In::PRIMARY, Value::Ascii(vec![b"ILCE-7M4".to_vec()])),
            date(Tag::DateTimeOriginal),
            date(Tag::DateTimeDigitized),
            date(Tag::DateTime),
            field(Tag::OffsetTimeOriginal, In::PRIMARY, Value::Ascii(vec![b"+01:00".to_vec()])),
        ];
        let meta = |shift: &str| MetadataOptions {
            time_shift: Some(shift.parse().unwrap()),
            ..Default::default()
        };
        let text = |out: &[exif::Field], tag| {
            out.iter().find(|f| f.tag == tag).unwrap().display_value().to_string()
        };
        let out = fields_for_output(source.iter(), (300, 200), &meta("7M4=+1h,Z 6=-1h"));
        for tag in [Tag::DateTimeOriginal, Tag::DateTimeDigitized, Tag::DateTime] {
            assert_eq!(text(&out, tag), "2026-11-15 00:30:00");
        }
        assert_eq!(text(&out, Tag::OffsetTimeOriginal), "\"+01:00\"");
        let out = fields_for_output(source.iter(), (300, 200), &meta("Z 6=-1h"));
        assert_eq!(text(&out, Tag::DateTimeOriginal), "2026-11-14 23:30:00");
    }

    #[test]
    fn fields_for_output_marks_developed_output_as_srgb() {
        use exif::{In, Tag, Value};
        let source = [
            field(Tag::ColorSpace, In::PRIMARY, Value::Short(vec![0xffff])),
            field(
                Tag::InteroperabilityIndex,
                In::PRIMARY,
                Value::Ascii(vec![b"R03".to_vec()]),
            ),
        ];
        let srgb = MetadataOptions {
            srgb: true,
            ..Default::default()
        };
        let out = fields_for_output(source.iter(), (300, 200), &srgb);
        assert_eq!(tags(&out), vec![Tag::ColorSpace]);
        assert_eq!(out[0].value.get_uint(0), Some(1));

        // --preview keeps the camera's own colour space.
        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].value.get_uint(0), Some(0xffff));
    }

    #[test]
    fn srgb_icc_profile_is_a_valid_rgb_display_profile() {
        let icc = srgb_icc_profile();
        assert!(icc.len() > 128);
        assert_eq!(&icc[12..16], b"mntr");
        assert_eq!(&icc[16..20], b"RGB ");
        assert_eq!(&icc[36..40], b"acsp");
        // A fixed creation date, so outputs are reproducible.
        assert_eq!(&icc[24..30], &[0x07, 0xe8, 0, 1, 0, 1]);
    }

    #[test]
    fn fields_for_output_drops_only_oversized_fields() {
        use exif::{Context, In, Tag, Value};
        let source = [
            description_field(),
            field(
                Tag(Context::Tiff, 0x127),
                In::PRIMARY,
                Value::Undefined(vec![0; MAX_FIELD_BYTES + 1], 0),
            ),
            field(Tag::Model, In::PRIMARY, Value::Ascii(vec![b"Cam".to_vec()])),
        ];
        let out = fields_for_output(source.iter(), (300, 200), &MetadataOptions::default());
        assert_eq!(tags(&out), vec![Tag::ImageDescription, Tag::Model]);
    }

    #[test]
    fn inject_exif_into_jpeg_rejects_data_larger_than_a_segment() {
        let img: image::RgbImage = image::ImageBuffer::from_pixel(2, 2, image::Rgb([1u8, 2, 3]));
        let mut jpeg_buf = Vec::new();
        img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new(&mut jpeg_buf))
            .unwrap();
        assert!(inject_exif_into_jpeg(&jpeg_buf, &vec![0u8; JPEG_EXIF_MAX + 1]).is_err());
        assert!(inject_exif_into_jpeg(&jpeg_buf, &vec![0u8; JPEG_EXIF_MAX]).is_ok());
    }

    /// The EXIF blob of a small TIFF file with a Model field.
    fn tiff_with_model() -> Vec<u8> {
        let tmp = write_tiff_with_fields(&[field(
            exif::Tag::Model,
            exif::In::PRIMARY,
            exif::Value::Ascii(vec![b"Cam".to_vec()]),
        )]);
        std::fs::read(tmp.path()).unwrap()
    }

    fn model_of(bytes: &[u8]) -> Option<String> {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), bytes).unwrap();
        let exif = read_source_exif(tmp.path()).unwrap()?;
        let model = exif.get_field(exif::Tag::Model, exif::In::PRIMARY)?;
        Some(model.display_value().to_string())
    }

    #[test]
    fn read_source_exif_reads_the_whole_file_when_metadata_continues_past_the_start() {
        // The writer puts the IFD first and the long value after it, so the
        // value ends beyond the first read.
        let long_text = vec![b'x'; HEAD_BYTES as usize + 10];
        let fields = [
            field(
                exif::Tag::Model,
                exif::In::PRIMARY,
                exif::Value::Ascii(vec![b"Cam".to_vec()]),
            ),
            field(
                exif::Tag::ImageDescription,
                exif::In::PRIMARY,
                exif::Value::Ascii(vec![long_text.clone()]),
            ),
        ];
        let tmp = write_tiff_with_fields(&fields);
        let exif = read_source_exif(tmp.path()).unwrap().unwrap();
        let description = exif
            .get_field(exif::Tag::ImageDescription, exif::In::PRIMARY)
            .unwrap();
        match &description.value {
            exif::Value::Ascii(v) => assert_eq!(v[0].len(), long_text.len()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn read_source_exif_skips_canon_cr3() {
        let mut cr3 = vec![0u8; 64];
        cr3[4..12].copy_from_slice(b"ftypcrx ");
        assert_eq!(model_of(&cr3), None);
    }

    #[test]
    fn read_source_exif_accepts_rw2_and_orf_headers() {
        let field = field(
            exif::Tag::Model,
            exif::In::PRIMARY,
            exif::Value::Ascii(vec![b"Cam".to_vec()]),
        );
        let tiff = |little_endian: bool| {
            let mut writer = exif::experimental::Writer::new();
            writer.push_field(&field);
            let mut buf = Cursor::new(Vec::new());
            writer.write(&mut buf, little_endian).unwrap();
            buf.into_inner()
        };

        // Little endian: plain TIFF, RW2, and the two ORF variants.
        for magic in [[0x2au8, 0x00], [0x55, 0x00], *b"RO", *b"RS"] {
            let mut raw = tiff(true);
            assert_eq!(&raw[..2], b"II");
            raw[2..4].copy_from_slice(&magic);
            assert_eq!(model_of(&raw).as_deref(), Some("\"Cam\""), "{magic:?}");
        }
        // Big endian: plain TIFF and ORF.
        for magic in [[0x00u8, 0x2a], *b"OR"] {
            let mut raw = tiff(false);
            assert_eq!(&raw[..2], b"MM");
            raw[2..4].copy_from_slice(&magic);
            assert_eq!(model_of(&raw).as_deref(), Some("\"Cam\""), "{magic:?}");
        }
    }

    #[test]
    fn read_source_exif_reads_the_jpeg_preview_of_a_raf_file() {
        // A JPEG that carries EXIF.
        let img: image::RgbImage = image::ImageBuffer::from_pixel(2, 2, image::Rgb([1u8, 2, 3]));
        let mut jpeg = Vec::new();
        img.write_with_encoder(image::codecs::jpeg::JpegEncoder::new(&mut jpeg))
            .unwrap();
        let jpeg = inject_exif_into_jpeg(&jpeg, &tiff_with_model()).unwrap();

        let mut raf = b"FUJIFILMCCD-RAW ".to_vec();
        raf.resize(100, 0);
        raf[84..88].copy_from_slice(&100u32.to_be_bytes());
        raf[88..92].copy_from_slice(&(jpeg.len() as u32).to_be_bytes());
        raf.extend_from_slice(&jpeg);
        assert_eq!(model_of(&raf).as_deref(), Some("\"Cam\""));

        // A length past the end of the file: what exists is read.
        raf[88..92].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(model_of(&raf).as_deref(), Some("\"Cam\""));

        // An offset past the end of the file is not an error.
        raf[84..88].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(model_of(&raf), None);
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
