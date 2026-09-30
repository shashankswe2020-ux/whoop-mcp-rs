//! Fixed-window, per-client rate limiting with `express-rate-limit` semantics
//! (draft-6 `RateLimit-*` headers, IPv6 /56 keying).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;

/// Outcome of counting one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateInfo {
    pub limited: bool,
    pub headers: Vec<(String, String)>,
}

/// Fixed-window limiter keyed by client.
pub struct RateLimiter {
    window_ms: f64,
    limit: u64,
    hits: Mutex<HashMap<String, (u64, f64)>>,
}

impl RateLimiter {
    pub fn new(window_ms: u64, limit: u64) -> Self {
        Self {
            window_ms: window_ms as f64,
            limit,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Count a request for `key` at `now` (epoch ms).
    pub fn hit_at(&self, key: &str, now: f64) -> RateInfo {
        let mut hits = self
            .hits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hits.retain(|_, (_, reset)| *reset > now);
        let entry = hits
            .entry(key.to_string())
            .or_insert((0, now + self.window_ms));
        entry.0 += 1;
        let (total, reset_at) = *entry;
        let reset_seconds = ((reset_at - now) / 1000.0).ceil().max(0.0) as u64;
        let limited = total > self.limit;
        let mut headers = vec![
            (
                "RateLimit-Policy".to_string(),
                format!(
                    "{};w={}",
                    self.limit,
                    (self.window_ms / 1000.0).ceil() as u64
                ),
            ),
            ("RateLimit-Limit".to_string(), self.limit.to_string()),
            (
                "RateLimit-Remaining".to_string(),
                self.limit.saturating_sub(total).to_string(),
            ),
            ("RateLimit-Reset".to_string(), reset_seconds.to_string()),
        ];
        if limited {
            headers.push(("Retry-After".to_string(), reset_seconds.to_string()));
        }
        RateInfo { limited, headers }
    }

    /// Count a request now.
    pub fn hit(&self, key: &str) -> RateInfo {
        self.hit_at(key, crate::logging::now_ms())
    }
}

/// `ipKeyGenerator`: IPv4 as-is, IPv6 masked to its /56 subnet.
pub fn ip_key(ip: &str) -> String {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => {
            let masked = u128::from(v6) & (!0u128 << (128 - 56));
            format!("{}/56", std::net::Ipv6Addr::from(masked))
        }
        _ => ip.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_after_threshold_and_resets() {
        let limiter = RateLimiter::new(60_000, 2);
        assert!(!limiter.hit_at("a", 0.0).limited);
        let second = limiter.hit_at("a", 1000.0);
        assert!(!second.limited);
        assert!(
            second
                .headers
                .contains(&("RateLimit-Remaining".into(), "0".into()))
        );
        assert!(
            second
                .headers
                .contains(&("RateLimit-Reset".into(), "59".into()))
        );
        let third = limiter.hit_at("a", 2000.0);
        assert!(third.limited);
        assert!(third.headers.iter().any(|(k, _)| k == "Retry-After"));
        assert!(!limiter.hit_at("b", 2000.0).limited);
        assert!(!limiter.hit_at("a", 60_001.0).limited);
    }

    #[test]
    fn masks_ipv6_subnets() {
        assert_eq!(ip_key("203.0.113.9"), "203.0.113.9");
        assert_eq!(ip_key("2001:db8:1234:5678::1"), "2001:db8:1234:5600::/56");
    }
}
