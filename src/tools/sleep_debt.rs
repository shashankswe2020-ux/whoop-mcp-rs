//! `get_sleep_debt`: nightly deficits, standing debt, and clock consistency.

use super::analytics::{
    DISCLAIMER, asleep_hours, finish_quality, load_analytics_source, local_day, local_time,
    main_sleeps, observed_period, period_json,
};
use super::baselines::data_quality;
use super::dates::resolve_date_expression;
use super::stats::{circular_stats, mean};
use super::{ToolError, ToolResult};
use crate::api::{ApiError, ENDPOINT_SLEEP, WhoopApi};
use crate::catalog::{RecordKind, record_schema};
use crate::js::{self, num, obj, opt_num};
use serde_json::Value;

fn need_milli(night: &Value) -> f64 {
    let need = |key: &str| js::f(night.pointer(&format!("/score/sleep_needed/{key}")));
    need("baseline_milli")
        + need("need_from_recent_strain_milli")
        + need("need_from_recent_nap_milli")
}

fn clock_minutes(ms: f64) -> (f64, i64) {
    js::utc_parts(ms).map_or((f64::NAN, -1), |p| {
        (
            (p.hour * 60 + p.minute) as f64 + p.second as f64 / 60.0,
            p.weekday,
        )
    })
}

/// `get_sleep_debt`
pub async fn get_sleep_debt(
    client: &dyn WhoopApi,
    days: Option<f64>,
    start: Option<&str>,
    now: f64,
) -> ToolResult<Value> {
    let days = days.unwrap_or(14.0);
    let start_time = match start.filter(|s| !s.is_empty()) {
        Some(start) => js::parse(&resolve_date_expression(start, now)?.start),
        None => now - days * js::DAY_MS,
    };
    let end_time = (start_time + days * js::DAY_MS).min(now);
    if !start_time.is_finite() || start_time >= end_time {
        return Err(ToolError::Invalid(
            "Sleep window must begin before the evaluation time.".into(),
        ));
    }
    let (period_start, period_end) = (js::to_iso(start_time)?, js::to_iso(end_time)?);
    let (records, mut quality) = load_analytics_source(
        client,
        ENDPOINT_SLEEP,
        &period_start,
        &period_end,
        record_schema(RecordKind::Sleep),
    )
    .await;
    if quality.status == "fetch_failed" {
        return Err(ToolError::Api(ApiError::Network(
            "Sleep source unavailable".into(),
        )));
    }
    let mut selected = Vec::new();
    for night in main_sleeps(&records, &period_start, &period_end, &mut quality)? {
        if need_milli(&night) < 0.0 {
            quality.exclude("invalid_need");
        } else {
            selected.push(night);
        }
    }

    let mut nights = Vec::new();
    let mut debts = Vec::new();
    for night in &selected {
        let needed = need_milli(night) / js::HOUR_MS;
        let achieved = asleep_hours(night);
        let debt = (needed - achieved).max(0.0);
        let debt = if (needed - achieved).is_nan() {
            f64::NAN
        } else {
            debt
        };
        debts.push(debt);
        nights.push(obj([
            (
                "date",
                Value::from(local_day(
                    js::s(night.get("end")),
                    js::s(night.get("timezone_offset")),
                )?),
            ),
            ("needed_hours", num(needed)),
            ("achieved_hours", num(achieved)),
            ("debt_hours", num(debt)),
        ]));
    }

    let (mut bedtimes, mut waketimes, mut weekdays, mut weekends) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for night in &selected {
        let offset = js::s(night.get("timezone_offset"));
        let (start_text, end_text) = (js::s(night.get("start")), js::s(night.get("end")));
        let (bed_minutes, _) = clock_minutes(local_time(start_text, offset)?);
        let (wake_minutes, wake_day) = clock_minutes(local_time(end_text, offset)?);
        bedtimes.push(bed_minutes);
        waketimes.push(wake_minutes);
        let midpoint =
            (bed_minutes + (js::parse(end_text) - js::parse(start_text)) / 120_000.0) % 1440.0;
        if wake_day == 0 || wake_day == 6 {
            weekends.push(midpoint)
        } else {
            weekdays.push(midpoint)
        }
    }
    let weekday_mean = circular_stats(&weekdays)?.0;
    let weekend_mean = circular_stats(&weekends)?.0;
    let midpoint_distance = weekday_mean.zip(weekend_mean).map(|(a, b)| (a - b).abs());
    let sufficient = nights.len() >= 3;
    finish_quality(&mut quality, &selected);
    let ends: Vec<String> = selected
        .iter()
        .map(|n| js::s(n.get("end")).to_string())
        .collect();
    let observed = observed_period(&ends);
    let truncated = quality.truncated;

    let summary = format!(
        "{} Social jetlag is a circular midpoint heuristic.{}",
        if sufficient {
            "Sum of observed nightly deficits, not outstanding debt."
        } else {
            "Insufficient data: at least three scored main sleeps are required."
        },
        if truncated {
            " Partial history: pagination limit reached."
        } else {
            ""
        }
    );
    let consistency = obj([
        (
            "bedtime_std_dev_minutes",
            if sufficient {
                opt_num(circular_stats(&bedtimes)?.1)
            } else {
                Value::Null
            },
        ),
        (
            "waketime_std_dev_minutes",
            if sufficient {
                opt_num(circular_stats(&waketimes)?.1)
            } else {
                Value::Null
            },
        ),
        (
            "social_jetlag_minutes",
            match midpoint_distance {
                Some(d) if sufficient => num(d.min(1440.0 - d)),
                _ => Value::Null,
            },
        ),
    ]);
    let standing_debt = selected
        .first()
        .map(|n| js::f(n.pointer("/score/sleep_needed/need_from_sleep_debt_milli")) / js::HOUR_MS);
    let evaluated_at = js::to_iso(now)?;
    Ok(obj([
        ("period", period_json(&period_start, &period_end)),
        ("nights_analyzed", Value::from(nights.len())),
        (
            "status",
            Value::from(if sufficient {
                "available"
            } else {
                "insufficient_data"
            }),
        ),
        (
            "total_debt_hours",
            if sufficient {
                num(debts.iter().sum())
            } else {
                Value::Null
            },
        ),
        (
            "avg_nightly_debt_hours",
            if sufficient {
                num(mean(&debts)?)
            } else {
                Value::Null
            },
        ),
        ("standing_debt_hours", opt_num(standing_debt)),
        (
            "standing_debt_date",
            nights.first().map_or(Value::Null, |n| n["date"].clone()),
        ),
        ("consistency", consistency),
        (
            "nights",
            Value::Array(nights.iter().take(30).cloned().collect()),
        ),
        ("output_capped", Value::from(nights.len() > 30)),
        ("truncated", Value::from(truncated)),
        ("summary", Value::from(summary)),
        ("disclaimer", Value::from(DISCLAIMER)),
        (
            "data_quality",
            data_quality(
                &evaluated_at,
                &period_start,
                &period_end,
                &observed,
                &[("sleep", &quality)],
                "sleep-debt-1",
                &[
                    "One recorded offset cannot reconstruct within-sleep DST changes.",
                    "Deficit totals do not predict recovery or prescribe repayment.",
                ],
            ),
        ),
    ]))
}
