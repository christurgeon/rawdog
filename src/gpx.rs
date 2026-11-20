//! `--gpx`: positions from a GPS track, for cameras without GPS.

use std::str::FromStr;

use crate::timeshift::{to_seconds, Taken};

/// Two track points further apart than this are not interpolated.
const MAX_GAP: i64 = 300;
/// A capture time outside the track's points uses the nearest point only
/// when it is at most this far away.
const MAX_DISTANCE: i64 = 60;

/// The points of a GPX track, sorted by time.
#[derive(Debug, Default)]
pub struct Track {
    /// (seconds since 1970 UTC, latitude, longitude)
    points: Vec<(i64, f64, f64)>,
}

impl Track {
    /// Read the `<trkpt>` points of a GPX file. Points without a time are
    /// left out.
    pub fn parse(text: &str) -> Result<Track, String> {
        let mut points = Vec::new();
        for chunk in text.split("<trkpt").skip(1) {
            let end = chunk.find("</trkpt>").unwrap_or(chunk.len());
            let point = &chunk[..end];
            let attribute = |name: &str| -> Option<f64> {
                let start = point.find(&format!("{name}="))? + name.len() + 1;
                let quote = point[start..].chars().next()?;
                let value = &point[start + 1..];
                value[..value.find(quote)?].trim().parse().ok()
            };
            let time = point
                .find("<time>")
                .and_then(|start| {
                    let value = &point[start + 6..];
                    parse_utc(&value[..value.find("</time>")?])
                });
            if let (Some(lat), Some(lon), Some(time)) = (attribute("lat"), attribute("lon"), time) {
                if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
                    points.push((time, lat, lon));
                }
            }
        }
        if points.is_empty() {
            return Err("it has no track points with a time".to_string());
        }
        points.sort_by_key(|p| p.0);
        Ok(Track { points })
    }

    /// The position at `utc` seconds: interpolated between the two points
    /// around it, or the nearest point when the time is just outside.
    pub fn position(&self, utc: i64) -> Option<(f64, f64)> {
        let after = self.points.partition_point(|p| p.0 < utc);
        let next = self.points.get(after);
        let previous = after.checked_sub(1).and_then(|i| self.points.get(i));
        match (previous, next) {
            (_, Some(&(t, lat, lon))) if t == utc => Some((lat, lon)),
            (Some(&(t0, lat0, lon0)), Some(&(t1, lat1, lon1))) if t1 - t0 <= MAX_GAP => {
                let f = (utc - t0) as f64 / (t1 - t0) as f64;
                Some((lat0 + (lat1 - lat0) * f, lon0 + (lon1 - lon0) * f))
            }
            (previous, next) => [previous, next]
                .into_iter()
                .flatten()
                .filter(|p| (p.0 - utc).abs() <= MAX_DISTANCE)
                .min_by_key(|p| (p.0 - utc).abs())
                .map(|&(_, lat, lon)| (lat, lon)),
        }
    }
}

/// A UTC offset such as "+02:00" or "-05:30", in seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UtcOffset(pub i64);

impl FromStr for UtcOffset {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        parse_offset(s.trim())
            .map(UtcOffset)
            .ok_or_else(|| format!("'{s}' is not a UTC offset such as +02:00 or -05:30"))
    }
}

/// "+02:00", "-0530", or "Z" in seconds.
pub fn parse_offset(s: &str) -> Option<i64> {
    if s == "Z" {
        return Some(0);
    }
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (hours, minutes): (i64, i64) = (digits[..2].parse().ok()?, digits[2..].parse().ok()?);
    (hours <= 14 && minutes < 60).then_some(sign * (hours * 3600 + minutes * 60))
}

/// A GPX time ("2026-11-14T10:23:45Z", with optional fraction or offset)
/// in seconds since 1970 UTC.
fn parse_utc(s: &str) -> Option<i64> {
    let s = s.trim();
    let number = |range: std::ops::Range<usize>| s.get(range)?.parse::<u16>().ok();
    let taken: Taken = (
        number(0..4)?,
        number(5..7)? as u8,
        number(8..10)? as u8,
        number(11..13)? as u8,
        number(14..16)? as u8,
        number(17..19)? as u8,
    );
    // Skip a fraction of a second, then read the offset.
    let rest = s.get(19..)?.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let offset = if rest.is_empty() { 0 } else { parse_offset(rest)? };
    Some(to_seconds(taken) - offset)
}

/// Degrees as EXIF degrees, minutes, and seconds (hundredths).
pub fn to_dms(value: f64) -> [(u32, u32); 3] {
    let value = value.abs();
    let degrees = value.trunc();
    let minutes = ((value - degrees) * 60.0).trunc();
    let seconds = ((value - degrees) * 60.0 - minutes) * 60.0;
    [(degrees as u32, 1), (minutes as u32, 1), ((seconds * 100.0).round() as u32, 100)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const GPX: &str = r#"<?xml version="1.0"?><gpx><trk><trkseg>
        <trkpt lat="48.8584" lon="2.2945"><ele>35</ele><time>2026-11-14T10:00:00Z</time></trkpt>
        <trkpt lon='2.3000' lat='48.8600'><time>2026-11-14T10:02:00.500Z</time></trkpt>
        <trkpt lat="48.9000" lon="2.4000"><time>2026-11-14T11:00:00+01:00</time></trkpt>
        <trkpt lat="1" lon="1"></trkpt>
        </trkseg></trk></gpx>"#;

    fn utc(h: u8, m: u8, s: u8) -> i64 {
        to_seconds((2026, 11, 14, h, m, s))
    }

    #[test]
    fn parse_reads_points_in_either_attribute_order() {
        let track = Track::parse(GPX).unwrap();
        // The point at 11:00+01:00 is 10:00 UTC; it sorts first (stable).
        assert_eq!(track.points.len(), 3);
        assert!(Track::parse("<gpx></gpx>").is_err());
    }

    #[test]
    fn position_interpolates_and_respects_gaps() {
        let track = Track { points: vec![(utc(10, 0, 0), 48.0, 2.0), (utc(10, 2, 0), 49.0, 3.0), (utc(11, 0, 0), 50.0, 4.0)] };
        assert_eq!(track.position(utc(10, 0, 0)), Some((48.0, 2.0)));
        assert_eq!(track.position(utc(10, 1, 0)), Some((48.5, 2.5)));
        // 10:02 to 11:00 is too far apart to interpolate; near the ends only.
        assert_eq!(track.position(utc(10, 2, 50)), Some((49.0, 3.0)));
        assert_eq!(track.position(utc(10, 30, 0)), None);
        assert_eq!(track.position(utc(9, 59, 30)), Some((48.0, 2.0)));
        assert_eq!(track.position(utc(9, 50, 0)), None);
        assert_eq!(track.position(utc(11, 0, 59)), Some((50.0, 4.0)));
    }

    #[test]
    fn offsets_and_times() {
        assert_eq!(parse_offset("+02:00"), Some(7200));
        assert_eq!(parse_offset("-0530"), Some(-19800));
        assert_eq!(parse_offset("Z"), Some(0));
        for bad in ["2", "+2", "+25:00", "+02:60", "02:00"] {
            assert_eq!(parse_offset(bad), None, "{bad}");
        }
        assert_eq!(parse_utc("2026-11-14T11:00:00+01:00"), Some(utc(10, 0, 0)));
        assert_eq!(parse_utc("2026-11-14T10:00:00.250Z"), Some(utc(10, 0, 0)));
        assert_eq!(parse_utc("yesterday"), None);
    }

    #[test]
    fn dms_rationals() {
        assert_eq!(to_dms(48.8584), [(48, 1), (51, 1), (3024, 100)]);
        assert_eq!(to_dms(-2.5), [(2, 1), (30, 1), (0, 100)]);
    }
}
