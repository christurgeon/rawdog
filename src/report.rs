//! The `--report` file: one line or object per input file.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// What happened to one input file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Converted,
    Skipped,
    Failed,
    Excluded,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Converted => "converted",
            Status::Skipped => "skipped",
            Status::Failed => "failed",
            Status::Excluded => "excluded",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Record {
    pub input: PathBuf,
    pub output: Option<PathBuf>,
    pub status: Status,
    /// The error, or the reason for an exclusion.
    pub message: String,
    pub seconds: f64,
}

/// Write the records as JSON if `path` ends in `.json`, otherwise as CSV.
pub fn write(path: &Path, records: &mut [Record]) -> std::io::Result<()> {
    records.sort_by(|a, b| a.input.cmp(&b.input));
    let json = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"));
    let text = if json { to_json(records) } else { to_csv(records) };
    std::fs::write(path, text)
}

fn to_csv(records: &[Record]) -> String {
    // Quote every field; double the quotes inside it.
    let field = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut out = String::from("input,output,status,message,seconds\n");
    for r in records {
        let _ = writeln!(
            out,
            "{},{},{},{},{:.2}",
            field(&r.input.to_string_lossy()),
            field(&r.output.as_deref().map(|p| p.to_string_lossy()).unwrap_or_default()),
            r.status.as_str(),
            field(&r.message),
            r.seconds
        );
    }
    out
}

fn to_json(records: &[Record]) -> String {
    let string = |s: &str| {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    let _ = write!(out, "\\u{:04x}", c as u32);
                }
                c => out.push(c),
            }
        }
        out.push('"');
        out
    };
    let items: Vec<String> = records
        .iter()
        .map(|r| {
            format!(
                "  {{\"input\": {}, \"output\": {}, \"status\": \"{}\", \"message\": {}, \"seconds\": {:.2}}}",
                string(&r.input.to_string_lossy()),
                r.output
                    .as_deref()
                    .map(|p| string(&p.to_string_lossy()))
                    .unwrap_or_else(|| "null".to_string()),
                r.status.as_str(),
                string(&r.message),
                r.seconds
            )
        })
        .collect();
    format!("[\n{}\n]\n", items.join(",\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records() -> Vec<Record> {
        vec![
            Record {
                input: PathBuf::from("b, \"odd\".ARW"),
                output: None,
                status: Status::Failed,
                message: "line one\nline \"two\"".to_string(),
                seconds: 0.5,
            },
            Record {
                input: PathBuf::from("a.ARW"),
                output: Some(PathBuf::from("out/a.jpg")),
                status: Status::Converted,
                message: String::new(),
                seconds: 1.234,
            },
        ]
    }

    #[test]
    fn csv_quotes_every_text_field() {
        let mut r = records();
        r.sort_by(|a, b| a.input.cmp(&b.input));
        let csv = to_csv(&r);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "input,output,status,message,seconds");
        assert_eq!(lines[1], "\"a.ARW\",\"out/a.jpg\",converted,\"\",1.23");
        assert!(csv.contains("\"b, \"\"odd\"\".ARW\",\"\",failed,\"line one\nline \"\"two\"\"\",0.50"));
    }

    #[test]
    fn json_escapes_strings_and_uses_null_for_no_output() {
        let json = to_json(&records());
        assert!(json.contains(r#""input": "b, \"odd\".ARW", "output": null"#), "{json}");
        assert!(json.contains(r#""message": "line one\nline \"two\"""#), "{json}");
        assert!(json.trim_start().starts_with('[') && json.trim_end().ends_with(']'));
    }

    #[test]
    fn write_sorts_and_picks_the_format_from_the_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut r = records();
        write(&tmp.path().join("run.JSON"), &mut r).unwrap();
        let json = std::fs::read_to_string(tmp.path().join("run.JSON")).unwrap();
        assert!(json.find("a.ARW").unwrap() < json.find("b, ").unwrap());

        write(&tmp.path().join("run.csv"), &mut r).unwrap();
        let csv = std::fs::read_to_string(tmp.path().join("run.csv")).unwrap();
        assert!(csv.starts_with("input,"));
    }
}
