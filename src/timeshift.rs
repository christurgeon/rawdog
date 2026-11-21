//! `--time-shift`: correct a camera clock that was set wrong.

use std::str::FromStr;

/// Capture time as (year, month, day, hour, minute, second).
pub type Taken = (u16, u8, u8, u8, u8, u8);

/// Offsets for the capture time, for all cameras or per camera model.
#[derive(Clone, Debug, PartialEq)]
pub struct TimeShift {
    /// (model text in lower case, offset in seconds). A rule without model
    /// text applies to cameras that no other rule names.
    rules: Vec<(Option<String>, i64)>,
}

impl FromStr for TimeShift {
    type Err = String;

    /// `+1h`, `-30m`, `+3m12s`, `+1d2h`, or per camera
    /// `7M4=+3m12s,Z 6=-1h`, with an optional rule for the other cameras.
    fn from_str(s: &str) -> Result<Self, String> {
        let mut rules = Vec::new();
        for rule in s.split(',') {
            let (model, offset) = match rule.rsplit_once('=') {
                Some((model, offset)) => {
                    let model = model.trim();
                    if model.is_empty() {
                        return Err(format!("'{rule}' has no camera model before '='"));
                    }
                    (Some(model.to_lowercase()), offset)
                }
                None => (None, rule),
            };
            if rules.iter().any(|(m, _)| *m == model) {
                return Err(match model {
                    Some(m) => format!("camera '{m}' is given twice"),
                    None => "more than one offset without a camera model".to_string(),
                });
            }
            rules.push((model, parse_offset(offset.trim())?));
        }
        Ok(TimeShift { rules })
    }
}

/// An offset such as `+1h`, `-3m12s`, or `2d`, in seconds.
fn parse_offset(s: &str) -> Result<i64, String> {
    let error = || format!("'{s}' is not an offset such as +1h, -30m, or +3m12s");
    let (sign, mut rest) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    if rest.is_empty() {
        return Err(error());
    }
    let mut total: i64 = 0;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit()).ok_or_else(error)?;
        let number: i64 = rest[..digits].parse().map_err(|_| error())?;
        let unit = match rest[digits..].chars().next() {
            Some('d') => 86_400,
            Some('h') => 3_600,
            Some('m') => 60,
            Some('s') => 1,
            _ => return Err(error()),
        };
        total = number
            .checked_mul(unit)
            .and_then(|v| total.checked_add(v))
            .filter(|v| *v <= 366 * 86_400 * 100)
            .ok_or_else(error)?;
        rest = &rest[digits + 1..];
    }
    Ok(sign * total)
}

impl TimeShift {
    /// The offset in seconds for a camera model: the first rule whose text
    /// the model contains (not case-sensitive), or else the rule without a
    /// model.
    pub fn seconds_for(&self, model: Option<&str>) -> i64 {
        let model = model.unwrap_or_default().to_lowercase();
        self.rules
            .iter()
            .find(|(m, _)| m.as_ref().is_some_and(|m| model.contains(m.as_str())))
            .or_else(|| self.rules.iter().find(|(m, _)| m.is_none()))
            .map_or(0, |(_, seconds)| *seconds)
    }

    /// `taken` corrected for a camera model.
    pub fn apply(&self, taken: Taken, model: Option<&str>) -> Taken {
        add_seconds(taken, self.seconds_for(model))
    }
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a number of days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

/// Seconds since 1970-01-01 of a capture time, for measuring gaps.
pub fn to_seconds((y, mo, d, h, mi, s): Taken) -> i64 {
    days_from_civil(y.into(), mo.into(), d.into()) * 86_400
        + i64::from(h) * 3_600
        + i64::from(mi) * 60
        + i64::from(s)
}

/// `taken` moved by `seconds`. A time outside the years 1 to 9999 is left
/// unchanged.
pub fn add_seconds(taken: Taken, seconds: i64) -> Taken {
    if seconds == 0 {
        return taken;
    }
    let (y, mo, d, h, mi, s) = taken;
    let total = days_from_civil(y.into(), mo.into(), d.into()) * 86_400
        + i64::from(h) * 3_600
        + i64::from(mi) * 60
        + i64::from(s)
        + seconds;
    let (days, secs) = (total.div_euclid(86_400), total.rem_euclid(86_400));
    let (y, mo, d) = civil_from_days(days);
    if !(1..=9999).contains(&y) {
        return taken;
    }
    (
        y as u16,
        mo as u8,
        d as u8,
        (secs / 3_600) as u8,
        (secs / 60 % 60) as u8,
        (secs % 60) as u8,
    )
}

/// The current year (UTC).
pub fn current_year() -> u16 {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    add_seconds((1970, 1, 1, 0, 0, 0), seconds).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_offsets() {
        assert_eq!(parse_offset("+1h"), Ok(3_600));
        assert_eq!(parse_offset("-30m"), Ok(-1_800));
        assert_eq!(parse_offset("3m12s"), Ok(192));
        assert_eq!(parse_offset("+1d2h"), Ok(93_600));
        for bad in ["", "+", "1", "1x", "h", "1h2", "--1h", "1.5h", "99999999999d"] {
            assert!(parse_offset(bad).is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn rules_for_all_cameras_and_per_camera() {
        let shift: TimeShift = "+1h".parse().unwrap();
        assert_eq!(shift.seconds_for(Some("ILCE-7M4")), 3_600);
        assert_eq!(shift.seconds_for(None), 3_600);

        let shift: TimeShift = "7m4=+3m12s, Z 6=-1h".parse().unwrap();
        assert_eq!(shift.seconds_for(Some("ILCE-7M4")), 192);
        assert_eq!(shift.seconds_for(Some("NIKON Z 6_2")), -3_600);
        assert_eq!(shift.seconds_for(Some("X-T5")), 0);
        assert_eq!(shift.seconds_for(None), 0);

        let shift: TimeShift = "7M4=-2m,+1h".parse().unwrap();
        assert_eq!(shift.seconds_for(Some("ILCE-7M4")), -120);
        assert_eq!(shift.seconds_for(Some("X-T5")), 3_600);
    }

    #[test]
    fn rejects_unclear_rules() {
        for bad in ["=+1h", "7M4=+1h,7m4=+2h", "+1h,-1h", "7M4=", "7M4"] {
            assert!(bad.parse::<TimeShift>().is_err(), "{bad} was accepted");
        }
    }

    #[test]
    fn current_year_is_plausible() {
        assert!((2026..2100).contains(&current_year()));
    }

    #[test]
    fn add_seconds_crosses_days_months_and_years() {
        assert_eq!(add_seconds((2026, 11, 14, 23, 30, 0), 3_600), (2026, 11, 15, 0, 30, 0));
        assert_eq!(add_seconds((2026, 1, 1, 0, 0, 5), -10), (2025, 12, 31, 23, 59, 55));
        assert_eq!(add_seconds((2024, 2, 28, 12, 0, 0), 86_400), (2024, 2, 29, 12, 0, 0));
        assert_eq!(add_seconds((2023, 2, 28, 12, 0, 0), 86_400), (2023, 3, 1, 12, 0, 0));
        assert_eq!(add_seconds((2026, 3, 9, 7, 5, 42), 0), (2026, 3, 9, 7, 5, 42));
        assert_eq!(add_seconds((9999, 12, 31, 23, 0, 0), 7_200), (9999, 12, 31, 23, 0, 0));
    }
}
