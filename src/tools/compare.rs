//! `compare_periods`: per-period averages and percentage changes.

use super::ToolResult;
use super::dates::{InvalidDateExpression, validate_date_range};
use super::weekly::{mean_or_zero, score_value, scored, sleep_duration_hours};
use crate::api::pagination::{PageOptions, fetch_all_pages};
use crate::api::{ENDPOINT_CYCLE, ENDPOINT_RECOVERY, ENDPOINT_SLEEP, WhoopApi};
use crate::js::{self, num, obj};
use serde_json::Value;

const MAX_PERIOD_DAYS: f64 = 90.0;
const UNCHANGED_THRESHOLD: f64 = 5.0;

/// Inputs for `compare_periods`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparePeriodsParams {
    pub period_a_start: String,
    pub period_a_end: String,
    pub period_b_start: String,
    pub period_b_end: String,
}

fn percent_change(old: f64, new: f64) -> f64 {
    if old == 0.0 {
        return if new == 0.0 { 0.0 } else { 100.0 };
    }
    ((new - old) / old.abs()) * 100.0
}

fn direction(change: f64, up: &'static str, down: &'static str) -> &'static str {
    if change.abs() <= UNCHANGED_THRESHOLD {
        "unchanged"
    } else if change > 0.0 {
        up
    } else {
        down
    }
}

async fn fetch_records(
    client: &dyn WhoopApi,
    endpoint: &str,
    query: &str,
) -> ToolResult<Vec<Value>> {
    let options = PageOptions {
        max_records: 100,
        max_pages: 10,
        inter_page_delay_ms: 0,
    };
    Ok(
        fetch_all_pages(client, &format!("{endpoint}{query}"), options, false)
            .await?
            .records,
    )
}

struct PeriodData {
    recovery: f64,
    sleep: f64,
    strain: f64,
}

async fn load_period(client: &dyn WhoopApi, start: &str, end: &str) -> ToolResult<PeriodData> {
    let query = format!(
        "?{}",
        js::search_params(&[("start", start), ("end", end), ("limit", "25")])
    );
    let recovery = fetch_records(client, ENDPOINT_RECOVERY, &query).await?;
    let sleep = fetch_records(client, ENDPOINT_SLEEP, &query).await?;
    let cycle = fetch_records(client, ENDPOINT_CYCLE, &query).await?;
    let recovery_scores: Vec<f64> = recovery
        .iter()
        .filter(|r| scored(r))
        .map(|r| score_value(r, "recovery_score"))
        .collect();
    let sleep_hours: Vec<f64> = sleep
        .iter()
        .filter(|s| scored(s) && !js::truthy(s.get("nap")))
        .map(sleep_duration_hours)
        .collect();
    let strains: Vec<f64> = cycle
        .iter()
        .filter(|c| scored(c))
        .map(|c| score_value(c, "strain"))
        .collect();
    Ok(PeriodData {
        recovery: mean_or_zero(&recovery_scores)?,
        sleep: mean_or_zero(&sleep_hours)?,
        strain: mean_or_zero(&strains)?,
    })
}

fn period(start: &str, end: &str) -> Value {
    obj([
        ("start", Value::from(start)),
        ("end", Value::from(end)),
        (
            "days",
            num((js::parse(end) - js::parse(start)) / js::DAY_MS),
        ),
    ])
}

/// `compare_periods`
pub async fn compare_periods(
    client: &dyn WhoopApi,
    params: &ComparePeriodsParams,
) -> ToolResult<Value> {
    let p = params;
    validate_date_range(&p.period_a_start, &p.period_a_end, MAX_PERIOD_DAYS)?;
    validate_date_range(&p.period_b_start, &p.period_b_end, MAX_PERIOD_DAYS)?;
    let (a_start, a_end) = (js::parse(&p.period_a_start), js::parse(&p.period_a_end));
    let (b_start, b_end) = (js::parse(&p.period_b_start), js::parse(&p.period_b_end));
    if a_start < b_end && b_start < a_end {
        return Err(InvalidDateExpression(
            "Periods overlap. Provide two non-overlapping time ranges for comparison.".into(),
        )
        .into());
    }

    let a = load_period(client, &p.period_a_start, &p.period_a_end).await?;
    let b = load_period(client, &p.period_b_start, &p.period_b_end).await?;
    let recovery_change = percent_change(a.recovery, b.recovery);
    let sleep_change = percent_change(a.sleep, b.sleep);
    let strain_change = percent_change(a.strain, b.strain);

    Ok(obj([
        ("period_a", period(&p.period_a_start, &p.period_a_end)),
        ("period_b", period(&p.period_b_start, &p.period_b_end)),
        (
            "recovery",
            obj([
                ("period_a_avg", num(a.recovery)),
                ("period_b_avg", num(b.recovery)),
                ("change_pct", num(recovery_change)),
                (
                    "direction",
                    Value::from(direction(recovery_change, "improved", "declined")),
                ),
            ]),
        ),
        (
            "sleep",
            obj([
                ("period_a_avg_hours", num(a.sleep)),
                ("period_b_avg_hours", num(b.sleep)),
                ("change_pct", num(sleep_change)),
                (
                    "direction",
                    Value::from(direction(sleep_change, "improved", "declined")),
                ),
            ]),
        ),
        (
            "strain",
            obj([
                ("period_a_avg", num(a.strain)),
                ("period_b_avg", num(b.strain)),
                ("change_pct", num(strain_change)),
                (
                    "direction",
                    Value::from(direction(strain_change, "increased", "decreased")),
                ),
            ]),
        ),
    ]))
}
