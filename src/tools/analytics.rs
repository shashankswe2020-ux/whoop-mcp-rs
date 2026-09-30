//! Shared analytics plumbing: data-quality reporting, local-day math, schema
//! validated source loading, and main-sleep selection.

use super::{ToolError, ToolResult};
use crate::api::WhoopApi;
use crate::api::pagination::{ABSOLUTE_MAX_RECORDS, PageError, PageOptions, fetch_all_pages};
use crate::js::{self, obj};
use crate::schema;
use serde_json::{Map, Value};

/// Standard medical disclaimer.
pub const DISCLAIMER: &str = "Statistical observation from your data, not medical advice.";

/// Quality metadata for one data source.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceQuality {
    pub status: &'static str,
    pub fetched_at: Option<String>,
    pub source_updated_at: Option<String>,
    pub cache_status: &'static str,
    pub records_fetched: usize,
    pub records_used: usize,
    pub exclusions: Vec<(String, u64)>,
    pub truncated: bool,
}

impl SourceQuality {
    /// A fresh "missing" quality record.
    pub fn new(count: usize, truncated: bool) -> Self {
        Self {
            status: "missing",
            fetched_at: None,
            source_updated_at: None,
            cache_status: "unknown",
            records_fetched: count,
            records_used: 0,
            exclusions: Vec::new(),
            truncated,
        }
    }

    /// Quality with a specific status and no records.
    pub fn with_status(status: &'static str) -> Self {
        Self {
            status,
            ..Self::new(0, false)
        }
    }

    /// Count an exclusion reason.
    pub fn exclude(&mut self, reason: &str) {
        match self.exclusions.iter_mut().find(|(r, _)| r == reason) {
            Some((_, count)) => *count += 1,
            None => self.exclusions.push((reason.to_string(), 1)),
        }
    }

    fn exclusion(&self, reason: &str) -> u64 {
        self.exclusions
            .iter()
            .find(|(r, _)| r == reason)
            .map_or(0, |(_, c)| *c)
    }

    pub fn to_json(&self) -> Value {
        let exclusions: Map<String, Value> = self
            .exclusions
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(*v)))
            .collect();
        obj([
            ("status", Value::from(self.status)),
            (
                "fetched_at",
                self.fetched_at.clone().map_or(Value::Null, Value::from),
            ),
            (
                "source_updated_at",
                self.source_updated_at
                    .clone()
                    .map_or(Value::Null, Value::from),
            ),
            ("cache_status", Value::from(self.cache_status)),
            ("records_fetched", Value::from(self.records_fetched)),
            ("records_used", Value::from(self.records_used)),
            ("exclusions", Value::Object(exclusions)),
            ("truncated", Value::from(self.truncated)),
        ])
    }
}

/// A `{start, end}` period.
pub fn period_json(start: &str, end: &str) -> Value {
    obj([("start", Value::from(start)), ("end", Value::from(end))])
}

/// Parse `±HH:MM` into minutes (validates the offset pattern).
fn offset_minutes(offset: &str) -> ToolResult<f64> {
    if !schema::is_offset(offset) {
        return Err(ToolError::Invalid("Invalid timezone offset.".into()));
    }
    let hours: f64 = offset[1..3].parse().unwrap_or(0.0);
    let minutes: f64 = offset[4..6].parse().unwrap_or(0.0);
    let sign = if offset.starts_with('-') { -1.0 } else { 1.0 };
    Ok((hours * 60.0 + minutes) * sign)
}

/// Wall-clock time at the recorded offset, as a UTC-based timestamp.
pub fn local_time(timestamp: &str, offset: &str) -> ToolResult<f64> {
    Ok(js::parse(timestamp) + offset_minutes(offset)? * 60_000.0)
}

/// Local calendar day (`YYYY-MM-DD`) of `timestamp` at `offset`.
pub fn local_day(timestamp: &str, offset: &str) -> ToolResult<String> {
    Ok(js::to_iso(local_time(timestamp, offset)?)?[..10].to_string())
}

/// Hours asleep (light + slow wave + REM) for a scored sleep.
pub fn asleep_hours(sleep: &Value) -> f64 {
    let stages = sleep.pointer("/score/stage_summary");
    let get = |key: &str| js::f(stages.and_then(|s| s.get(key)));
    (get("total_light_sleep_time_milli")
        + get("total_slow_wave_sleep_time_milli")
        + get("total_rem_sleep_time_milli"))
        / js::HOUR_MS
}

/// Earliest and latest parseable timestamps.
pub fn observed_period(timestamps: &[String]) -> Option<(String, String)> {
    let mut valid: Vec<&String> = timestamps
        .iter()
        .filter(|t| js::parse(t).is_finite())
        .collect();
    valid.sort_by(|a, b| {
        js::parse(a)
            .partial_cmp(&js::parse(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Some(((*valid.first()?).clone(), (*valid.last()?).clone()))
}

/// JSON for an optional observed period.
pub fn observed_json(period: &Option<(String, String)>) -> Value {
    period
        .as_ref()
        .map_or(Value::Null, |(s, e)| period_json(s, e))
}

/// Validate records against a schema, counting invalid ones.
pub fn parse_records(records: &[Value], schema: &Value, quality: &mut SourceQuality) -> Vec<Value> {
    let parsed: Vec<Value> = records
        .iter()
        .filter_map(|record| match schema::validate(schema, Some(record)) {
            Ok(value) => Some(value),
            Err(_) => {
                quality.exclude("invalid");
                None
            }
        })
        .collect();
    if parsed.is_empty() && !records.is_empty() {
        quality.status = "invalid";
    }
    parsed
}

/// Load and validate an analytics source over a period (up to 500 records).
pub async fn load_analytics_source(
    client: &dyn WhoopApi,
    endpoint: &str,
    start: &str,
    end: &str,
    schema: &Value,
) -> (Vec<Value>, SourceQuality) {
    let query = js::search_params(&[("start", start), ("end", end), ("limit", "25")]);
    let options = PageOptions {
        max_records: ABSOLUTE_MAX_RECORDS,
        ..PageOptions::default()
    };
    match fetch_all_pages(client, &format!("{endpoint}?{query}"), options, true).await {
        Ok(pages) => {
            let mut quality = SourceQuality::new(pages.records.len(), pages.truncated);
            let records = parse_records(&pages.records, schema, &mut quality);
            (records, quality)
        }
        Err(PageError::Invalid) => (Vec::new(), SourceQuality::with_status("invalid")),
        Err(_) => (Vec::new(), SourceQuality::with_status("fetch_failed")),
    }
}

fn ms(record: &Value, key: &str) -> f64 {
    js::parse(js::s(record.get(key)))
}

/// One scored main sleep per local wake day, newest first.
pub fn main_sleeps(
    records: &[Value],
    start: &str,
    end: &str,
    quality: &mut SourceQuality,
) -> ToolResult<Vec<Value>> {
    let (period_start, period_end) = (js::parse(start), js::parse(end));
    let mut selected: Vec<(String, Value)> = Vec::new();
    for record in records {
        let (record_start, record_end) = (ms(record, "start"), ms(record, "end"));
        let duration = record_end - record_start;
        if duration <= 0.0 || record_end < period_start || record_end >= period_end {
            quality.exclude("outside_window_or_invalid_duration");
            continue;
        }
        if js::truthy(record.get("nap")) {
            quality.exclude("nap");
            continue;
        }
        let state = js::s(record.get("score_state"));
        if state != "SCORED" || !js::truthy(record.get("score")) {
            quality.exclude(if state == "PENDING_SCORE" {
                "pending"
            } else {
                "unscored"
            });
            continue;
        }
        let key = local_day(
            js::s(record.get("end")),
            js::s(record.get("timezone_offset")),
        )?;
        if let Some(index) = selected.iter().position(|(k, _)| *k == key) {
            quality.exclude("duplicate_day");
            let previous = &selected[index].1;
            let previous_duration = ms(previous, "end") - ms(previous, "start");
            let id = js::s(record.get("id")).encode_utf16();
            let keep_previous = duration < previous_duration
                || (duration == previous_duration
                    && (record_end < ms(previous, "end")
                        || (js::s(record.get("end")) == js::s(previous.get("end"))
                            && id.cmp(js::s(previous.get("id")).encode_utf16())
                                != std::cmp::Ordering::Less)));
            if !keep_previous {
                selected[index].1 = record.clone();
            }
            continue;
        }
        selected.push((key, record.clone()));
    }
    let mut nights: Vec<Value> = selected.into_iter().map(|(_, v)| v).collect();
    nights.sort_by(|a, b| {
        ms(b, "end")
            .partial_cmp(&ms(a, "end"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(nights)
}

/// Finalize quality after records are selected.
pub fn finish_quality(quality: &mut SourceQuality, records: &[Value]) {
    quality.records_used = records.len();
    let updated: Vec<String> = records
        .iter()
        .map(|r| js::s(r.get("updated_at")).to_string())
        .collect();
    quality.source_updated_at = observed_period(&updated).map(|(_, end)| end);
    if !records.is_empty() {
        quality.status = "available";
    } else if quality.status != "fetch_failed" && quality.status != "invalid" {
        quality.status = if quality.exclusion("pending") > 0 {
            "pending"
        } else if quality.exclusion("calibrating") > 0 {
            "calibrating"
        } else if quality.exclusion("unscored") > 0 {
            "unscored"
        } else {
            "missing"
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sleep(id: &str, start: &str, end: &str) -> Value {
        json!({"id": id, "start": start, "end": end, "timezone_offset": "+00:00", "nap": false, "score_state": "SCORED", "score": {"stage_summary": {}}})
    }

    #[test]
    fn local_days_follow_offsets() {
        assert_eq!(
            local_day("2024-01-01T20:00:00Z", "+05:30").unwrap(),
            "2024-01-02"
        );
        assert_eq!(
            local_day("2024-01-01T02:00:00Z", "-05:00").unwrap(),
            "2023-12-31"
        );
        assert!(matches!(
            local_day("2024-01-01T02:00:00Z", "+0500"),
            Err(ToolError::Invalid(_))
        ));
    }

    #[test]
    fn selects_longest_main_sleep_per_day() {
        let records = vec![
            sleep("a", "2024-01-01T22:00:00Z", "2024-01-02T06:00:00Z"),
            sleep("b", "2024-01-01T23:00:00Z", "2024-01-02T07:00:00Z"),
            sleep("c", "2024-01-02T20:00:00Z", "2024-01-03T07:00:00Z"),
            json!({"id": "n", "start": "2024-01-02T13:00:00Z", "end": "2024-01-02T14:00:00Z", "timezone_offset": "+00:00", "nap": true}),
        ];
        let mut quality = SourceQuality::new(4, false);
        let nights = main_sleeps(
            &records,
            "2024-01-01T00:00:00Z",
            "2024-01-05T00:00:00Z",
            &mut quality,
        )
        .unwrap();
        let ids: Vec<&str> = nights.iter().map(|n| n["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["c", "b"]);
        assert_eq!(
            quality.exclusions,
            vec![("duplicate_day".to_string(), 1), ("nap".to_string(), 1)]
        );
    }

    #[test]
    fn quality_serializes_in_contract_order() {
        let mut quality = SourceQuality::new(3, true);
        quality.exclude("pending");
        finish_quality(&mut quality, &[]);
        assert_eq!(quality.status, "pending");
        assert_eq!(
            js::stringify(&quality.to_json()),
            r#"{"status":"pending","fetched_at":null,"source_updated_at":null,"cache_status":"unknown","records_fetched":3,"records_used":0,"exclusions":{"pending":1},"truncated":true}"#
        );
    }
}
