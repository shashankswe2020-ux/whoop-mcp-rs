//! Statistical helpers (population statistics, regression, anomalies).

use super::{ToolError, ToolResult};

fn sorted(values: &[f64]) -> Vec<f64> {
    let mut copy = values.to_vec();
    copy.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    copy
}

fn non_empty(values: &[f64], name: &str) -> ToolResult<()> {
    if values.is_empty() {
        return Err(ToolError::Other(format!(
            "{name}: cannot operate on an empty array"
        )));
    }
    Ok(())
}

/// Linear-interpolated percentile (0–100).
pub fn percentile(values: &[f64], percent: f64) -> ToolResult<f64> {
    non_empty(values, "percentile")?;
    if !percent.is_finite()
        || !(0.0..=100.0).contains(&percent)
        || values.iter().any(|v| !v.is_finite())
    {
        return Err(ToolError::Invalid(
            "Percentile requires finite observations and a percentage from 0 to 100.".into(),
        ));
    }
    let s = sorted(values);
    let position = ((s.len() - 1) as f64 * percent) / 100.0;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    Ok(s[lower] + (s[upper] - s[lower]) * (position - lower as f64))
}

/// Circular mean and standard deviation of clock times in minutes (0–1440).
pub fn circular_stats(minutes: &[f64]) -> ToolResult<(Option<f64>, Option<f64>)> {
    if minutes.is_empty() {
        return Ok((None, None));
    }
    if minutes.iter().any(|v| !v.is_finite()) {
        return Err(ToolError::Invalid("Clock times must be finite.".into()));
    }
    let radians: Vec<f64> = minutes
        .iter()
        .map(|v| v * std::f64::consts::PI / 720.0)
        .collect();
    let cosine = mean(&radians.iter().map(|r| r.cos()).collect::<Vec<_>>())?;
    let sine = mean(&radians.iter().map(|r| r.sin()).collect::<Vec<_>>())?;
    let length = cosine.hypot(sine).min(1.0);
    if length < 1e-10 {
        return Ok((None, None));
    }
    let mean_minutes = ((sine.atan2(cosine) * 720.0) / std::f64::consts::PI + 1440.0) % 1440.0;
    let sd = ((-2.0 * length.ln()).sqrt() * 720.0) / std::f64::consts::PI;
    Ok((Some(mean_minutes), Some(sd)))
}

/// Arithmetic mean.
pub fn mean(values: &[f64]) -> ToolResult<f64> {
    non_empty(values, "mean")?;
    Ok(values.iter().sum::<f64>() / values.len() as f64)
}

/// Median (average of the two middle values for even lengths).
pub fn median(values: &[f64]) -> ToolResult<f64> {
    non_empty(values, "median")?;
    let s = sorted(values);
    let mid = s.len() / 2;
    Ok(if s.len().is_multiple_of(2) {
        (s[mid - 1] + s[mid]) / 2.0
    } else {
        s[mid]
    })
}

/// Population standard deviation (0 for a single value).
pub fn standard_deviation(values: &[f64]) -> ToolResult<f64> {
    non_empty(values, "standardDeviation")?;
    if values.len() == 1 {
        return Ok(0.0);
    }
    let avg = mean(values)?;
    let sum: f64 = values.iter().map(|v| (v - avg) * (v - avg)).sum();
    Ok((sum / values.len() as f64).sqrt())
}

/// Simple linear regression on indices `0..n`: `(slope, r2)`.
pub fn linear_regression(values: &[f64]) -> ToolResult<(f64, f64)> {
    non_empty(values, "linearRegression")?;
    let n = values.len() as f64;
    if values.len() == 1 {
        return Ok((0.0, 0.0));
    }
    let sum_x = (n * (n - 1.0)) / 2.0;
    let sum_x2 = (n * (n - 1.0) * (2.0 * n - 1.0)) / 6.0;
    let mut sum_y = 0.0;
    let mut sum_xy = 0.0;
    for (i, v) in values.iter().enumerate() {
        sum_y += v;
        sum_xy += i as f64 * v;
    }
    let denominator = n * sum_x2 - sum_x * sum_x;
    if denominator == 0.0 {
        return Ok((0.0, 0.0));
    }
    let slope = (n * sum_xy - sum_x * sum_y) / denominator;
    let intercept = (sum_y - slope * sum_x) / n;
    let y_mean = sum_y / n;
    let mut ss_tot = 0.0;
    let mut ss_res = 0.0;
    for (i, v) in values.iter().enumerate() {
        ss_tot += (v - y_mean) * (v - y_mean);
        let residual = v - (intercept + slope * i as f64);
        ss_res += residual * residual;
    }
    let r2 = if ss_tot == 0.0 {
        0.0
    } else {
        1.0 - ss_res / ss_tot
    };
    Ok((slope, r2))
}

/// Values more than `threshold` standard deviations from the mean: `(index, value, deviation)`.
pub fn detect_anomalies(values: &[f64], threshold: f64) -> ToolResult<Vec<(usize, f64, f64)>> {
    non_empty(values, "detectAnomalies")?;
    let avg = mean(values)?;
    let sd = standard_deviation(values)?;
    if sd == 0.0 {
        return Ok(Vec::new());
    }
    Ok(values
        .iter()
        .enumerate()
        .filter_map(|(i, v)| {
            let deviation = (v - avg).abs() / sd;
            (deviation > threshold).then_some((i, *v, deviation))
        })
        .collect())
}

/// Classify a trend: stable unless `r2 > 0.4` and `|slope| >= 0.001`.
pub fn trend_direction(slope: f64, r2: f64) -> &'static str {
    if r2 <= 0.4 || slope.abs() < 0.001 {
        return "stable";
    }
    if slope > 0.0 {
        "improving"
    } else {
        "declining"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_statistics() {
        assert_eq!(mean(&[1.0, 2.0, 3.0]).unwrap(), 2.0);
        assert_eq!(median(&[3.0, 1.0, 2.0, 4.0]).unwrap(), 2.5);
        assert_eq!(
            standard_deviation(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]).unwrap(),
            2.0
        );
        assert_eq!(standard_deviation(&[5.0]).unwrap(), 0.0);
        assert!(mean(&[]).is_err());
    }

    #[test]
    fn percentiles_interpolate() {
        let values = [10.0, 20.0, 30.0, 40.0];
        assert_eq!(percentile(&values, 50.0).unwrap(), 25.0);
        assert_eq!(percentile(&values, 0.0).unwrap(), 10.0);
        assert_eq!(percentile(&values, 100.0).unwrap(), 40.0);
        assert!(matches!(
            percentile(&values, 101.0),
            Err(ToolError::Invalid(_))
        ));
    }

    #[test]
    fn regression_and_trends() {
        let (slope, r2) = linear_regression(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert!((slope - 1.0).abs() < 1e-12 && (r2 - 1.0).abs() < 1e-12);
        assert_eq!(linear_regression(&[5.0, 5.0, 5.0]).unwrap(), (0.0, 0.0));
        assert_eq!(trend_direction(1.0, 0.9), "improving");
        assert_eq!(trend_direction(-1.0, 0.9), "declining");
        assert_eq!(trend_direction(1.0, 0.3), "stable");
        assert_eq!(trend_direction(0.0005, 0.9), "stable");
    }

    #[test]
    fn anomalies_and_circular_stats() {
        let anomalies = detect_anomalies(&[10.0, 10.0, 10.0, 10.0, 10.0, 50.0], 2.0).unwrap();
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].0, 5);
        let (m, sd) = circular_stats(&[1430.0, 10.0]).unwrap();
        assert!((m.unwrap() - 0.0).abs() < 1e-9 || (m.unwrap() - 1440.0).abs() < 1e-9);
        assert!(sd.unwrap() > 0.0);
        assert_eq!(circular_stats(&[]).unwrap(), (None, None));
        assert_eq!(circular_stats(&[0.0, 720.0]).unwrap(), (None, None));
    }
}
