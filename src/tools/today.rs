//! `get_today`: current recovery, last night's sleep, day strain, and latest workout.

use super::analytics::{
    SourceQuality, asleep_hours, local_day, observed_json, observed_period, parse_records,
};
use super::weekly::score_value;
use super::{ToolError, ToolResult};
use crate::api::{
    ApiError, ENDPOINT_CYCLE, ENDPOINT_RECOVERY, ENDPOINT_SLEEP, ENDPOINT_WORKOUT, GetOptions,
    WhoopApi,
};
use crate::catalog::{RecordKind, record_schema};
use crate::js::{self, num, obj};
use crate::schema::omit;
use serde_json::Value;

/// TTL for recovery, sleep, and workout lookups (5 minutes).
pub const DYNAMIC_TTL_MS: u64 = 5 * 60 * 1000;
/// TTL for cycle lookups (2 minutes).
pub const CYCLE_TTL_MS: u64 = 2 * 60 * 1000;
/// TTL for profile lookups (1 hour).
pub const PROFILE_TTL_MS: u64 = 60 * 60 * 1000;

fn unpack(result: &Result<Value, ApiError>) -> (Vec<Value>, SourceQuality) {
    match result {
        Err(_) => (Vec::new(), SourceQuality::with_status("fetch_failed")),
        Ok(value) => match value.get("records").and_then(Value::as_array) {
            None => (Vec::new(), SourceQuality::with_status("invalid")),
            Some(records) => {
                let truncated = js::truthy(value.get("next_token")) || records.len() > 25;
                (
                    records.iter().take(25).cloned().collect(),
                    SourceQuality::new(records.len(), truncated),
                )
            }
        },
    }
}

fn ms(record: &Value, key: &str) -> f64 {
    js::parse(js::s(record.get(key)))
}

fn sort_desc(records: &mut [Value], key: &str) {
    records.sort_by(|a, b| {
        ms(b, key)
            .partial_cmp(&ms(a, key))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Strict equality for JSON scalars (`===`).
fn same(a: Option<&Value>, b: Option<&Value>) -> bool {
    match (a, b) {
        (Some(Value::Number(x)), Some(Value::Number(y))) => x.as_f64() == y.as_f64(),
        (Some(x), Some(y)) => x == y,
        (None, None) => true,
        _ => false,
    }
}

fn mark(record: Option<&Value>, quality: &mut SourceQuality) -> bool {
    let Some(record) = record else {
        return false;
    };
    quality.source_updated_at = Some(js::s(record.get("updated_at")).to_string());
    let state = js::s(record.get("score_state"));
    quality.status = if state == "PENDING_SCORE" {
        "pending"
    } else if state != "SCORED" {
        "unscored"
    } else if js::truthy(record.get("score")) {
        "available"
    } else {
        "invalid"
    };
    quality.records_used = usize::from(quality.status == "available");
    quality.status == "available"
}

fn milli_to_hours(value: f64) -> f64 {
    js::round1(value / js::HOUR_MS)
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

/// `get_today`
pub async fn get_today(client: &dyn WhoopApi, now: f64) -> ToolResult<Value> {
    let recovery_path = format!("{ENDPOINT_RECOVERY}?limit=25");
    let sleep_path = format!("{ENDPOINT_SLEEP}?limit=25");
    let cycle_path = format!("{ENDPOINT_CYCLE}?limit=25");
    let workout_path = format!("{ENDPOINT_WORKOUT}?limit=25");
    let (recovery_result, sleep_result, cycle_result, workout_result) = tokio::join!(
        client.get(&recovery_path, GetOptions::cached(DYNAMIC_TTL_MS)),
        client.get(&sleep_path, GetOptions::cached(DYNAMIC_TTL_MS)),
        client.get(&cycle_path, GetOptions::cached(CYCLE_TTL_MS)),
        client.get(&workout_path, GetOptions::cached(DYNAMIC_TTL_MS)),
    );
    if recovery_result.is_err() && sleep_result.is_err() && cycle_result.is_err() {
        return Err(ToolError::Api(ApiError::Network(
            "All API calls failed. Unable to retrieve today's health snapshot.".into(),
        )));
    }

    let now_iso = js::to_iso(now)?;
    let (recovery_records, mut recovery_q) = unpack(&recovery_result);
    let (sleep_records, mut sleep_q) = unpack(&sleep_result);
    let (cycle_records, mut cycle_q) = unpack(&cycle_result);
    let (workout_records, mut workout_q) = unpack(&workout_result);

    let cycle_schema = record_schema(RecordKind::Cycle);
    let mut cycles: Vec<Value> =
        parse_records(&cycle_records, &omit(cycle_schema, "score"), &mut cycle_q)
            .into_iter()
            .filter(|r| ms(r, "start") <= now)
            .collect();
    sort_desc(&mut cycles, "start");
    let mut cycle_candidate = None;
    for record in &cycles {
        let offset = js::s(record.get("timezone_offset"));
        let open = matches!(record.get("end"), None | Some(Value::Null));
        if open || local_day(js::s(record.get("end")), offset)? == local_day(&now_iso, offset)? {
            cycle_candidate = Some(record.clone());
            break;
        }
    }
    let cycle = parse_records(
        &cycle_candidate.iter().cloned().collect::<Vec<_>>(),
        cycle_schema,
        &mut cycle_q,
    )
    .into_iter()
    .next();
    if cycle_candidate.is_none() && !cycles.is_empty() {
        cycle_q.status = "stale";
    }

    let sleep_schema = record_schema(RecordKind::Sleep);
    let mut sleeps: Vec<Value> =
        parse_records(&sleep_records, &omit(sleep_schema, "score"), &mut sleep_q)
            .into_iter()
            .filter(|r| {
                !js::truthy(r.get("nap")) && ms(r, "end") <= now && ms(r, "end") > ms(r, "start")
            })
            .collect();
    sort_desc(&mut sleeps, "end");
    let primary_sleep = sleeps.first();
    let sleep_candidate = match primary_sleep {
        Some(primary) => {
            let offset = js::s(primary.get("timezone_offset"));
            (local_day(js::s(primary.get("end")), offset)? == local_day(&now_iso, offset)?)
                .then(|| primary.clone())
        }
        None => None,
    };
    let current_sleep = parse_records(
        &sleep_candidate.iter().cloned().collect::<Vec<_>>(),
        sleep_schema,
        &mut sleep_q,
    )
    .into_iter()
    .next();
    if primary_sleep.is_some() && sleep_candidate.is_none() {
        sleep_q.status = "stale";
    }

    let recoveries = parse_records(
        &recovery_records,
        record_schema(RecordKind::Recovery),
        &mut recovery_q,
    );
    let recovery_record = match (&cycle, &current_sleep) {
        (Some(cycle), Some(sleep)) if same(sleep.get("cycle_id"), cycle.get("id")) => recoveries
            .iter()
            .find(|r| {
                same(r.get("cycle_id"), cycle.get("id"))
                    && same(r.get("sleep_id"), sleep.get("id"))
                    && same(r.get("user_id"), cycle.get("user_id"))
                    && same(r.get("user_id"), sleep.get("user_id"))
            })
            .cloned(),
        _ => None,
    };

    let mut workouts: Vec<Value> = parse_records(
        &workout_records,
        record_schema(RecordKind::Workout),
        &mut workout_q,
    )
    .into_iter()
    .filter(|r| ms(r, "end") <= now && ms(r, "end") > ms(r, "start"))
    .collect();
    sort_desc(&mut workouts, "start");

    let sleep_available = mark(current_sleep.as_ref(), &mut sleep_q);
    let cycle_available = mark(cycle.as_ref(), &mut cycle_q);
    let recovery_available = mark(recovery_record.as_ref(), &mut recovery_q);
    if !sleep_available && recovery_available {
        recovery_q.status = sleep_q.status;
        recovery_q.records_used = 0;
    }
    if recovery_record
        .as_ref()
        .is_some_and(|r| js::truthy(r.pointer("/score/user_calibrating")))
    {
        recovery_q.status = "calibrating";
    }
    let workout_available = mark(workouts.first(), &mut workout_q);

    let recovery = match &recovery_record {
        Some(record)
            if recovery_available && sleep_available && js::truthy(record.get("score")) =>
        {
            let optional = |key: &str| match record.pointer(&format!("/score/{key}")) {
                None | Some(Value::Null) => Value::Null,
                Some(v) => v.clone(),
            };
            Some(obj([
                ("score", num(score_value(record, "recovery_score"))),
                (
                    "hrv_rmssd_milli",
                    num(score_value(record, "hrv_rmssd_milli")),
                ),
                (
                    "resting_heart_rate",
                    num(score_value(record, "resting_heart_rate")),
                ),
                ("spo2_pct", optional("spo2_percentage")),
                ("skin_temp_celsius", optional("skin_temp_celsius")),
            ]))
        }
        _ => None,
    };

    let sleep = match &current_sleep {
        Some(record)
            if sleep_available
                && js::s(record.get("score_state")) == "SCORED"
                && js::truthy(record.get("score")) =>
        {
            let stage = |key: &str| js::f(record.pointer(&format!("/score/stage_summary/{key}")));
            let optional = |key: &str| match record.pointer(&format!("/score/{key}")) {
                None | Some(Value::Null) => Value::Null,
                Some(v) => v.clone(),
            };
            Some(obj([
                (
                    "total_hours",
                    num(milli_to_hours(stage("total_in_bed_time_milli"))),
                ),
                (
                    "time_in_bed_hours",
                    num(milli_to_hours(stage("total_in_bed_time_milli"))),
                ),
                ("asleep_hours", num(js::round1(asleep_hours(record)))),
                (
                    "rem_hours",
                    num(milli_to_hours(stage("total_rem_sleep_time_milli"))),
                ),
                (
                    "deep_hours",
                    num(milli_to_hours(stage("total_slow_wave_sleep_time_milli"))),
                ),
                (
                    "light_hours",
                    num(milli_to_hours(stage("total_light_sleep_time_milli"))),
                ),
                (
                    "awake_hours",
                    num(milli_to_hours(stage("total_awake_time_milli"))),
                ),
                ("performance_pct", optional("sleep_performance_percentage")),
                ("efficiency_pct", optional("sleep_efficiency_percentage")),
                ("respiratory_rate", optional("respiratory_rate")),
            ]))
        }
        _ => None,
    };

    let strain = match &cycle {
        Some(record) if cycle_available && js::truthy(record.get("score")) => {
            let last_workout = workouts
                .first()
                .filter(|w| workout_available && js::truthy(w.get("score")))
                .map(|w| {
                    obj([
                        (
                            "sport_name",
                            w.get("sport_name").cloned().unwrap_or(Value::Null),
                        ),
                        ("strain", num(score_value(w, "strain"))),
                        (
                            "occurred_at",
                            w.get("start").cloned().unwrap_or(Value::Null),
                        ),
                        ("percent_recorded", num(score_value(w, "percent_recorded"))),
                    ])
                });
            Some(obj([
                ("day_strain", num(score_value(record, "strain"))),
                ("energy_burned_kj", num(score_value(record, "kilojoule"))),
                ("last_workout", last_workout.unwrap_or(Value::Null)),
            ]))
        }
        _ => None,
    };

    let mut parts = Vec::new();
    if let Some(recovery) = &recovery {
        let score = js::f(recovery.get("score"));
        parts.push(format!(
            "Recovery {}% ({})",
            js::number_to_string(score),
            zone(score)
        ));
    }
    if let Some(sleep) = &sleep {
        parts.push(format!(
            "{}h sleep",
            js::number_to_string(js::f(sleep.get("asleep_hours")))
        ));
    }
    if let Some(strain) = &strain {
        parts.push(format!(
            "strain {}",
            js::number_to_string(js::f(strain.get("day_strain")))
        ));
    }
    let mut summary = if parts.is_empty() {
        "No data available yet today".to_string()
    } else {
        parts.join(", ")
    };

    let mut observed = Vec::new();
    if sleep.is_some()
        && let Some(s) = &current_sleep
    {
        observed.push(js::s(s.get("end")).to_string());
    }
    if strain.is_some()
        && let Some(c) = &cycle
    {
        observed.push(js::s(c.get("start")).to_string());
    }
    let sources = [
        ("recovery", &recovery_q),
        ("sleep", &sleep_q),
        ("cycle", &cycle_q),
        ("workout", &workout_q),
    ];
    let unavailable: Vec<String> = sources
        .iter()
        .filter(|(_, q)| q.status != "available")
        .map(|(name, q)| format!("{name}: {}", q.status))
        .collect();
    if !unavailable.is_empty() {
        summary.push_str(&format!(". {}", unavailable.join(", ")));
    }

    Ok(obj([
        ("timestamp", Value::from(now_iso.clone())),
        ("recovery", opt_value(recovery)),
        ("sleep", opt_value(sleep)),
        ("strain", opt_value(strain)),
        ("summary", Value::from(summary)),
        (
            "data_quality",
            obj([
                ("evaluated_at", Value::from(now_iso.clone())),
                (
                    "requested_period",
                    obj([
                        ("start", Value::from(js::to_iso(now - js::DAY_MS)?)),
                        ("end", Value::from(now_iso)),
                    ]),
                ),
                (
                    "observed_period",
                    observed_json(&observed_period(&observed)),
                ),
                (
                    "sources",
                    obj([
                        ("recovery", recovery_q.to_json()),
                        ("sleep", sleep_q.to_json()),
                        ("cycle", cycle_q.to_json()),
                        ("workout", workout_q.to_json()),
                    ]),
                ),
                ("method_version", Value::from("today-2")),
                (
                    "limitations",
                    Value::from(vec![
                        "Recorded offsets define local days; latest workout may be historical.",
                        "Fetch time and cache status are not available from the client.",
                    ]),
                ),
            ]),
        ),
    ]))
}

fn opt_value(value: Option<Value>) -> Value {
    value.unwrap_or(Value::Null)
}
