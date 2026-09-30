//! `get_calendar`: day-by-day grid of recovery, sleep, and strain.

use super::ToolResult;
use super::dates::resolve_date_expression;
use crate::api::pagination::{PageOptions, fetch_all_pages};
use crate::api::{ENDPOINT_CYCLE, ENDPOINT_RECOVERY, ENDPOINT_SLEEP, WhoopApi};
use crate::js::{self, date_utc, date_utc_ymd, num, obj};
use serde_json::Value;

fn utc_day(ms: f64, offset_days: f64) -> f64 {
    match js::utc_parts(ms) {
        Some(p) => date_utc_ymd(p.year as f64, p.month0 as f64, p.day as f64 + offset_days),
        None => f64::NAN,
    }
}

fn format_date(ms: f64) -> String {
    match js::utc_parts(ms) {
        Some(p) => format!("{}-{:02}-{:02}", p.year, p.month0 + 1, p.day),
        None => "NaN-NaN-NaN".into(),
    }
}

fn average(values: &[f64]) -> Value {
    if values.is_empty() {
        return Value::Null;
    }
    num(js::round1(values.iter().sum::<f64>() / values.len() as f64))
}

fn zone(score: f64) -> &'static str {
    if score >= 67.0 {
        "green"
    } else if score >= 34.0 {
        "yellow"
    } else {
        "red"
    }
}

fn date_prefix(record: &Value, key: &str) -> String {
    js::s(record.get(key)).chars().take(10).collect()
}

fn nested(record: Option<&Value>, pointer: &str) -> Value {
    match record.and_then(|r| r.pointer(pointer)) {
        None | Some(Value::Null) => Value::Null,
        Some(v) => v.clone(),
    }
}

/// `get_calendar`
pub async fn get_calendar(
    client: &dyn WhoopApi,
    days: Option<f64>,
    start: Option<&str>,
    now: f64,
) -> ToolResult<Value> {
    let num_days = days.unwrap_or(7.0);
    let today = utc_day(now, 0.0);
    let (grid_start, grid_end, ascending) = match start.filter(|s| !s.is_empty()) {
        Some(start) => {
            let resolved = resolve_date_expression(start, now)?;
            let grid_start = utc_day(js::parse(&resolved.start), 0.0);
            let tentative_end = utc_day(grid_start, num_days - 1.0);
            (
                grid_start,
                if tentative_end > today {
                    today
                } else {
                    tentative_end
                },
                true,
            )
        }
        None => (utc_day(today, -num_days + 1.0), today, false),
    };

    if grid_start > today {
        let day = format_date(grid_start);
        return Ok(obj([
            (
                "period",
                obj([
                    ("start", Value::from(day.clone())),
                    ("end", Value::from(day)),
                    ("days", Value::from(0)),
                ]),
            ),
            ("days", Value::Array(Vec::new())),
            (
                "averages",
                obj([
                    ("recovery", Value::Null),
                    ("sleep_hours", Value::Null),
                    ("strain", Value::Null),
                ]),
            ),
        ]));
    }

    let grid_length = js::round((grid_end - grid_start) / js::DAY_MS) + 1.0;
    let start_iso = js::to_iso(utc_day(grid_start, 0.0))?;
    let end_iso = match js::utc_parts(grid_end) {
        Some(p) => js::to_iso(date_utc(
            p.year as f64,
            p.month0 as f64,
            p.day as f64,
            23.0,
            59.0,
            59.0,
            999.0,
        ))?,
        None => js::to_iso(f64::NAN)?,
    };
    let options = PageOptions {
        max_records: (grid_length * 2.0) as usize,
        inter_page_delay_ms: if num_days > 30.0 { 100 } else { 0 },
        ..PageOptions::default()
    };
    let path = |endpoint: &str| format!("{endpoint}?start={start_iso}&end={end_iso}&limit=25");
    let (recovery_path, sleep_path, cycle_path) = (
        path(ENDPOINT_RECOVERY),
        path(ENDPOINT_SLEEP),
        path(ENDPOINT_CYCLE),
    );
    let (recovery, sleep, cycle) = tokio::try_join!(
        fetch_all_pages(client, &recovery_path, options, false),
        fetch_all_pages(client, &sleep_path, options, false),
        fetch_all_pages(client, &cycle_path, options, false),
    )?;

    let mut recovery_by_date: Vec<(String, &Value)> = Vec::new();
    for record in &recovery.records {
        upsert(
            &mut recovery_by_date,
            date_prefix(record, "created_at"),
            record,
            true,
        );
    }
    let mut sleep_by_date: Vec<(String, &Value)> = Vec::new();
    for record in sleep.records.iter().filter(|s| !js::truthy(s.get("nap"))) {
        upsert(
            &mut sleep_by_date,
            date_prefix(record, "end"),
            record,
            false,
        );
    }
    let mut cycle_by_date: Vec<(String, &Value)> = Vec::new();
    for record in &cycle.records {
        upsert(
            &mut cycle_by_date,
            date_prefix(record, "start"),
            record,
            true,
        );
    }
    let mut grid = Vec::new();
    let (mut recoveries, mut sleeps, mut strains) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..grid_length.max(0.0) as i64 {
        let (anchor, offset) = if ascending {
            (grid_start, i as f64)
        } else {
            (grid_end, -(i as f64))
        };
        let date = format_date(utc_day(anchor, offset));
        let recovery = lookup(&recovery_by_date, &date);
        let sleep = lookup(&sleep_by_date, &date);
        let cycle = lookup(&cycle_by_date, &date);

        let recovery_score = nested(recovery, "/score/recovery_score");
        let recovery_zone = if recovery_score.is_null() {
            Value::Null
        } else {
            Value::from(zone(recovery_score.as_f64().unwrap_or(f64::NAN)))
        };
        let sleep_hours = if sleep.is_some_and(|s| js::truthy(s.get("score"))) {
            let in_bed = js::f(
                sleep.and_then(|s| s.pointer("/score/stage_summary/total_in_bed_time_milli")),
            );
            num(js::round1(in_bed / js::HOUR_MS))
        } else {
            Value::Null
        };
        let day_strain = nested(cycle, "/score/strain");
        for (value, bucket) in [
            (&recovery_score, &mut recoveries),
            (&sleep_hours, &mut sleeps),
            (&day_strain, &mut strains),
        ] {
            if !value.is_null() {
                bucket.push(value.as_f64().unwrap_or(f64::NAN));
            }
        }
        grid.push(obj([
            ("date", Value::from(date)),
            ("recovery_score", recovery_score),
            ("recovery_zone", recovery_zone),
            ("sleep_hours", sleep_hours),
            (
                "sleep_performance_pct",
                nested(sleep, "/score/sleep_performance_percentage"),
            ),
            ("day_strain", day_strain),
        ]));
    }

    Ok(obj([
        (
            "period",
            obj([
                ("start", Value::from(format_date(grid_start))),
                ("end", Value::from(format_date(grid_end))),
                ("days", num(grid_length)),
            ]),
        ),
        ("days", Value::Array(grid)),
        (
            "averages",
            obj([
                ("recovery", average(&recoveries)),
                ("sleep_hours", average(&sleeps)),
                ("strain", average(&strains)),
            ]),
        ),
    ]))
}

fn lookup<'a>(map: &[(String, &'a Value)], date: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k == date).map(|(_, v)| *v)
}

fn upsert<'a>(map: &mut Vec<(String, &'a Value)>, key: String, value: &'a Value, replace: bool) {
    match map.iter_mut().find(|(k, _)| *k == key) {
        Some(entry) if replace => entry.1 = value,
        Some(_) => {}
        None => map.push((key, value)),
    }
}
