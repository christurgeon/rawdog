//! Output file naming from a template such as `{date}/{model}_{stem}`.

use std::io::BufReader;
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
}

const TOKENS: &[(&str, Token)] = &[
    ("stem", Token::Stem),
    ("date", Token::Date),
    ("year", Token::Year),
    ("month", Token::Month),
    ("day", Token::Day),
    ("time", Token::Time),
    ("model", Token::Model),
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
        let Ok(file) = std::fs::File::open(path) else {
            return info;
        };
        let exif = match exif::Reader::new()
            .continue_on_error(true)
            .read_from_container(&mut BufReader::new(file))
        {
            Ok(exif) => exif,
            Err(exif::Error::PartialResult(partial)) => partial.into_inner().0,
            Err(_) => return info,
        };

        let ascii = |tag| match exif.get_field(tag, exif::In::PRIMARY).map(|f| &f.value) {
            Some(exif::Value::Ascii(v)) => v.first().cloned(),
            _ => None,
        };
        info.taken = ascii(exif::Tag::DateTimeOriginal)
            .or_else(|| ascii(exif::Tag::DateTime))
            .and_then(|raw| exif::DateTime::from_ascii(&raw).ok())
            .map(|d| (d.year, d.month, d.day, d.hour, d.minute, d.second));
        info.model = ascii(exif::Tag::Model)
            .map(|raw| String::from_utf8_lossy(&raw).trim().to_string())
            .filter(|m| !m.is_empty());
        info
    }
}

impl NameTemplate {
    /// True if the template uses a value that comes from EXIF data.
    pub fn needs_exif(&self) -> bool {
        self.0
            .iter()
            .any(|p| matches!(p, Part::Token(t) if *t != Token::Stem))
    }

    /// Build the output path, without extension, relative to the output
    /// directory. Values that the file does not have become `unknown`.
    pub fn expand(&self, info: &NameInfo) -> PathBuf {
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
                    };
                    // A value must not add path components.
                    out.extend(value.chars().map(|c| match c {
                        '/' | '\\' | ':' | '\0' => '_',
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
        }
    }

    fn expand(template: &str, info: &NameInfo) -> PathBuf {
        template.parse::<NameTemplate>().unwrap().expand(info)
    }

    #[test]
    fn expands_every_field() {
        assert_eq!(
            expand("{year}/{month}/{day}/{date}_{time}_{model}_{stem}", &info()),
            PathBuf::from("2024/03/09/2024-03-09_070542_ILCE-7M3_DSC0001")
        );
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
        };
        assert_eq!(expand("{model}_{stem}", &info), PathBuf::from(".._Evil_Cam_a"));
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
