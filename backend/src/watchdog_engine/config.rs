#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn rejects_invalid_patch_and_preserves_unknown_existing_fields() {
        let current =
            json!({"controller":"http://127.0.0.1:9090","target_group":"production","secret":"private","legacy":42});
        let merged = validate_patch(&current, &json!({"top_n":3})).unwrap();
        assert_eq!(merged["legacy"], 42);
        assert_eq!(merged["secret"], "private");
        assert!(validate_patch(&current, &json!({"new_unknown":true})).is_err());
        assert!(validate_patch(&current, &json!({"top_n":true})).is_err());
        assert!(validate_patch(&current, &json!({"benchmark_group":"production"})).is_err());
    }
    #[test]
    fn public_view_redacts_secret_and_legacy_fields() {
        let c:Config=serde_json::from_value(json!({"controller":"http://localhost:9090","target_group":"production","secret":"private","speed_test_url":"private"})).unwrap();
        c.validate().unwrap();
        let view = c.public_view();
        assert_eq!(view["secret_configured"], true);
        assert!(view["config"].get("secret").is_none());
        assert!(view["config"].get("speed_test_url").is_none());
    }
}

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

pub const EDITABLE_FIELDS: &[&str] = &[
    "controller",
    "secret",
    "target_group",
    "benchmark_group",
    "benchmark_proxy",
    "top_n",
    "healthcheck_parallelism",
    "healthcheck_timeout_ms",
    "speed_connect_timeout_seconds",
    "switch_wait_seconds",
    "speed_test_provider",
    "download_bytes",
    "parallel_streams",
    "multi_rounds",
    "log_cleanup_days",
    "schedule",
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub controller: String,
    pub secret: String,
    pub target_group: String,
    pub benchmark_group: String,
    pub benchmark_proxy: String,
    pub top_n: usize,
    pub healthcheck_parallelism: usize,
    pub healthcheck_timeout_ms: u64,
    pub speed_connect_timeout_seconds: u64,
    pub switch_wait_seconds: f64,
    pub speed_test_provider: String,
    pub download_bytes: u64,
    pub parallel_streams: usize,
    pub multi_rounds: usize,
    pub log_cleanup_days: u64,
    pub schedule: Value,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            controller: "http://127.0.0.1:9090".into(),
            secret: String::new(),
            target_group: String::new(),
            benchmark_group: "BENCHMARK".into(),
            benchmark_proxy: "http://127.0.0.1:17890".into(),
            top_n: 7,
            healthcheck_parallelism: 6,
            healthcheck_timeout_ms: 5000,
            speed_connect_timeout_seconds: 5,
            switch_wait_seconds: 0.7,
            speed_test_provider: "cloudflare".into(),
            download_bytes: 25_000_000,
            parallel_streams: 4,
            multi_rounds: 2,
            log_cleanup_days: 3,
            schedule: json!({"windows":[{"start":"07:00","end":"01:00","every_minutes":1},{"start":"01:00","end":"07:00","every_minutes":30}]}),
        }
    }
}

pub fn minute(value: &Value) -> Option<u64> {
    let s = value.as_str()?;
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' || ![b[0], b[1], b[3], b[4]].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let h = ((b[0] - b'0') * 10 + b[1] - b'0') as u64;
    let m = ((b[3] - b'0') * 10 + b[4] - b'0') as u64;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        for (key, text) in [("controller", &self.controller), ("benchmark_proxy", &self.benchmark_proxy)] {
            let url = reqwest::Url::parse(text).map_err(|_| format!("invalid_{key}"))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || text.chars().any(char::is_whitespace)
            {
                return Err(format!("invalid_{key}"));
            }
        }
        if self.target_group.trim().is_empty() {
            return Err("invalid_target_group".into());
        }
        if self.benchmark_group.trim().is_empty() || self.target_group == self.benchmark_group {
            return Err("invalid_benchmark_group".into());
        }
        for (key, n, lo, hi) in [
            ("top_n", self.top_n as u64, 1, 50),
            ("healthcheck_parallelism", self.healthcheck_parallelism as u64, 1, 32),
            ("healthcheck_timeout_ms", self.healthcheck_timeout_ms, 250, 10000),
            (
                "speed_connect_timeout_seconds",
                self.speed_connect_timeout_seconds,
                1,
                30,
            ),
            ("download_bytes", self.download_bytes, 1_000_000, 200_000_000),
            ("parallel_streams", self.parallel_streams as u64, 1, 8),
            ("multi_rounds", self.multi_rounds as u64, 1, 4),
            ("log_cleanup_days", self.log_cleanup_days, 1, 3650),
        ] {
            if !(lo..=hi).contains(&n) {
                return Err(format!("invalid_{key}"));
            }
        }
        if !self.switch_wait_seconds.is_finite() || self.switch_wait_seconds < 0.0 {
            return Err("invalid_switch_wait_seconds".into());
        }
        if !matches!(self.speed_test_provider.as_str(), "cloudflare" | "speedtest") {
            return Err("invalid_speed_test_provider".into());
        }
        let schedule = self.schedule.as_object().ok_or("invalid_schedule")?;
        if schedule.len() != 1 {
            return Err("invalid_schedule".into());
        }
        let windows = schedule
            .get("windows")
            .and_then(Value::as_array)
            .ok_or("invalid_schedule")?;
        for w in windows {
            if w.as_object().is_none_or(|m| m.len() != 3)
                || minute(&w["start"]).is_none()
                || minute(&w["end"]).is_none()
                || !w["every_minutes"].as_u64().is_some_and(|n| (1..=1440).contains(&n))
            {
                return Err("invalid_schedule".into());
            }
        }
        Ok(())
    }
    pub fn public_view(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or_else(|_| json!({}));
        value.as_object_mut().unwrap().remove("secret");
        let mut fields = EDITABLE_FIELDS.to_vec();
        fields.sort_unstable();
        json!({"config":value,"editable_fields":fields,"secret_configured":!self.secret.is_empty()})
    }
}

pub fn load(path: &Path) -> Result<Config, String> {
    let data = std::fs::read(path).map_err(|error| if error.kind() == std::io::ErrorKind::NotFound { "configuration_missing" } else { "config_unavailable" })?;
    let cfg: Config = serde_json::from_slice(&data).map_err(|_| "invalid_config")?;
    cfg.validate()?;
    Ok(cfg)
}

pub fn validate_patch(current: &Value, patch: &Value) -> Result<Value, String> {
    let p = patch
        .as_object()
        .filter(|m| !m.is_empty())
        .ok_or("invalid_config_patch")?;
    if p.keys().any(|k| !EDITABLE_FIELDS.contains(&k.as_str())) {
        return Err("invalid_config_patch".into());
    }
    let mut merged = current.as_object().cloned().ok_or("invalid_config")?;
    merged.extend(p.clone());
    let result = Value::Object(merged);
    let cfg: Config = serde_json::from_value(result.clone()).map_err(|_| "invalid_config")?;
    cfg.validate()?;
    Ok(result)
}
