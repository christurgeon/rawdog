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
                // The name must start a word: not the "lat" of "flat=".
                let pattern = format!("{name}=");
                let start = point
                    .match_indices(&pattern)
                    .find(|(i, _)| point[..*i].ends_with(|c: char| c.is_whitespace()))?
                    .0
                    + pattern.len();
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

/// The camera clock's UTC offsets for `--gpx-tz`: one offset, or offsets
/// that change at given camera-clock times, as in
/// "+01:00,2026-11-17T14:00=+05:30" for a trip across time zones.
#[derive(Clone, Debug, PartialEq)]
pub struct Zones {
    /// The offset before the first change, if given.
    first: Option<i64>,
    /// (camera-clock seconds since 1970, offset), sorted.
    changes: Vec<(i64, i64)>,
}

impl FromStr for Zones {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let error = |part: &str| {
            format!("'{part}' is not an offset such as +02:00, or a change such as 2026-11-17T14:00=+05:30")
        };
        let mut zones = Zones { first: None, changes: Vec::new() };
        for part in s.split(',').map(str::trim) {
            if part.is_empty() {
                return Err("an empty part; remove the extra comma".to_string());
            }
            match part.split_once('=') {
                None if zones.first.is_none() && zones.changes.is_empty() => {
                    zones.first = Some(parse_offset(part).ok_or_else(|| error(part))?);
                }
                None => return Err(format!("'{part}': only the first offset may be given without a time")),
                Some((when, offset)) => {
                    let when = parse_local(when.trim()).ok_or_else(|| error(part))?;
                    let offset = parse_offset(offset.trim()).ok_or_else(|| error(part))?;
                    zones.changes.push((when, offset));
                }
            }
        }
        zones.changes.sort_unstable();
        if zones.changes.windows(2).any(|w| w[0].0 == w[1].0) {
            return Err("two changes at the same time".to_string());
        }
        Ok(zones)
    }
}

impl Zones {
    /// The offset in force at camera-clock time `local` (seconds).
    pub fn at(&self, local: i64) -> Option<i64> {
        self.changes
            .iter()
            .rev()
            .find(|(when, _)| *when <= local)
            .map(|(_, offset)| *offset)
            .or(self.first)
    }
}

/// A camera-clock time "2026-11-17T14:00" (seconds optional), or a date
/// "2026-11-17" (midnight), in seconds since 1970.
fn parse_local(s: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| s.get(range)?.parse::<u16>().ok();
    let separators_ok = s.get(4..5) == Some("-")
        && s.get(7..8) == Some("-")
        && (s.len() == 10 || (s.get(10..11) == Some("T") && s.get(13..14) == Some(":")))
        && (s.len() != 19 || s.get(16..17) == Some(":"));
    if !separators_ok {
        return None;
    }
    let (y, mo, d) = (number(0..4)?, number(5..7)? as u8, number(8..10)? as u8);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&mo) || d < 1 || d > days[usize::from(mo) - 1] {
        return None;
    }
    let (h, mi, sec) = match s.len() {
        10 => (0, 0, 0),
        16 => (number(11..13)? as u8, number(14..16)? as u8, 0),
        19 => (number(11..13)? as u8, number(14..16)? as u8, number(17..19)? as u8),
        _ => return None,
    };
    ((1..=12).contains(&mo) && (1..=31).contains(&d) && h < 24 && mi < 60 && sec < 60)
        .then(|| to_seconds((y, mo, d, h, mi, sec)))
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
    // Round once, in hundredths of a second, so that no part reaches 60.
    let hundredths = (value.abs() * 360_000.0).round() as u64;
    let (degrees, rest) = (hundredths / 360_000, hundredths % 360_000);
    let (minutes, seconds) = (rest / 6_000, rest % 6_000);
    [(degrees as u32, 1), (minutes as u32, 1), (seconds as u32, 100)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const GPX: &str = r#"<?xml version="1.0"?><gpx><trk><trkseg>
        <trkpt lat="48.8584" lon="2.2945"><ele>35</ele><time>2026-11-14T10:00:00Z</time></trkpt>
        <trkpt lon='2.3000' lat='48.8600'><time>2026-11-14T10:02:00.500Z</time></trkpt>
        <trkpt lat="48.9000" lon="2.4000"><time>2026-11-14T11:00:00+01:00</time></trkpt>
        <trkpt lat="1" lon="1"></trkpt>
        <trkpt flat="9" lat="10" lon="11"><time>2026-11-14T12:00:00Z</time></trkpt>
        </trkseg></trk></gpx>"#;

    fn utc(h: u8, m: u8, s: u8) -> i64 {
        to_seconds((2026, 11, 14, h, m, s))
    }

    #[test]
    fn parse_reads_points_in_either_attribute_order() {
        let track = Track::parse(GPX).unwrap();
        // The point at 11:00+01:00 is 10:00 UTC; it sorts first (stable).
        assert_eq!(track.points.len(), 4);
        assert_eq!(track.position(utc(12, 0, 0)), Some((10.0, 11.0)));
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
    fn zones_change_at_camera_clock_times() {
        let zones: Zones = "+01:00, 2026-11-17T14:00=+05:30".parse().unwrap();
        assert_eq!(zones.at(utc(10, 0, 0)), Some(3600)); // 2026-11-14
        let change = to_seconds((2026, 11, 17, 14, 0, 0));
        assert_eq!(zones.at(change - 1), Some(3600));
        assert_eq!(zones.at(change), Some(19800));
        let only_change: Zones = "2026-11-17=+05:30".parse().unwrap();
        assert_eq!(only_change.at(utc(10, 0, 0)), None);
        assert_eq!("+02:00".parse::<Zones>().unwrap().at(0), Some(7200));
        for bad in [
            "",
            "soon",
            "+01:00,+02:00",
            "2026-13-01=+01:00",
            "2026-11-17T25:00=+01:00",
            "2026x11x17=+01:00",
            "2026-02-31=+01:00",
            "+01:00,",
            "2026-11-17=+01:00,2026-11-17=+02:00",
        ] {
            assert!(bad.parse::<Zones>().is_err(), "{bad}");
        }
    }

    #[test]
    fn dms_rationals() {
        assert_eq!(to_dms(48.8584), [(48, 1), (51, 1), (3024, 100)]);
        assert_eq!(to_dms(-2.5), [(2, 1), (30, 1), (0, 100)]);
        assert_eq!(to_dms(2.3), [(2, 1), (18, 1), (0, 100)]);
        for v in [0.1, 2.3, 48.85, 179.99999, 33.75] {
            let [_, (m, _), (s, _)] = to_dms(v);
            assert!(m < 60 && s < 6_000, "{v}");
        }
    }
}
