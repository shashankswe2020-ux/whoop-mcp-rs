//! JavaScript-compatible primitives.
//!
//! The TypeScript implementation relies on `Date`, `Math.round`, `JSON.stringify`,
//! `URLSearchParams`, and `encodeURIComponent`. These helpers reproduce the exact
//! observable behavior so that tool outputs match byte-for-byte.

use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use serde_json::{Map, Number, Value};

/// Milliseconds in one day.
pub const DAY_MS: f64 = 86_400_000.0;
/// Milliseconds in one hour.
pub const HOUR_MS: f64 = 3_600_000.0;
const MAX_TIME_MS: f64 = 8.64e15;

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

/// `Math.round`: round half toward positive infinity.
pub fn round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// Round to one decimal place exactly like `Math.round(x * 10) / 10`.
pub fn round1(x: f64) -> f64 {
    round(x * 10.0) / 10.0
}

/// `Math.min(...values)` (NaN-propagating; `Infinity` when empty).
pub fn min(values: &[f64]) -> f64 {
    values.iter().fold(f64::INFINITY, |acc, &x| {
        if x.is_nan() || acc.is_nan() {
            f64::NAN
        } else {
            acc.min(x)
        }
    })
}

/// `Math.max(...values)` (NaN-propagating; `-Infinity` when empty).
pub fn max(values: &[f64]) -> f64 {
    values.iter().fold(f64::NEG_INFINITY, |acc, &x| {
        if x.is_nan() || acc.is_nan() {
            f64::NAN
        } else {
            acc.max(x)
        }
    })
}

/// Convert an `f64` into a JSON value the way `JSON.stringify` would render it.
/// Non-finite values become `null`; integral values serialize without a fraction.
pub fn num(x: f64) -> Value {
    if !x.is_finite() {
        return Value::Null;
    }
    if x == x.trunc() && x.abs() < 9.007_199_254_740_992e15 {
        return Value::from(x as i64);
    }
    Number::from_f64(x).map_or(Value::Null, Value::Number)
}

/// JSON value for an optional number (`null` when absent or non-finite).
pub fn opt_num(x: Option<f64>) -> Value {
    x.map_or(Value::Null, num)
}

/// `String(x)` for numbers.
pub fn number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if x == 0.0 {
        return "0".into();
    }
    if x == x.trunc() && x.abs() < 1e21 {
        return format!("{x:.0}");
    }
    let abs = x.abs();
    if (1e-6..1e21).contains(&abs) {
        return format!("{x}");
    }
    // Exponential form, e.g. 1e+21 / 1.5e-7.
    let formatted = format!("{x:e}");
    match formatted.split_once('e') {
        Some((mantissa, exp)) if !exp.starts_with('-') => format!("{mantissa}e+{exp}"),
        _ => formatted,
    }
}

/// Recursively normalize numbers so serialization matches `JSON.stringify`
/// (integral floats print without `.0`, `-0` prints as `0`).
pub fn normalize(value: &mut Value) {
    match value {
        Value::Number(n) => {
            if let Some(f) = n.as_f64()
                && !n.is_i64()
                && !n.is_u64()
            {
                *value = num(f);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize),
        Value::Object(map) => map.values_mut().for_each(normalize),
        _ => {}
    }
}

/// `JSON.stringify(value, null, 2)`.
pub fn stringify_pretty(value: &Value) -> String {
    let mut normalized = value.clone();
    normalize(&mut normalized);
    serde_json::to_string_pretty(&normalized).unwrap_or_else(|_| "null".into())
}

/// `JSON.stringify(value)`.
pub fn stringify(value: &Value) -> String {
    let mut normalized = value.clone();
    normalize(&mut normalized);
    serde_json::to_string(&normalized).unwrap_or_else(|_| "null".into())
}

/// Numeric view of a JSON value, mirroring the TypeScript casts where a missing
/// or non-numeric field becomes `NaN`.
pub fn f(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).unwrap_or(f64::NAN)
}

/// Truthiness of a JSON value (`Boolean(value)`).
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// Build an object from key/value pairs preserving insertion order.
pub fn obj<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

/// String value helper.
pub fn s(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("")
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn time_clip(ms: f64) -> f64 {
    if !ms.is_finite() || ms.abs() > MAX_TIME_MS {
        f64::NAN
    } else {
        ms.trunc() + 0.0
    }
}

/// `Date.UTC(year, month0, day, hours, minutes, seconds, ms)` with JS overflow rules.
pub fn date_utc(year: f64, month0: f64, day: f64, h: f64, mi: f64, sec: f64, ms: f64) -> f64 {
    let parts = [year, month0, day, h, mi, sec, ms];
    if parts.iter().any(|p| !p.is_finite()) {
        return f64::NAN;
    }
    let ym = year.trunc() + (month0.trunc() / 12.0).floor();
    let mn = month0.trunc().rem_euclid(12.0);
    if ym.abs() > 400_000.0 {
        return f64::NAN;
    }
    let days = days_from_civil(ym as i64, mn as i64 + 1, 1) as f64 + day.trunc() - 1.0;
    let time = h.trunc() * HOUR_MS + mi.trunc() * 60_000.0 + sec.trunc() * 1000.0 + ms.trunc();
    time_clip(days * DAY_MS + time)
}

/// Shorthand for `Date.UTC(year, month0, day)`.
pub fn date_utc_ymd(year: f64, month0: f64, day: f64) -> f64 {
    date_utc(year, month0, day, 0.0, 0.0, 0.0, 0.0)
}

/// UTC calendar fields of a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcParts {
    pub year: i64,
    /// Zero-based month (0 = January), like `getUTCMonth()`.
    pub month0: i64,
    pub day: i64,
    pub hour: i64,
    pub minute: i64,
    pub second: i64,
    pub millis: i64,
    /// 0 = Sunday, like `getUTCDay()`.
    pub weekday: i64,
}

/// Break a timestamp into UTC fields; `None` for an invalid date.
pub fn utc_parts(ms: f64) -> Option<UtcParts> {
    if ms.is_nan() {
        return None;
    }
    let ms = ms as i64;
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    Some(UtcParts {
        year,
        month0: month - 1,
        day,
        hour: rem / 3_600_000,
        minute: rem % 3_600_000 / 60_000,
        second: rem % 60_000 / 1000,
        millis: rem % 1000,
        weekday: (days + 4).rem_euclid(7),
    })
}

/// Error raised by `Date.prototype.toISOString` for invalid dates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTime;

impl std::fmt::Display for InvalidTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid time value")
    }
}

impl std::error::Error for InvalidTime {}

/// `new Date(ms).toISOString()`.
pub fn to_iso(ms: f64) -> Result<String, InvalidTime> {
    let p = utc_parts(time_clip(ms)).ok_or(InvalidTime)?;
    let year = if (0..=9999).contains(&p.year) {
        format!("{:04}", p.year)
    } else if p.year < 0 {
        format!("-{:06}", -p.year)
    } else {
        format!("+{:06}", p.year)
    };
    Ok(format!(
        "{year}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        p.month0 + 1,
        p.day,
        p.hour,
        p.minute,
        p.second,
        p.millis
    ))
}

/// `YYYY-MM-DD` of a timestamp in UTC.
pub fn format_ymd(ms: f64) -> Result<String, InvalidTime> {
    to_iso(ms).map(|iso| iso[..iso.len() - 14].to_string())
}

/// Start of the UTC day containing `ms`.
pub fn start_of_utc_day(ms: f64) -> f64 {
    match utc_parts(ms) {
        Some(p) => date_utc_ymd(p.year as f64, p.month0 as f64, p.day as f64),
        None => f64::NAN,
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn digits(&mut self, count: usize) -> Option<i64> {
        let end = self.pos + count;
        let slice = self.bytes.get(self.pos..end)?;
        if !slice.iter().all(u8::is_ascii_digit) {
            return None;
        }
        self.pos = end;
        Some(
            slice
                .iter()
                .fold(0, |acc, b| acc * 10 + i64::from(b - b'0')),
        )
    }

    fn done(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

/// `Date.parse` for the ISO-8601 shapes accepted by V8.
///
/// Date-only forms are UTC; date-time forms without an offset use local time.
/// Unrecognized input returns `NaN`.
pub fn parse(input: &str) -> f64 {
    parse_inner(input).unwrap_or(f64::NAN)
}

fn parse_inner(input: &str) -> Option<f64> {
    let mut c = Cursor {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let year = match c.peek()? {
        b'+' | b'-' => {
            let negative = c.peek() == Some(b'-');
            c.pos += 1;
            let y = c.digits(6)?;
            if negative && y == 0 {
                return None;
            }
            if negative { -y } else { y }
        }
        _ => c.digits(4)?,
    };
    let mut month = 1;
    let mut day = 1;
    if c.eat(b'-') {
        month = c.digits(2)?;
        if c.eat(b'-') {
            day = c.digits(2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let date_days = days_from_civil(year, month, 1) + day - 1;
    if c.done() {
        return Some(time_clip(date_days as f64 * DAY_MS));
    }
    if !(c.eat(b'T') || c.eat(b't') || c.eat(b' ')) {
        return None;
    }
    let hour = c.digits(2)?;
    if !c.eat(b':') {
        return None;
    }
    let minute = c.digits(2)?;
    let mut second = 0;
    let mut millis = 0;
    if c.eat(b':') {
        second = c.digits(2)?;
        if c.eat(b'.') {
            let start = c.pos;
            while c.peek().is_some_and(|b| b.is_ascii_digit()) {
                c.pos += 1;
            }
            let frac = &input[start..c.pos];
            if frac.is_empty() {
                return None;
            }
            let padded = format!("{:0<3}", &frac[..frac.len().min(3)]);
            millis = padded.parse().ok()?;
        }
    }
    let valid_clock = (hour < 24 && minute < 60 && second < 60)
        || (hour == 24 && minute == 0 && second == 0 && millis == 0);
    if !valid_clock {
        return None;
    }
    let local_ms = date_days as f64 * DAY_MS
        + (hour * 3_600_000 + minute * 60_000 + second * 1000 + millis) as f64;
    let offset_ms = if c.done() {
        return local_to_utc(local_ms);
    } else if c.eat(b'Z') || c.eat(b'z') {
        0
    } else {
        let sign = match c.peek()? {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        c.pos += 1;
        let oh = c.digits(2)?;
        c.eat(b':');
        let om = c.digits(2)?;
        if oh > 23 || om > 59 {
            return None;
        }
        sign * (oh * 60 + om) * 60_000
    };
    if !c.done() {
        return None;
    }
    Some(time_clip(local_ms - offset_ms as f64))
}

fn local_to_utc(local_ms: f64) -> Option<f64> {
    // Normalize via the UTC value so overflowing days (e.g. Feb 30) behave like V8.
    let p = utc_parts(local_ms)?;
    let date = NaiveDate::from_ymd_opt(
        i32::try_from(p.year).ok()?,
        p.month0 as u32 + 1,
        p.day as u32,
    )?;
    let naive: NaiveDateTime = date.and_hms_milli_opt(
        p.hour as u32,
        p.minute as u32,
        p.second as u32,
        p.millis as u32,
    )?;
    let offset = match Local.offset_from_local_datetime(&naive) {
        chrono::LocalResult::Single(o) => o,
        chrono::LocalResult::Ambiguous(earliest, _) => earliest,
        chrono::LocalResult::None => Local.offset_from_utc_datetime(&naive),
    };
    let offset_ms = f64::from(offset.local_minus_utc()) * 1000.0;
    Some(time_clip(local_ms - offset_ms))
}

// ---------------------------------------------------------------------------
// URL encoding
// ---------------------------------------------------------------------------

fn percent_encode(input: &str, keep: impl Fn(u8) -> bool, space_plus: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        if keep(byte) {
            out.push(byte as char);
        } else if space_plus && byte == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `application/x-www-form-urlencoded` byte serializer used by `URLSearchParams`.
pub fn form_encode(input: &str) -> String {
    percent_encode(
        input,
        |b| b.is_ascii_alphanumeric() || b"*-._".contains(&b),
        true,
    )
}

/// `encodeURIComponent`.
pub fn encode_uri_component(input: &str) -> String {
    percent_encode(
        input,
        |b| b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b),
        false,
    )
}

/// Serialize ordered key/value pairs like `URLSearchParams.toString()`.
pub fn search_params(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Decode a form-urlencoded component (`+` is a space).
pub fn form_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a query string into ordered pairs like `new URLSearchParams(query)`.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .trim_start_matches('?')
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) => (form_decode(k), form_decode(v)),
            None => (form_decode(part), String::new()),
        })
        .collect()
}

/// Sort pairs by key using UTF-16 code unit order (stable), like `URLSearchParams.sort()`.
pub fn sort_pairs(pairs: &mut [(String, String)]) {
    pairs.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
}

/// Length of a string in UTF-16 code units (`string.length`).
pub fn js_len(input: &str) -> usize {
    input.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_math_round() {
        assert_eq!(round(2.5), 3.0);
        assert_eq!(round(-2.5), -2.0);
        assert_eq!(round(0.499_999_999_999_999_94), 0.0);
        assert_eq!(round1(7.25), 7.3);
    }

    #[test]
    fn numbers_render_like_javascript() {
        assert_eq!(number_to_string(67.0), "67");
        assert_eq!(number_to_string(6.5), "6.5");
        assert_eq!(number_to_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(number_to_string(1e21), "1e+21");
        assert_eq!(number_to_string(1.5e-7), "1.5e-7");
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(num(67.0), Value::from(67));
        assert_eq!(num(f64::NAN), Value::Null);
        assert_eq!(
            stringify(&serde_json::json!({"a": 1.0, "b": -0.0})),
            r#"{"a":1,"b":0}"#
        );
    }

    #[test]
    fn parses_iso_shapes_like_v8() {
        assert_eq!(parse("2024-02-30"), 1_709_251_200_000.0);
        assert_eq!(parse("2024-01-01T24:00:00Z"), 1_704_153_600_000.0);
        assert!(parse("2024-01-01T24:00:01Z").is_nan());
        assert_eq!(parse("2024-01-01T10:00:00.123456Z"), 1_704_103_200_123.0);
        assert_eq!(parse("2024-01-01T10:00Z"), 1_704_103_200_000.0);
        assert!(parse("2024-13-01").is_nan());
        assert!(parse("2024-01-00").is_nan());
        assert!(parse("2024-01-32").is_nan());
        assert_eq!(parse("2024-01-01T10:00:00+05:30"), 1_704_083_400_000.0);
        assert_eq!(parse("2024-01-01T10:00:00.5+01:00"), 1_704_099_600_500.0);
        assert_eq!(parse("2024-01-01t10:00:00z"), 1_704_103_200_000.0);
        assert_eq!(parse("2024"), 1_704_067_200_000.0);
        assert_eq!(parse("2024-05"), 1_714_521_600_000.0);
        assert_eq!(parse("+002024-01-01T00:00:00Z"), 1_704_067_200_000.0);
        assert!(parse("2024-01-01T10:00:60Z").is_nan());
        assert!(parse("2024-01-01T10:00:00.Z").is_nan());
        assert!(parse("garbage").is_nan());
    }

    #[test]
    fn date_utc_normalizes_overflow() {
        assert_eq!(
            to_iso(date_utc_ymd(2024.0, 0.0, -9.0)).unwrap(),
            "2023-12-22T00:00:00.000Z"
        );
        assert_eq!(date_utc_ymd(2024.0, 14.0, 1.0), 1_740_787_200_000.0);
        assert_eq!(to_iso(-1.0).unwrap(), "1969-12-31T23:59:59.999Z");
        assert!(to_iso(f64::NAN).is_err());
        assert_eq!(utc_parts(0.0).unwrap().weekday, 4);
    }

    #[test]
    fn url_encoding_matches_web_apis() {
        assert_eq!(
            search_params(&[("start", "2024-01-01T00:00:00.000Z"), ("q", "a b")]),
            "start=2024-01-01T00%3A00%3A00.000Z&q=a+b"
        );
        assert_eq!(encode_uri_component("a b/c~"), "a%20b%2Fc~");
        assert_eq!(
            parse_query("?b=2&a=1+2&c=%41"),
            vec![
                ("b".into(), "2".into()),
                ("a".into(), "1 2".into()),
                ("c".into(), "A".into())
            ]
        );
    }
}
