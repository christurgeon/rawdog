//! Output file naming from a template such as `{date}/{model}_{stem}`.

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Token {
    Stem,
    Date,
    Year,
    Month,
    Day,
    Time,
    Model,
    Rating,
    Seq,
}

const TOKENS: &[(&str, Token)] = &[
    ("stem", Token::Stem),
    ("date", Token::Date),
    ("year", Token::Year),
    ("month", Token::Month),
    ("day", Token::Day),
    ("time", Token::Time),
    ("model", Token::Model),
    ("rating", Token::Rating),
    ("seq", Token::Seq),
];

#[derive(Clone, Debug, PartialEq)]
enum Part {
    Literal(String),
    Token(Token),
}

/// A parsed `--name` template.
#[derive(Clone, Debug)]
pub struct NameTemplate(Vec<Part>);

impl FromStr for NameTemplate {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut rest = s;
        while let Some(open) = rest.find('{') {
            if open > 0 {
                parts.push(Part::Literal(rest[..open].to_string()));
            }
            let close = rest[open..]
                .find('}')
                .ok_or_else(|| "'{' without a matching '}'".to_string())?
                + open;
            let name = &rest[open + 1..close];
            let token = TOKENS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, t)| *t)
                .ok_or_else(|| {
                    let known: Vec<String> =
                        TOKENS.iter().map(|(n, _)| format!("{{{n}}}")).collect();
                    format!("unknown field '{{{name}}}'; use {}", known.join(", "))
                })?;
            parts.push(Part::Token(token));
            rest = &rest[close + 1..];
        }
        if !rest.is_empty() {
            parts.push(Part::Literal(rest.to_string()));
        }

        // The template is a path below the output directory.
        let literal: String = parts
            .iter()
            .map(|p| match p {
                Part::Literal(l) => l.as_str(),
                Part::Token(_) => "x",
            })
            .collect();
        let escapes = Path::new(&literal)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)));
        if literal.is_empty() || literal.ends_with('/') || escapes {
            return Err("must be a relative file path without '..'".to_string());
        }
        Ok(NameTemplate(parts))
    }
}

/// Capture time as (year, month, day, hour, minute, second).
type Taken = (u16, u8, u8, u8, u8, u8);

/// The values of one input file that a template can use.
#[derive(Debug, Default, PartialEq)]
pub struct NameInfo {
    pub stem: String,
    pub taken: Option<Taken>,
    pub model: Option<String>,
    /// Star rating: 1 to 5, 0 for unrated, -1 for rejected (Lightroom).
    pub rating: Option<i8>,
}

impl NameInfo {
    /// Read the values for `path`. Missing EXIF data is not an error.
    pub fn read(path: &Path, needs_exif: bool) -> NameInfo {
        let mut info = NameInfo {
            stem: path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        };
        if !needs_exif {
            return info;
        }
        let Ok(Some((fields, _))) = crate::exif::read_source_fields(path) else {
            info.rating = read_rating(path, &[]);
            return info;
        };

        let ascii = |tag| {
            fields
                .iter()
                .find(|f| f.tag == tag && f.ifd_num == exif::In::PRIMARY)
                .and_then(|f| match &f.value {
                    exif::Value::Ascii(v) => v.first().cloned(),
                    _ => None,
                })
        };
        info.taken = ascii(exif::Tag::DateTimeOriginal)
            .or_else(|| ascii(exif::Tag::DateTime))
            .and_then(|raw| exif::DateTime::from_ascii(&raw).ok())
            .map(|d| (d.year, d.month, d.day, d.hour, d.minute, d.second));
        info.model = ascii(exif::Tag::Model)
            .map(|raw| String::from_utf8_lossy(&raw).trim().to_string())
            .filter(|m| !m.is_empty());
        info.rating = read_rating(path, &fields);
        info
    }
}

/// The rating of a RAW file. Sources, in order:
/// 1. A sidecar XMP file, written by Lightroom, Capture One, or darktable.
///    It wins because it holds the photographer's later decision.
/// 2. The XMP packet in the file's main EXIF directory (Sony, Nikon, Leica,
///    Canon CR2) or the EXIF Rating tag.
/// 3. The XMP packet that the decoder reads (Canon CR3, Fujifilm RAF).
fn read_rating(path: &Path, fields: &[exif::Field]) -> Option<i8> {
    const XMP_TAG: exif::Tag = exif::Tag(exif::Context::Tiff, 0x02bc);
    const RATING_TAG: exif::Tag = exif::Tag(exif::Context::Tiff, 0x4746);

    let mut sidecars = vec![path.with_extension("xmp"), path.with_extension("XMP")];
    let mut darktable = path.as_os_str().to_owned();
    darktable.push(".xmp");
    sidecars.push(PathBuf::from(darktable));
    if let Some(rating) = sidecars
        .iter()
        .filter_map(|p| std::fs::read(p).ok())
        .find_map(|xmp| xmp_rating(&xmp))
    {
        return Some(rating);
    }

    for field in fields {
        let rating = match (&field.value, field.tag) {
            (exif::Value::Byte(xmp) | exif::Value::Undefined(xmp, _), tag) if tag == XMP_TAG => {
                xmp_rating(xmp)
            }
            (value, tag) if tag == RATING_TAG => value.get_uint(0).map(|v| v.min(5) as i8),
            _ => None,
        };
        if rating.is_some() {
            return rating;
        }
    }

    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    if ext != "cr3" && ext != "raf" {
        return None;
    }
    // rawler can panic on files that are not RAW files.
    let xmp = std::panic::catch_unwind(|| {
        let source = rawler::rawsource::RawSource::new(path).ok()?;
        let decoder = rawler::get_decoder(&source).ok()?;
        decoder
            .xpacket(&source, &rawler::decoders::RawDecodeParams::default())
            .ok()?
    })
    .ok()??;
    xmp_rating(&xmp)
}

/// The `xmp:Rating` value of an XMP packet, in element or attribute form.
fn xmp_rating(xmp: &[u8]) -> Option<i8> {
    let text = xmp_text(xmp);
    const NAME: &str = "xmp:Rating";
    let mut from = 0;
    while let Some(found) = text[from..].find(NAME) {
        let rest = &text[from + found + NAME.len()..];
        from += found + NAME.len();
        // The name must end here: not `xmp:RatingPercent`.
        if !rest.starts_with(['=', '>', ' ', '\t', '\n', '\r']) {
            continue;
        }
        let value = rest.trim_start_matches(|c: char| {
            c == '>' || c == '=' || c == '"' || c == '\'' || c.is_whitespace()
        });
        let end = value
            .find(|c: char| !(c.is_ascii_digit() || c == '-'))
            .unwrap_or(value.len());
        if let Ok(rating) = value[..end].parse::<i8>() {
            return Some(rating.clamp(-1, 5));
        }
    }
    None
}

/// XMP is UTF-8 in practice, but the standard also allows UTF-16.
fn xmp_text(xmp: &[u8]) -> String {
    let utf16 = |bytes: &[u8], big_endian: bool| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| {
                if big_endian {
                    u16::from_be_bytes(pair)
                } else {
                    u16::from_le_bytes(pair)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match xmp {
        [0xff, 0xfe, rest @ ..] => utf16(rest, false),
        [0xfe, 0xff, rest @ ..] => utf16(rest, true),
        [0xef, 0xbb, 0xbf, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        _ => String::from_utf8_lossy(xmp).into_owned(),
    }
}

impl NameTemplate {
    /// True if the template uses a value that comes from EXIF data.
    /// `{seq}` counts in capture order, so it needs the capture time.
    pub fn needs_exif(&self) -> bool {
        self.0
            .iter()
            .any(|p| matches!(p, Part::Token(t) if *t != Token::Stem))
    }

    /// True if the template uses `{seq}`.
    pub fn uses_seq(&self) -> bool {
        self.0.contains(&Part::Token(Token::Seq))
    }

    /// Build the output path, without extension, relative to the output
    /// directory. Values that the file does not have become `unknown`.
    /// `seq` is the value of `{seq}`.
    pub fn expand(&self, info: &NameInfo, seq: &str) -> PathBuf {
        let unknown = || "unknown".to_string();
        let taken = |f: fn(Taken) -> String| info.taken.map(f).unwrap_or_else(unknown);
        let mut out = String::new();
        for part in &self.0 {
            match part {
                Part::Literal(l) => out.push_str(l),
                Part::Token(token) => {
                    let value = match token {
                        Token::Stem => info.stem.clone(),
                        Token::Date => taken(|(y, m, d, ..)| format!("{y:04}-{m:02}-{d:02}")),
                        Token::Year => taken(|(y, ..)| format!("{y:04}")),
                        Token::Month => taken(|(_, m, ..)| format!("{m:02}")),
                        Token::Day => taken(|(_, _, d, ..)| format!("{d:02}")),
                        Token::Time => taken(|(_, _, _, h, m, s)| format!("{h:02}{m:02}{s:02}")),
                        Token::Model => info.model.clone().unwrap_or_else(unknown),
                        Token::Rating => info.rating.unwrap_or(0).to_string(),
                        Token::Seq => seq.to_string(),
                    };
                    // A value must not add path components, and must not
                    // be the name of the current or the parent directory.
                    let dots_only = !value.is_empty() && value.chars().all(|c| c == '.');
                    out.extend(value.chars().map(|c| match c {
                        '/' | '\\' | ':' | '\0' => '_',
                        '.' if dots_only => '_',
                        c => c,
                    }));
                }
            }
        }
        PathBuf::from(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> NameInfo {
        NameInfo {
            stem: "DSC0001".to_string(),
            taken: Some((2024, 3, 9, 7, 5, 42)),
            model: Some("ILCE-7M3".to_string()),
            rating: Some(4),
        }
    }

    fn expand(template: &str, info: &NameInfo) -> PathBuf {
        template.parse::<NameTemplate>().unwrap().expand(info, "0007")
    }

    #[test]
    fn expand_seq() {
        let template: NameTemplate = "{date}/{seq}_{stem}".parse().unwrap();
        assert!(template.uses_seq() && template.needs_exif());
        assert_eq!(
            template.expand(&info(), "0042"),
            PathBuf::from("2024-03-09/0042_DSC0001")
        );
        assert!(!"{stem}".parse::<NameTemplate>().unwrap().uses_seq());
    }

    #[test]
    fn expands_every_field() {
        assert_eq!(
            expand("{year}/{month}/{day}/{date}_{time}_{model}_{stem}", &info()),
            PathBuf::from("2024/03/09/2024-03-09_070542_ILCE-7M3_DSC0001")
        );
    }

    #[test]
    fn expands_rating_with_zero_for_unrated() {
        assert_eq!(expand("{rating}/{stem}", &info()), PathBuf::from("4/DSC0001"));
        let unrated = NameInfo {
            stem: "a".to_string(),
            ..Default::default()
        };
        assert_eq!(expand("{rating}/{stem}", &unrated), PathBuf::from("0/a"));
    }

    #[test]
    fn xmp_rating_reads_element_and_attribute_forms() {
        assert_eq!(xmp_rating(b"<xmp:Rating>3</xmp:Rating>"), Some(3));
        assert_eq!(xmp_rating(b"<x xmp:Rating=\"5\" />"), Some(5));
        assert_eq!(xmp_rating(b"xmp:Rating='-1'"), Some(-1));
        assert_eq!(xmp_rating(b"<xmp:Rating>0</xmp:Rating>"), Some(0));
        assert_eq!(xmp_rating(b"<xmp:CreatorTool>x</xmp:CreatorTool>"), None);
        assert_eq!(xmp_rating(b"xmp:Rating=\"9\""), Some(5));
        // A longer name that starts with the same text is not the rating.
        assert_eq!(
            xmp_rating(b"<d xmp:RatingPercent=\"20\" xmp:Rating=\"5\"/>"),
            Some(5)
        );
        assert_eq!(xmp_rating(b"<d xmp:RatingPercent=\"20\"/>"), None);
        assert_eq!(xmp_rating(b"<d MicrosoftPhoto:Rating=\"99\"/>"), None);
        // UTF-16 with a byte order mark, and UTF-8 with one.
        let utf16: Vec<u8> = [0xff, 0xfe]
            .into_iter()
            .chain("<x xmp:Rating=\"4\"/>".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(xmp_rating(&utf16), Some(4));
        assert_eq!(xmp_rating(b"\xef\xbb\xbfxmp:Rating=\"2\""), Some(2));
    }

    #[test]
    fn sidecar_rating_wins_over_the_rating_in_the_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let raw = tmp.path().join("DSC1.ARW");
        std::fs::write(&raw, b"not a raw file").unwrap();
        let in_file = [exif::Field {
            tag: exif::Tag(exif::Context::Tiff, 0x02bc),
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Byte(b"<xmp:Rating>2</xmp:Rating>".to_vec()),
        }];
        assert_eq!(read_rating(&raw, &in_file), Some(2));

        // Lightroom and Capture One: DSC1.xmp. darktable: DSC1.ARW.xmp.
        std::fs::write(tmp.path().join("DSC1.ARW.xmp"), b"xmp:Rating=\"4\"").unwrap();
        assert_eq!(read_rating(&raw, &in_file), Some(4));
        std::fs::write(tmp.path().join("DSC1.xmp"), b"xmp:Rating=\"5\"").unwrap();
        assert_eq!(read_rating(&raw, &in_file), Some(5));
    }

    #[test]
    fn literal_only_and_stem_only_templates() {
        assert_eq!(expand("proofs/{stem}", &info()), PathBuf::from("proofs/DSC0001"));
        assert_eq!(expand("{stem}-web", &info()), PathBuf::from("DSC0001-web"));
    }

    #[test]
    fn missing_values_become_unknown() {
        let info = NameInfo {
            stem: "a".to_string(),
            ..Default::default()
        };
        assert_eq!(
            expand("{date}/{model}_{stem}", &info),
            PathBuf::from("unknown/unknown_a")
        );
    }

    #[test]
    fn values_cannot_add_path_components() {
        let info = NameInfo {
            stem: "a".to_string(),
            taken: None,
            model: Some("../Evil/Cam".to_string()),
            rating: None,
        };
        assert_eq!(expand("{model}_{stem}", &info), PathBuf::from(".._Evil_Cam_a"));
    }

    #[test]
    fn a_value_of_dots_cannot_name_the_parent_directory() {
        let info = NameInfo {
            stem: "a".to_string(),
            taken: None,
            model: Some("..".to_string()),
            rating: None,
        };
        assert_eq!(expand("{model}/{stem}", &info), PathBuf::from("__/a"));
    }

    #[test]
    fn rejects_bad_templates() {
        for bad in [
            "",
            "{stem",
            "{nope}",
            "/abs/{stem}",
            "../{stem}",
            "a/../{stem}",
            "{date}/",
        ] {
            assert!(bad.parse::<NameTemplate>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn needs_exif_only_for_exif_fields() {
        assert!(!"x/{stem}".parse::<NameTemplate>().unwrap().needs_exif());
        assert!("{date}/{stem}".parse::<NameTemplate>().unwrap().needs_exif());
    }

    #[test]
    fn reads_date_and_model_from_a_tiff_container() {
        let fields = [
            exif::Field {
                tag: exif::Tag::Model,
                ifd_num: exif::In::PRIMARY,
                value: exif::Value::Ascii(vec![b"ILCE-7M3".to_vec()]),
            },
            exif::Field {
                tag: exif::Tag::DateTimeOriginal,
                ifd_num: exif::In::PRIMARY,
                value: exif::Value::Ascii(vec![b"2024:03:09 07:05:42".to_vec()]),
            },
        ];
        let mut writer = exif::experimental::Writer::new();
        for f in &fields {
            writer.push_field(f);
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        writer.write(&mut buf, true).unwrap();
        let tmp = tempfile::Builder::new().suffix(".arw").tempfile().unwrap();
        std::fs::write(tmp.path(), buf.into_inner()).unwrap();

        let read = NameInfo::read(tmp.path(), true);
        assert_eq!(read.taken, Some((2024, 3, 9, 7, 5, 42)));
        assert_eq!(read.model.as_deref(), Some("ILCE-7M3"));

        // Without EXIF fields in the template the file is not opened.
        assert_eq!(NameInfo::read(tmp.path(), false).taken, None);
    }

    #[test]
    fn file_without_exif_gives_only_the_stem() {
        let tmp = tempfile::Builder::new().suffix(".arw").tempfile().unwrap();
        std::fs::write(tmp.path(), b"not a raw file").unwrap();
        let read = NameInfo::read(tmp.path(), true);
        assert!(read.taken.is_none() && read.model.is_none());
        assert!(!read.stem.is_empty());
    }
}
