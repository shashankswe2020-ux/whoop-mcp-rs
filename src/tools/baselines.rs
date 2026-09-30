//! `get_baselines`: personal rolling distributions excluding the latest observation.

use super::analytics::{
    DISCLAIMER, SourceQuality, asleep_hours, finish_quality, load_analytics_source, local_day,
    main_sleeps, observed_json, observed_period, period_json,
};
use super::stats::{mean, median, percentile, standard_deviation};
use super::weekly::score_value;
use super::{ToolError, ToolResult};
use crate::api::{ApiError, ENDPOINT_CYCLE, ENDPOINT_RECOVERY, ENDPOINT_SLEEP, WhoopApi};
use crate::catalog::{RecordKind, record_schema};
use crate::js::{self, num, obj};
use serde_json::{Map, Value};

const METRICS: [(&str, &str); 5] = [
    ("hrv", "ms"),
    ("rhr", "bpm"),
    ("respiratory_rate", "breaths/min"),
    ("sleep_hours", "hours"),
    ("recovery_score", "%"),
];

struct Observation {
    value: f64,
    timestamp: String,
    offset: String,
}

fn id_text(record: &Value, key: &str) -> String {
    js::number_to_string(js::f(record.get(key)))
}

/// `get_baselines`
pub async fn get_baselines(
    client: &dyn WhoopApi,
    baseline_days: Option<f64>,
    now: f64,
) -> ToolResult<Value> {
    let baseline_days = baseline_days.unwrap_or(30.0);
    let start = js::to_iso(now - baseline_days * js::DAY_MS)?;
    let end = js::to_iso(now)?;
    let ((recovery, mut recovery_q), (sleep, mut sleep_q), (cycle, mut cycle_q)) = tokio::join!(
        load_analytics_source(
            client,
            ENDPOINT_RECOVERY,
            &start,
            &end,
            record_schema(RecordKind::Recovery)
        ),
        load_analytics_source(
            client,
            ENDPOINT_SLEEP,
            &start,
            &end,
            record_schema(RecordKind::Sleep)
        ),
        load_analytics_source(
            client,
            ENDPOINT_CYCLE,
            &start,
            &end,
            record_schema(RecordKind::Cycle)
        ),
    );
    if [&recovery_q, &sleep_q, &cycle_q]
        .iter()
        .all(|q| q.status == "fetch_failed")
    {
        return Err(ToolError::Api(ApiError::Network(
            "Baseline sources unavailable".into(),
        )));
    }

    let mut observations: Vec<(&str, Vec<Observation>)> =
        METRICS.iter().map(|(m, _)| (*m, Vec::new())).collect();
    let mut push = |metric: &str, value: f64, timestamp: &str, offset: &str| {
        if let Some((_, list)) = observations.iter_mut().find(|(m, _)| *m == metric) {
            list.push(Observation {
                value,
                timestamp: timestamp.into(),
                offset: offset.into(),
            });
        }
    };

    let mut cycles: Vec<(String, &Value)> = Vec::new();
    for record in &cycle {
        let key = format!("{}:{}", id_text(record, "user_id"), id_text(record, "id"));
        match cycles.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = record,
            None => cycles.push((key, record)),
        }
    }
    let period_start = js::parse(&start);
    let mut used_recoveries = Vec::new();
    let mut used_cycles = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for record in &recovery {
        let state = js::s(record.get("score_state"));
        if state != "SCORED" || !js::truthy(record.get("score")) {
            recovery_q.exclude(if state == "PENDING_SCORE" {
                "pending"
            } else {
                "unscored"
            });
            continue;
        }
        if js::truthy(record.pointer("/score/user_calibrating")) {
            recovery_q.exclude("calibrating");
            continue;
        }
        let key = format!(
            "{}:{}",
            id_text(record, "user_id"),
            id_text(record, "cycle_id")
        );
        let Some((_, context)) = cycles.iter().find(|(k, _)| *k == key) else {
            recovery_q.exclude("missing_join");
            continue;
        };
        let context_start = js::parse(js::s(context.get("start")));
        if context_start < period_start || context_start >= now {
            recovery_q.exclude("outside_window");
            continue;
        }
        if seen.contains(&key) {
            recovery_q.exclude("duplicate_cycle");
            continue;
        }
        seen.push(key);
        used_recoveries.push(record.clone());
        used_cycles.push((*context).clone());
        let (timestamp, offset) = (
            js::s(context.get("start")),
            js::s(context.get("timezone_offset")),
        );
        push(
            "hrv",
            score_value(record, "hrv_rmssd_milli"),
            timestamp,
            offset,
        );
        push(
            "rhr",
            score_value(record, "resting_heart_rate"),
            timestamp,
            offset,
        );
        push(
            "recovery_score",
            score_value(record, "recovery_score"),
            timestamp,
            offset,
        );
    }

    let nights = main_sleeps(&sleep, &start, &end, &mut sleep_q)?;
    for night in &nights {
        let (timestamp, offset) = (js::s(night.get("end")), js::s(night.get("timezone_offset")));
        push("sleep_hours", asleep_hours(night), timestamp, offset);
        if !matches!(
            night.pointer("/score/respiratory_rate"),
            None | Some(Value::Null)
        ) {
            push(
                "respiratory_rate",
                score_value(night, "respiratory_rate"),
                timestamp,
                offset,
            );
        }
    }

    let now_iso = js::to_iso(now)?;
    let mut metrics = Map::new();
    let mut metric_status = Map::new();
    let mut used_timestamps = Vec::new();
    for (metric, unit) in METRICS {
        let list = &mut observations
            .iter_mut()
            .find(|(m, _)| *m == metric)
            .expect("metric")
            .1;
        list.sort_by(|a, b| {
            js::parse(&b.timestamp)
                .partial_cmp(&js::parse(&a.timestamp))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let latest = list.first().map(|o| o.value);
        let mut values = Vec::new();
        for item in list.iter().skip(1) {
            if local_day(&item.timestamp, &item.offset)? != local_day(&now_iso, &item.offset)? {
                values.push(item.value);
                used_timestamps.push(item.timestamp.clone());
            }
        }
        metric_status.insert(
            metric.into(),
            obj([
                (
                    "status",
                    Value::from(if values.len() >= 14 {
                        "available"
                    } else {
                        "insufficient_data"
                    }),
                ),
                ("sample_size", Value::from(values.len())),
                ("unit", Value::from(unit)),
            ]),
        );
        let band = if values.len() < 14 {
            Value::Null
        } else {
            let sd = standard_deviation(&values)?;
            let latest_percentile = latest.map(|l| {
                let below = values.iter().filter(|v| **v < l).count() as f64;
                let equal = values.iter().filter(|v| **v == l).count() as f64;
                (100.0 * (below + 0.5 * equal)) / values.len() as f64
            });
            obj([
                ("sample_size", Value::from(values.len())),
                ("mean", num(mean(&values)?)),
                ("median", num(median(&values)?)),
                ("std_dev", num(sd)),
                ("p10", num(percentile(&values, 10.0)?)),
                ("p25", num(percentile(&values, 25.0)?)),
                ("p50", num(percentile(&values, 50.0)?)),
                ("p75", num(percentile(&values, 75.0)?)),
                ("p90", num(percentile(&values, 90.0)?)),
                ("latest", js::opt_num(latest)),
                ("latest_percentile", js::opt_num(latest_percentile)),
                ("constant_baseline", Value::from(sd == 0.0)),
            ])
        };
        metrics.insert(metric.into(), band);
    }

    finish_quality(&mut recovery_q, &used_recoveries);
    finish_quality(&mut sleep_q, &nights);
    finish_quality(&mut cycle_q, &used_cycles);
    let observed = observed_period(&used_timestamps);
    let truncated = [&recovery_q, &sleep_q, &cycle_q]
        .iter()
        .any(|q| q.truncated);
    let mut limitations = vec![
        "Descriptive personal distributions, not population norms or diagnosis.",
        "Latest observation and current local day are excluded from each baseline.",
    ];
    if truncated {
        limitations.push("Partial history: upstream pagination limit reached.");
    }
    Ok(obj([
        ("period", observed_json(&observed)),
        ("metrics", Value::Object(metrics)),
        ("metric_status", Value::Object(metric_status)),
        ("truncated", Value::from(truncated)),
        ("disclaimer", Value::from(DISCLAIMER)),
        (
            "data_quality",
            data_quality(
                &now_iso,
                &start,
                &end,
                &observed,
                &[
                    ("recovery", &recovery_q),
                    ("sleep", &sleep_q),
                    ("cycle", &cycle_q),
                ],
                "baselines-1",
                &limitations,
            ),
        ),
    ]))
}

/// Shared `data_quality` object for analytics tools.
pub(crate) fn data_quality(
    evaluated_at: &str,
    start: &str,
    end: &str,
    observed: &Option<(String, String)>,
    sources: &[(&str, &SourceQuality)],
    method_version: &str,
    limitations: &[&str],
) -> Value {
    let sources: Map<String, Value> = sources
        .iter()
        .map(|(name, q)| ((*name).to_string(), q.to_json()))
        .collect();
    obj([
        ("evaluated_at", Value::from(evaluated_at)),
        ("requested_period", period_json(start, end)),
        ("observed_period", observed_json(observed)),
        ("sources", Value::Object(sources)),
        ("method_version", Value::from(method_version)),
        ("limitations", Value::from(limitations.to_vec())),
    ])
}
