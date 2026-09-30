//! `get_weekly_summary`: seven-day aggregates across recovery, sleep, workouts, and cycles.

use super::dates::{monday_utc, resolve_date_expression};
use super::stats::{linear_regression, mean, trend_direction};
use super::{ToolError, ToolResult};
use crate::api::pagination::{PageOptions, fetch_all_pages};
use crate::api::{ENDPOINT_CYCLE, ENDPOINT_RECOVERY, ENDPOINT_SLEEP, ENDPOINT_WORKOUT, WhoopApi};
use crate::js::{self, num, obj};
use serde_json::{Map, Value};

/// Sunday 23:59:59.999 UTC of the week starting on `monday`.
fn sunday_end(monday: f64) -> ToolResult<String> {
    let p = js::utc_parts(monday).ok_or(js::InvalidTime)?;
    Ok(js::to_iso(js::date_utc(
        p.year as f64,
        p.month0 as f64,
        (p.day + 6) as f64,
        23.0,
        59.0,
        59.0,
        999.0,
    ))?)
}

fn resolve_week_range(week_start: Option<&str>, now: f64) -> ToolResult<(String, String)> {
    if let Some(week_start) = week_start.filter(|w| !w.is_empty()) {
        let resolved = resolve_date_expression(week_start, now)?;
        let end = sunday_end(js::parse(&resolved.start))?;
        return Ok((resolved.start, end));
    }
    let monday = monday_utc(now);
    Ok((js::to_iso(monday)?, sunday_end(monday)?))
}

/// Mean of scored values, or 0 when empty.
pub(crate) fn mean_or_zero(values: &[f64]) -> ToolResult<f64> {
    if values.is_empty() {
        Ok(0.0)
    } else {
        mean(values)
    }
}

pub(crate) fn scored(record: &Value) -> bool {
    js::s(record.get("score_state")) == "SCORED" && js::truthy(record.get("score"))
}

pub(crate) fn score_value(record: &Value, key: &str) -> f64 {
    js::f(record.get("score").and_then(|s| s.get(key)))
}

pub(crate) fn sleep_duration_hours(sleep: &Value) -> f64 {
    (js::parse(js::s(sleep.get("end"))) - js::parse(js::s(sleep.get("start")))) / js::HOUR_MS
}

/// Non-null optional score values (`v != null`).
pub(crate) fn present_scores(records: &[&Value], key: &str) -> Vec<f64> {
    records
        .iter()
        .filter_map(|r| match r.get("score").and_then(|s| s.get(key)) {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_f64().unwrap_or(f64::NAN)),
        })
        .collect()
}

async fn safe_fetch(
    client: &dyn WhoopApi,
    endpoint: &str,
    query: &str,
) -> Result<Vec<Value>, String> {
    let options = PageOptions {
        max_records: 50,
        max_pages: 5,
        inter_page_delay_ms: 0,
    };
    fetch_all_pages(client, &format!("{endpoint}{query}"), options, false)
        .await
        .map(|pages| pages.records)
        .map_err(|error| error.to_string())
}

/// `get_weekly_summary`
pub async fn get_weekly_summary(
    client: &dyn WhoopApi,
    week_start: Option<&str>,
    now: f64,
) -> ToolResult<Value> {
    let (start, end) = resolve_week_range(week_start, now)?;
    let query = format!(
        "?{}",
        js::search_params(&[("start", &start), ("end", &end), ("limit", "25")])
    );

    let recovery = safe_fetch(client, ENDPOINT_RECOVERY, &query).await;
    let sleep = safe_fetch(client, ENDPOINT_SLEEP, &query).await;
    let workout = safe_fetch(client, ENDPOINT_WORKOUT, &query).await;
    let cycle = safe_fetch(client, ENDPOINT_CYCLE, &query).await;

    let mut warnings = Vec::new();
    for (name, result) in [
        ("recovery", &recovery),
        ("sleep", &sleep),
        ("workout", &workout),
        ("cycle", &cycle),
    ] {
        if let Err(message) = result {
            warnings.push(format!("{name}: {message}"));
        }
    }
    if warnings.len() == 4 {
        return Err(ToolError::Other(format!(
            "All endpoints failed: {}",
            warnings.join("; ")
        )));
    }
    let records = |r: &Result<Vec<Value>, String>| r.clone().unwrap_or_default();

    let recoveries = records(&recovery);
    let scored_recoveries: Vec<&Value> = recoveries.iter().filter(|r| scored(r)).collect();
    let scores: Vec<f64> = scored_recoveries
        .iter()
        .map(|r| score_value(r, "recovery_score"))
        .collect();
    let hrv: Vec<f64> = scored_recoveries
        .iter()
        .map(|r| score_value(r, "hrv_rmssd_milli"))
        .collect();
    let rhr: Vec<f64> = scored_recoveries
        .iter()
        .map(|r| score_value(r, "resting_heart_rate"))
        .collect();
    let trend = if scores.len() >= 2 {
        let (slope, r2) = linear_regression(&scores)?;
        trend_direction(slope, r2)
    } else {
        "stable"
    };
    let bounded = |f: fn(&[f64]) -> f64| if scores.is_empty() { 0.0 } else { f(&scores) };
    let recovery_json = obj([
        ("average_score", num(mean_or_zero(&scores)?)),
        ("min_score", num(bounded(js::min))),
        ("max_score", num(bounded(js::max))),
        ("average_hrv", num(mean_or_zero(&hrv)?)),
        ("average_rhr", num(mean_or_zero(&rhr)?)),
        ("trend", Value::from(trend)),
    ]);

    let sleeps = records(&sleep);
    let scored_sleeps: Vec<&Value> = sleeps
        .iter()
        .filter(|s| scored(s) && !js::truthy(s.get("nap")))
        .collect();
    let durations: Vec<f64> = scored_sleeps
        .iter()
        .map(|s| sleep_duration_hours(s))
        .collect();
    let sleep_json = obj([
        ("average_duration_hours", num(mean_or_zero(&durations)?)),
        (
            "average_performance_pct",
            num(mean_or_zero(&present_scores(
                &scored_sleeps,
                "sleep_performance_percentage",
            ))?),
        ),
        (
            "average_efficiency_pct",
            num(mean_or_zero(&present_scores(
                &scored_sleeps,
                "sleep_efficiency_percentage",
            ))?),
        ),
    ]);

    let workouts = records(&workout);
    let scored_workouts: Vec<&Value> = workouts.iter().filter(|w| scored(w)).collect();
    let mut breakdown = Map::new();
    let (mut total_strain, mut total_kj) = (0.0, 0.0);
    for workout in &scored_workouts {
        total_strain += score_value(workout, "strain");
        total_kj += score_value(workout, "kilojoule");
        let sport = workout
            .get("sport_name")
            .and_then(Value::as_str)
            .unwrap_or("undefined")
            .to_string();
        let count = breakdown.get(&sport).and_then(Value::as_f64).unwrap_or(0.0);
        breakdown.insert(sport, num(count + 1.0));
    }
    let workouts_json = obj([
        ("count", Value::from(scored_workouts.len())),
        ("total_strain", num(total_strain)),
        ("total_calories_kj", num(total_kj)),
        ("sport_breakdown", Value::Object(breakdown)),
    ]);

    let cycles = records(&cycle);
    let strains: Vec<f64> = cycles
        .iter()
        .filter(|c| scored(c))
        .map(|c| score_value(c, "strain"))
        .collect();
    let strain_json = obj([
        ("average_daily_strain", num(mean_or_zero(&strains)?)),
        (
            "max_daily_strain",
            num(if strains.is_empty() {
                0.0
            } else {
                js::max(&strains)
            }),
        ),
    ]);

    let mut result = obj([
        ("week_start", Value::from(start)),
        ("week_end", Value::from(end)),
        ("recovery", recovery_json),
        ("sleep", sleep_json),
        ("workouts", workouts_json),
        ("strain", strain_json),
    ]);
    if !warnings.is_empty() {
        result["warnings"] = Value::from(warnings);
    }
    Ok(result)
}
