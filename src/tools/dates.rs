//! Relative date expressions ("today", "last 7 days", ...) resolved to ISO
//! 8601 ranges in UTC, plus range validation and collection query building.

use crate::js::{self, date_utc, date_utc_ymd, to_iso, utc_parts};

/// Error for unrecognized or out-of-range date expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDateExpression(pub String);

impl std::fmt::Display for InvalidDateExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidDateExpression {}

/// Resolved ISO 8601 range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateRange {
    pub start: String,
    pub end: String,
}

const MAX_LAST_N_DAYS: f64 = 365.0;
const MAX_LAST_N_WEEKS: f64 = 52.0;
const MAX_LAST_N_MONTHS: f64 = 12.0;

fn err(message: String) -> InvalidDateExpression {
    InvalidDateExpression(message)
}

fn start_of_day(ms: f64) -> String {
    let p = utc_parts(ms).expect("valid date");
    to_iso(date_utc_ymd(p.year as f64, p.month0 as f64, p.day as f64)).unwrap_or_default()
}

fn end_of_day(ms: f64) -> String {
    let p = utc_parts(ms).expect("valid date");
    to_iso(date_utc(
        p.year as f64,
        p.month0 as f64,
        p.day as f64,
        23.0,
        59.0,
        59.0,
        999.0,
    ))
    .unwrap_or_default()
}

/// Monday (UTC midnight) of the ISO week containing `ms`.
pub fn monday_utc(ms: f64) -> f64 {
    let p = utc_parts(ms).expect("valid date");
    let diff = if p.weekday == 0 { 6 } else { p.weekday - 1 };
    date_utc_ymd(p.year as f64, p.month0 as f64, (p.day - diff) as f64)
}

fn last_day_of_month(year: f64, month0: f64) -> f64 {
    date_utc_ymd(year, month0 + 1.0, 0.0)
}

/// Match `/^last\s+(\d+)\s+<unit>s?$/` on a lowercased expression.
fn match_last_n(lower: &str, unit: &str) -> Option<f64> {
    let rest = lower.strip_prefix("last")?;
    let trimmed = rest.trim_start();
    if trimmed.len() == rest.len() {
        return None;
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let (number, rest) = trimmed.split_at(digits);
    let tail = rest.trim_start();
    if tail.len() == rest.len() {
        return None;
    }
    let tail = tail.strip_prefix(unit)?;
    if !(tail.is_empty() || tail == "s") {
        return None;
    }
    number.parse::<f64>().ok()
}

/// Match `/^<first>\s+<second>$/`.
fn match_words(lower: &str, first: &str, second: &str) -> bool {
    lower
        .strip_prefix(first)
        .and_then(|rest| {
            let trimmed = rest.trim_start();
            (trimmed.len() != rest.len()).then_some(trimmed)
        })
        .is_some_and(|rest| rest == second)
}

fn check_count(n: f64, max: f64, unit: &str, plural: &str) -> Result<(), InvalidDateExpression> {
    let text = js::number_to_string(n);
    if n <= 0.0 {
        return Err(err(format!(
            "Invalid {unit} count: {text}. Must be between 1 and {}.",
            js::number_to_string(max)
        )));
    }
    if n > max {
        let capital = format!("{}{}", unit[..1].to_uppercase(), &unit[1..]);
        return Err(err(format!(
            "{capital} count {text} exceeds maximum of {} {plural}.",
            js::number_to_string(max)
        )));
    }
    Ok(())
}

/// Resolve a date expression relative to `now` (epoch ms).
pub fn resolve_date_expression(
    expression: &str,
    now: f64,
) -> Result<DateRange, InvalidDateExpression> {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        return Err(err("Unrecognized date expression: empty string".into()));
    }
    if crate::schema::is_iso_8601(trimmed) {
        return Ok(DateRange {
            start: trimmed.into(),
            end: trimmed.into(),
        });
    }
    let lower = trimmed.to_lowercase();
    let p = utc_parts(now).expect("valid now");
    let (y, m, d) = (p.year as f64, p.month0 as f64, p.day as f64);
    let range = |start: f64, end: f64| DateRange {
        start: start_of_day(start),
        end: end_of_day(end),
    };

    if lower == "today" {
        return Ok(range(now, now));
    }
    if lower == "yesterday" {
        let yesterday = date_utc_ymd(y, m, d - 1.0);
        return Ok(range(yesterday, yesterday));
    }
    if let Some(n) = match_last_n(&lower, "day") {
        check_count(n, MAX_LAST_N_DAYS, "day", "days")?;
        return Ok(range(date_utc_ymd(y, m, d - n), now));
    }
    if lower == "this week" {
        return Ok(range(monday_utc(now), now));
    }
    if lower == "last week" {
        let this_monday = monday_utc(now);
        return Ok(range(
            this_monday - 7.0 * js::DAY_MS,
            this_monday - js::DAY_MS,
        ));
    }
    if lower == "this month" {
        return Ok(range(date_utc_ymd(y, m, 1.0), now));
    }
    if lower == "last month" {
        let (year, month) = if m == 0.0 {
            (y - 1.0, 11.0)
        } else {
            (y, m - 1.0)
        };
        return Ok(range(
            date_utc_ymd(year, month, 1.0),
            last_day_of_month(year, month),
        ));
    }
    if let Some(n) = match_last_n(&lower, "week") {
        check_count(n, MAX_LAST_N_WEEKS, "week", "weeks")?;
        return Ok(range(date_utc_ymd(y, m, d - n * 7.0), now));
    }
    if let Some(n) = match_last_n(&lower, "month") {
        check_count(n, MAX_LAST_N_MONTHS, "month", "months")?;
        let target_month = m - n;
        let last_day =
            utc_parts(date_utc_ymd(y, target_month + 1.0, 0.0)).map_or(28, |p| p.day) as f64;
        let start = date_utc_ymd(y, target_month, d.min(last_day));
        return Ok(range(start, now));
    }
    if match_words(&lower, "this", "quarter") {
        let quarter_start = (m / 3.0).floor() * 3.0;
        return Ok(range(date_utc_ymd(y, quarter_start, 1.0), now));
    }
    if match_words(&lower, "last", "quarter") {
        let current = (m / 3.0).floor();
        let (start_month, year) = if current == 0.0 {
            (9.0, y - 1.0)
        } else {
            ((current - 1.0) * 3.0, y)
        };
        return Ok(range(
            date_utc_ymd(year, start_month, 1.0),
            last_day_of_month(year, start_month + 2.0),
        ));
    }
    if match_words(&lower, "last", "year") {
        return Ok(range(
            date_utc_ymd(y - 1.0, 0.0, 1.0),
            date_utc_ymd(y - 1.0, 11.0, 31.0),
        ));
    }
    if let Some((year, month)) = month_literal(trimmed) {
        if !(2010.0..=2099.0).contains(&year) {
            return Err(err(format!(
                "Year {} is outside supported range (2010-2099).",
                js::number_to_string(year)
            )));
        }
        let month0 = month - 1.0;
        return Ok(range(
            date_utc_ymd(year, month0, 1.0),
            last_day_of_month(year, month0),
        ));
    }
    Err(err(format!(
        "Unrecognized date expression: \"{trimmed}\". Supported: \"today\", \"yesterday\", \"last N days\", \"last N weeks\", \"last N months\", \"this week\", \"last week\", \"this month\", \"last month\", \"this quarter\", \"last quarter\", \"last year\", \"YYYY-MM\", or ISO 8601."
    )))
}

/// Match `/^(\d{4})-(0[1-9]|1[0-2])$/`.
fn month_literal(text: &str) -> Option<(f64, f64)> {
    let b = text.as_bytes();
    if b.len() != 7
        || b[4] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || c.is_ascii_digit())
    {
        return None;
    }
    let month: f64 = text[5..7].parse().ok()?;
    (1.0..=12.0)
        .contains(&month)
        .then(|| (text[0..4].parse().unwrap_or(0.0), month))
}

/// Validate an ISO range: parseable, ordered, and at most `max_days` long.
pub fn validate_date_range(
    start: &str,
    end: &str,
    max_days: f64,
) -> Result<(), InvalidDateExpression> {
    let start_ms = js::parse(start);
    let end_ms = js::parse(end);
    if start_ms.is_nan() || end_ms.is_nan() {
        return Err(err(format!(
            "Invalid date string: start=\"{start}\", end=\"{end}\". Expected ISO 8601 format."
        )));
    }
    if end_ms < start_ms {
        return Err(err(format!(
            "End date is before start date: {end} < {start}"
        )));
    }
    let diff_days = (end_ms - start_ms) / js::DAY_MS;
    if diff_days > max_days {
        return Err(err(format!(
            "Date range of {} days exceeds maximum of {} days.",
            js::number_to_string(diff_days.ceil()),
            js::number_to_string(max_days)
        )));
    }
    Ok(())
}

/// Collection query parameters shared by the four collection tools.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CollectionParams {
    pub start: Option<String>,
    pub end: Option<String>,
    pub limit: Option<f64>,
    pub next_token: Option<String>,
}

/// Build `?start=..&end=..&limit=..&nextToken=..`, resolving date expressions.
pub fn build_collection_query(
    params: &CollectionParams,
    now: f64,
) -> Result<String, InvalidDateExpression> {
    let start = params
        .start
        .as_deref()
        .map(|v| resolve_date_expression(v, now).map(|r| r.start))
        .transpose()?;
    let end = params
        .end
        .as_deref()
        .map(|v| resolve_date_expression(v, now).map(|r| r.end))
        .transpose()?;
    let limit = params.limit.map(js::number_to_string);
    let mut pairs: Vec<(&str, &str)> = Vec::new();
    if let Some(start) = &start {
        pairs.push(("start", start));
    }
    if let Some(end) = &end {
        pairs.push(("end", end));
    }
    if let Some(limit) = &limit {
        pairs.push(("limit", limit));
    }
    if let Some(token) = &params.next_token {
        pairs.push(("nextToken", token));
    }
    let query = js::search_params(&pairs);
    Ok(if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Wednesday 2026-09-30T12:34:56.789Z
    const NOW: f64 = 1_790_771_696_789.0;

    fn resolve(expr: &str) -> (String, String) {
        let r = resolve_date_expression(expr, NOW).unwrap();
        (r.start, r.end)
    }

    #[test]
    fn resolves_relative_expressions() {
        assert_eq!(
            resolve("today"),
            (
                "2026-09-30T00:00:00.000Z".into(),
                "2026-09-30T23:59:59.999Z".into()
            )
        );
        assert_eq!(
            resolve(" Yesterday "),
            (
                "2026-09-29T00:00:00.000Z".into(),
                "2026-09-29T23:59:59.999Z".into()
            )
        );
        assert_eq!(resolve("last 7 days").0, "2026-09-23T00:00:00.000Z");
        assert_eq!(resolve("LAST  1 DAY").0, "2026-09-29T00:00:00.000Z");
        assert_eq!(resolve("this week").0, "2026-09-28T00:00:00.000Z");
        assert_eq!(
            resolve("last week"),
            (
                "2026-09-21T00:00:00.000Z".into(),
                "2026-09-27T23:59:59.999Z".into()
            )
        );
        assert_eq!(resolve("this month").0, "2026-09-01T00:00:00.000Z");
        assert_eq!(
            resolve("last month"),
            (
                "2026-08-01T00:00:00.000Z".into(),
                "2026-08-31T23:59:59.999Z".into()
            )
        );
        assert_eq!(resolve("last 2 weeks").0, "2026-09-16T00:00:00.000Z");
        assert_eq!(resolve("last 7 months").0, "2026-02-28T00:00:00.000Z");
        assert_eq!(resolve("this quarter").0, "2026-07-01T00:00:00.000Z");
        assert_eq!(
            resolve("last quarter"),
            (
                "2026-04-01T00:00:00.000Z".into(),
                "2026-06-30T23:59:59.999Z".into()
            )
        );
        assert_eq!(
            resolve("last year"),
            (
                "2025-01-01T00:00:00.000Z".into(),
                "2025-12-31T23:59:59.999Z".into()
            )
        );
        assert_eq!(
            resolve("2024-02"),
            (
                "2024-02-01T00:00:00.000Z".into(),
                "2024-02-29T23:59:59.999Z".into()
            )
        );
        assert_eq!(
            resolve("2024-05-01T10:00:00Z"),
            ("2024-05-01T10:00:00Z".into(), "2024-05-01T10:00:00Z".into())
        );
    }

    #[test]
    fn rejects_invalid_expressions() {
        let message = |e: &str| resolve_date_expression(e, NOW).unwrap_err().0;
        assert_eq!(
            message("last 0 days"),
            "Invalid day count: 0. Must be between 1 and 365."
        );
        assert_eq!(
            message("last 400 days"),
            "Day count 400 exceeds maximum of 365 days."
        );
        assert_eq!(
            message("last 53 weeks"),
            "Week count 53 exceeds maximum of 52 weeks."
        );
        assert_eq!(
            message("last 13 months"),
            "Month count 13 exceeds maximum of 12 months."
        );
        assert_eq!(
            message("2009-01"),
            "Year 2009 is outside supported range (2010-2099)."
        );
        assert_eq!(message(""), "Unrecognized date expression: empty string");
        assert!(
            message("next tuesday").starts_with("Unrecognized date expression: \"next tuesday\".")
        );
    }

    #[test]
    fn validates_ranges() {
        assert!(validate_date_range("2024-01-01", "2024-03-01", 90.0).is_ok());
        assert_eq!(
            validate_date_range("2024-01-01", "2024-06-01", 90.0)
                .unwrap_err()
                .0,
            "Date range of 152 days exceeds maximum of 90 days."
        );
        assert!(validate_date_range("2024-02-01", "2024-01-01", 90.0).is_err());
        assert!(
            validate_date_range("2024-13-45", "2024-01-01", 90.0)
                .unwrap_err()
                .0
                .starts_with("Invalid date string")
        );
    }

    #[test]
    fn builds_collection_queries() {
        let params = CollectionParams {
            start: Some("today".into()),
            limit: Some(5.0),
            next_token: Some("a b".into()),
            ..Default::default()
        };
        assert_eq!(
            build_collection_query(&params, NOW).unwrap(),
            "?start=2026-09-30T00%3A00%3A00.000Z&limit=5&nextToken=a+b"
        );
        assert_eq!(
            build_collection_query(&CollectionParams::default(), NOW).unwrap(),
            ""
        );
        let bad = CollectionParams {
            start: Some("whenever".into()),
            ..Default::default()
        };
        assert!(build_collection_query(&bad, NOW).is_err());
    }
}
