//! `get_trend`: regression, statistics, and anomalies for one metric.

use super::stats::{
    detect_anomalies, linear_regression, mean, median, standard_deviation, trend_direction,
};
use super::weekly::{score_value, scored, sleep_duration_hours};
use super::{ToolError, ToolResult};
use crate::api::pagination::{PageOptions, fetch_all_pages};
use crate::api::{ENDPOINT_CYCLE, ENDPOINT_RECOVERY, ENDPOINT_SLEEP, WhoopApi};
use crate::js::{self, num, obj};
use serde_json::Value;

fn confidence(r2: f64) -> &'static str {
    if r2 > 0.7 {
        "high"
    } else if r2 > 0.4 {
        "medium"
    } else {
        "low"
    }
}

fn extract(metric: &str, records: &[Value]) -> (Vec<f64>, Vec<String>) {
    let text = |r: &Value, key: &str| {
        r.get(key)
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string()
    };
    let pairs: Vec<(f64, String)> = match metric {
        "recovery" | "hrv" | "rhr" => {
            let field = match metric {
                "recovery" => "recovery_score",
                "hrv" => "hrv_rmssd_milli",
                _ => "resting_heart_rate",
            };
            records
                .iter()
                .filter(|r| scored(r))
                .map(|r| (score_value(r, field), text(r, "created_at")))
                .collect()
        }
        "sleep_duration" => records
            .iter()
            .filter(|s| scored(s) && !js::truthy(s.get("nap")))
            .map(|s| (sleep_duration_hours(s), text(s, "end")))
            .collect(),
        "sleep_performance" => records
            .iter()
            .filter(|s| {
                js::s(s.get("score_state")) == "SCORED"
                    && !matches!(
                        s.pointer("/score/sleep_performance_percentage"),
                        None | Some(Value::Null)
                    )
                    && !js::truthy(s.get("nap"))
            })
            .map(|s| {
                (
                    score_value(s, "sleep_performance_percentage"),
                    text(s, "end"),
                )
            })
            .collect(),
        _ => records
            .iter()
            .filter(|c| scored(c))
            .map(|c| (score_value(c, "strain"), text(c, "created_at")))
            .collect(),
    };
    pairs.into_iter().unzip()
}

fn endpoint(metric: &str) -> &'static str {
    match metric {
        "recovery" | "hrv" | "rhr" => ENDPOINT_RECOVERY,
        "sleep_duration" | "sleep_performance" => ENDPOINT_SLEEP,
        _ => ENDPOINT_CYCLE,
    }
}

/// `get_trend`
pub async fn get_trend(
    client: &dyn WhoopApi,
    metric: &str,
    days: Option<f64>,
    now: f64,
) -> ToolResult<Value> {
    let days = days.unwrap_or(30.0);
    let p = js::utc_parts(now).ok_or(js::InvalidTime)?;
    let start = js::to_iso(js::date_utc_ymd(
        p.year as f64,
        p.month0 as f64,
        p.day as f64 - days,
    ))?;
    let end = js::to_iso(now)?;
    let query = js::search_params(&[("start", &start), ("end", &end), ("limit", "25")]);
    let options = PageOptions {
        max_records: 100,
        max_pages: 10,
        inter_page_delay_ms: 0,
    };
    let pages = fetch_all_pages(
        client,
        &format!("{}?{query}", endpoint(metric)),
        options,
        false,
    )
    .await?;
    let (values, dates) = extract(metric, &pages.records);

    if values.len() < 2 {
        return Err(ToolError::Other(format!(
            "Insufficient data for trend analysis: need at least 2 scored data points, got {}. Try a longer time range or check that your WHOOP has recorded data for the \"{metric}\" metric.",
            values.len()
        )));
    }

    let (slope, r2) = linear_regression(&values)?;
    let anomalies: Vec<Value> = detect_anomalies(&values, 2.0)?
        .into_iter()
        .map(|(index, value, deviation)| {
            obj([
                (
                    "date",
                    Value::from(
                        dates
                            .get(index)
                            .cloned()
                            .unwrap_or_else(|| "unknown".into()),
                    ),
                ),
                ("value", num(value)),
                ("deviation_from_mean", num(deviation)),
            ])
        })
        .collect();

    Ok(obj([
        ("metric", Value::from(metric)),
        (
            "period",
            obj([
                ("start", Value::from(start)),
                ("end", Value::from(end)),
                ("days", num(days)),
            ]),
        ),
        (
            "values",
            Value::Array(values.iter().copied().map(num).collect()),
        ),
        (
            "statistics",
            obj([
                ("mean", num(mean(&values)?)),
                ("median", num(median(&values)?)),
                ("std_dev", num(standard_deviation(&values)?)),
                ("min", num(js::min(&values))),
                ("max", num(js::max(&values))),
            ]),
        ),
        (
            "trend",
            obj([
                ("direction", Value::from(trend_direction(slope, r2))),
                ("slope", num(slope)),
                ("confidence", Value::from(confidence(r2))),
            ]),
        ),
        ("anomalies", Value::Array(anomalies)),
    ]))
}
