#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latency_window_includes_50_and_excludes_51_ms() {
        let r = vec![
            ("low".to_string(), 10, 1.0),
            ("edge".to_string(), 60, 2.0),
            ("outside".to_string(), 61, 100.0),
        ];
        assert_eq!(choose_winner(&r).unwrap().0, "edge");
        assert!(choose_winner(&[("bad".into(), 0, 100.0)]).is_none());
    }
}

use super::config::Config;
use futures_util::{StreamExt, stream};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::Path,
    time::{Duration, Instant},
};
use tokio::{
    process::Command,
    time::{sleep, timeout},
};
use yaml_rust2::YamlLoader;
type Result<T> = std::result::Result<T, String>;
type Reading = (String, u64, f64);

fn log(path: &Path, code: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
        let _ = writeln!(f, "{} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), code);
    }
}
fn choose_winner(values: &[Reading]) -> Option<Reading> {
    let valid: Vec<_> = values
        .iter()
        .filter(|r| r.1 > 0 && r.2.is_finite() && r.2 > 0.0)
        .collect();
    let min = valid.iter().map(|r| r.1).min()?;
    valid
        .into_iter()
        .filter(|r| r.1 <= min.saturating_add(50))
        .min_by(|a, b| b.2.total_cmp(&a.2).then(a.1.cmp(&b.1)).then(a.0.cmp(&b.0)))
        .cloned()
}
fn providers(path: &Path, group: &str) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(path).map_err(|_| "mihomo_config_unavailable")?;
    let docs = YamlLoader::load_from_str(&raw).map_err(|_| "invalid_mihomo_yaml")?;
    let doc = docs.first().ok_or("invalid_mihomo_yaml")?;
    let groups = doc["proxy-groups"].as_vec().ok_or("invalid_mihomo_groups")?;
    let defs = doc["proxy-providers"].as_hash().ok_or("invalid_mihomo_providers")?;
    let matched: Vec<_> = groups.iter().filter(|g| g["name"].as_str() == Some(group)).collect();
    if matched.len() != 1 {
        return Err("invalid_target_group_definition".into());
    }
    let uses = matched[0]["use"]
        .as_vec()
        .filter(|v| !v.is_empty())
        .ok_or("invalid_target_provider_scope")?;
    let mut found = Vec::new();
    for item in uses {
        let name = item
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("invalid_target_provider_scope")?;
        if !defs.get(item).is_some_and(|v| v.as_hash().is_some()) {
            return Err("invalid_target_provider_scope".into());
        }
        if !found.iter().any(|s| s == name) {
            found.push(name.to_string());
        }
    }
    Ok(found)
}

#[derive(Clone)]
struct Api {
    http: Client,
    base: String,
    secret: String,
}
impl Api {
    fn new(cfg: &Config) -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|_| "controller_client_failed")?,
            base: cfg.controller.trim_end_matches('/').to_string(),
            secret: cfg.secret.clone(),
        })
    }
    async fn request(&self, method: Method, path: &str, body: Option<Value>, seconds: f64) -> Result<Value> {
        let mut req = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .timeout(Duration::from_secs_f64(seconds));
        if !self.secret.is_empty() {
            req = req.bearer_auth(&self.secret);
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req
            .send()
            .await
            .map_err(|_| "controller_unavailable")?
            .error_for_status()
            .map_err(|_| "controller_http_error")?;
        let bytes = response.bytes().await.map_err(|_| "controller_read_failed")?;
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(&bytes).map_err(|_| "controller_invalid_response".into())
        }
    }
    async fn group(&self, name: &str) -> Result<Value> {
        self.request(
            Method::GET,
            &format!("/proxies/{}", urlencoding::encode(name)),
            None,
            10.0,
        )
        .await
    }
    async fn current(&self, name: &str) -> Result<String> {
        self.group(name).await?["now"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or("invalid_current_selection".into())
    }
    async fn select(&self, group: &str, node: &str, seconds: u64) -> Result<()> {
        self.request(
            Method::PUT,
            &format!("/proxies/{}", urlencoding::encode(group)),
            Some(json!({"name":node})),
            seconds as f64,
        )
        .await?;
        Ok(())
    }
    async fn provider(&self, name: &str) -> Result<Value> {
        self.request(
            Method::GET,
            &format!("/providers/proxies/{}", urlencoding::encode(name)),
            None,
            10.0,
        )
        .await
    }
    async fn refresh(&self, names: &[String], log_path: &Path) {
        for name in names {
            for attempt in 0..5 {
                if self
                    .request(
                        Method::PUT,
                        &format!("/providers/proxies/{}", urlencoding::encode(name)),
                        None,
                        30.0,
                    )
                    .await
                    .is_ok()
                {
                    break;
                }
                log(log_path, "provider_refresh_failed");
                if attempt < 4 {
                    sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    async fn check(&self, node: &Node, cfg: &Config) -> Option<u64> {
        let path = format!(
            "/providers/proxies/{}/{}/healthcheck?url={}&timeout={}{}",
            urlencoding::encode(&node.provider),
            urlencoding::encode(&node.name),
            urlencoding::encode(&node.test_url),
            cfg.healthcheck_timeout_ms,
            node.expected.map(|n| format!("&expected={n}")).unwrap_or_default()
        );
        self.request(
            Method::GET,
            &path,
            None,
            cfg.healthcheck_timeout_ms as f64 / 1000.0 + 1.0,
        )
        .await
        .ok()?["delay"]
            .as_u64()
            .filter(|n| *n > 0)
    }
}

#[derive(Clone, Debug)]
struct Node {
    name: String,
    provider: String,
    test_url: String,
    expected: Option<i64>,
    last_delay: Option<u64>,
}
fn provider_nodes(data: &Value, provider: &str) -> Result<Vec<Node>> {
    let test_url = data["testUrl"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("invalid_provider_test_config")?;
    let expected = if data["expectedStatus"].is_null() {
        None
    } else {
        Some(
            data["expectedStatus"]
                .as_i64()
                .or_else(|| data["expectedStatus"].as_str()?.parse().ok())
                .ok_or("invalid_provider_test_config")?,
        )
    };
    Ok(data["proxies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let name = p["name"].as_str().filter(|s| !s.trim().is_empty())?;
            Some(Node {
                name: name.into(),
                provider: provider.into(),
                test_url: test_url.into(),
                expected,
                last_delay: p["history"]
                    .as_array()
                    .and_then(|a| a.last())
                    .and_then(|v| v["delay"].as_u64())
                    .filter(|n| *n > 0),
            })
        })
        .collect())
}
fn eligible_members(group: &Value) -> Result<Vec<String>> {
    if group["type"] != "Selector" {
        return Err("invalid_target_selector".into());
    }
    let members = group["all"].as_array().ok_or("invalid_target_members")?;
    let mut seen = HashSet::new();
    Ok(members
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .filter(|s| {
            let lower = s.to_lowercase();
            !lower.contains("россия") && !lower.contains("украина") && seen.insert(s.to_string())
        })
        .map(str::to_string)
        .collect())
}
async fn candidates(api: &Api, cfg: &Config, names: &[String], log_path: &Path) -> Result<Vec<Node>> {
    let members = eligible_members(&api.group(&cfg.target_group).await?)?;
    let wanted: HashSet<_> = members.iter().collect();
    let mut nodes = HashMap::new();
    for name in names {
        match api.provider(name).await.and_then(|d| provider_nodes(&d, name)) {
            Ok(items) => {
                for n in items {
                    if wanted.contains(&n.name) {
                        nodes.entry(n.name.clone()).or_insert(n);
                    }
                }
            }
            Err(_) => log(log_path, "provider_nodes_unavailable"),
        }
    }
    Ok(members.iter().filter_map(|n| nodes.remove(n)).collect())
}
async fn checked(api: &Api, cfg: &Config, nodes: &[Node], restore: bool, log_path: &Path) -> Result<Vec<(Node, u64)>> {
    let mut requests = stream::iter(nodes.iter().cloned().map(|n| async move {
        let delay = api.check(&n, cfg).await;
        (n, delay)
    }))
    .buffer_unordered(cfg.healthcheck_parallelism);
    let mut result = Vec::new();
    let mut restored = false;
    while let Some((node, delay)) = requests.next().await {
        if let Some(delay) = delay {
            if restore && !restored {
                api.select(&cfg.target_group, &node.name, 5).await?;
                restored = true;
                log(log_path, "emergency_fallback_selected");
            }
            result.push((node, delay));
        }
    }
    result.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.name.cmp(&b.0.name)));
    Ok(result)
}

async fn download(client: Client, size: u64) -> Result<u64> {
    let url = format!(
        "https://speed.cloudflare.com/__down?bytes={size}&cache_bust={}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    timeout(Duration::from_secs(60), async {
        let response = client
            .get(url)
            .header("Cache-Control", "no-cache")
            .send()
            .await
            .map_err(|_| "download_network_error")?
            .error_for_status()
            .map_err(|_| "download_http_error")?;
        let mut chunks = response.bytes_stream();
        let mut received = 0;
        while received < size {
            let Some(chunk) = chunks.next().await else {
                break;
            };
            received += std::cmp::min(
                chunk.map_err(|_| "download_network_error")?.len() as u64,
                size - received,
            );
        }
        if received < size * 8 / 10 {
            return Err("download_incomplete".into());
        }
        Ok(received)
    })
    .await
    .map_err(|_| "download_timeout".to_string())?
}
async fn speedtest(proxy: &str) -> Result<f64> {
    let mut cmd = Command::new("/opt/bin/speedtest-go");
    cmd.args([
        format!("--proxy={proxy}"),
        "--ping-mode=http".into(),
        "--no-upload".into(),
        "--json".into(),
    ])
    .kill_on_drop(true)
    .stdin(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped());
    // The kernel kills the direct child even if the panel dies before Drop runs.
    #[cfg(target_os = "linux")]
    unsafe {
        let parent = nix::libc::getpid();
        cmd.pre_exec(move || {
            if nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, nix::libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if nix::libc::getppid() != parent {
                return Err(std::io::Error::other("parent_exited"));
            }
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            "speedtest_unavailable"
        } else {
            "speedtest_failed"
        }
    })?;
    let output = timeout(Duration::from_secs(180), child.wait_with_output())
        .await
        .map_err(|_| "speedtest_timeout")?
        .map_err(|_| "speedtest_failed")?;
    if !output.status.success() {
        return Err("speedtest_failed".into());
    }
    parse_speedtest(&output.stdout)
}
fn parse_speedtest(raw: &[u8]) -> Result<f64> {
    let value: Value = serde_json::from_slice(raw).map_err(|_| "speedtest_invalid_result")?;
    let servers = value["servers"]
        .as_array()
        .filter(|a| a.len() == 1)
        .ok_or("speedtest_invalid_result")?;
    let speed = servers[0]["dl_speed"].as_f64().ok_or("speedtest_invalid_result")? * 8.0 / 1_000_000.0;
    if !speed.is_finite() || speed <= 0.0 {
        return Err("speedtest_invalid_result".into());
    }
    Ok(speed)
}
async fn throughput(api: &Api, cfg: &Config, node: &str) -> Result<f64> {
    api.select(&cfg.benchmark_group, node, cfg.speed_connect_timeout_seconds)
        .await?;
    sleep(Duration::try_from_secs_f64(cfg.switch_wait_seconds).map_err(|_| "invalid_switch_wait_seconds")?).await;
    if cfg.speed_test_provider == "speedtest" {
        return speedtest(&cfg.benchmark_proxy).await;
    }
    let client = Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(&cfg.benchmark_proxy).map_err(|_| "invalid_benchmark_proxy")?)
        .connect_timeout(Duration::from_secs(cfg.speed_connect_timeout_seconds))
        .read_timeout(Duration::from_secs(cfg.speed_connect_timeout_seconds))
        .user_agent("router-watchdog/native")
        .build()
        .map_err(|_| "download_failed")?;
    let mut values = Vec::new();
    for _ in 0..cfg.multi_rounds {
        let start = Instant::now();
        let mut tasks = stream::iter((0..cfg.parallel_streams).map(|_| download(client.clone(), cfg.download_bytes)))
            .buffer_unordered(cfg.parallel_streams);
        let mut bytes = 0;
        while let Some(result) = tasks.next().await {
            bytes += result?;
        }
        values.push(bytes as f64 * 8.0 / start.elapsed().as_secs_f64() / 1_000_000.0);
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    let result = if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    };
    if !result.is_finite() || result <= 0.0 {
        return Err("download_failed".into());
    }
    Ok(result)
}
fn benchmark_members(value: &Value) -> Result<HashSet<String>> {
    if value["type"] != "Selector" {
        return Err("invalid_benchmark_selector".into());
    }
    Ok(value["all"]
        .as_array()
        .ok_or("invalid_benchmark_members")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect())
}
async fn benchmark(api: &Api, cfg: &Config, nodes: &[(Node, u64)], log_path: &Path) -> Result<Option<Reading>> {
    let members = benchmark_members(&api.group(&cfg.benchmark_group).await?)?;
    let mut results = Vec::new();
    for (node, delay) in nodes {
        if !members.contains(&node.name) {
            continue;
        }
        match throughput(api, cfg, &node.name).await {
            Ok(speed) => results.push((node.name.clone(), *delay, speed)),
            Err(_) => log(log_path, "candidate_speed_failed"),
        }
    }
    Ok(choose_winner(&results))
}

pub async fn automatic(cfg: &Config, mihomo_path: &Path, log_path: &Path) -> Result<()> {
    cfg.validate()?;
    let api = Api::new(cfg)?;
    let scope = providers(mihomo_path, &cfg.target_group)?;
    log(log_path, "automatic_started");
    if let Ok(current) = api.current(&cfg.target_group).await {
        for name in &scope {
            if let Ok(nodes) = api.provider(name).await.and_then(|d| provider_nodes(&d, name)) {
                if let Some(node) = nodes.iter().find(|n| n.name == current) {
                    if api.check(node, cfg).await.is_some() {
                        log(log_path, "current_healthy");
                        return Ok(());
                    }
                    break;
                }
            }
        }
    }
    let mut nodes = candidates(&api, cfg, &scope, log_path).await?;
    let mut restored = false;
    for n in &nodes {
        if api.check(n, cfg).await.is_some() {
            api.select(&cfg.target_group, &n.name, 5).await?;
            restored = true;
            log(log_path, "emergency_fallback_selected");
            break;
        }
    }
    let mut winner = None;
    if restored {
        let mut shortlist: Vec<_> = nodes.iter().filter(|n| n.last_delay.is_some()).cloned().collect();
        shortlist.sort_by(|a, b| a.last_delay.cmp(&b.last_delay).then(a.name.cmp(&b.name)));
        shortlist.truncate(cfg.top_n);
        let checked = checked(&api, cfg, &shortlist, false, log_path).await?;
        if !checked.is_empty() {
            winner = benchmark(&api, cfg, &checked, log_path).await?;
        }
    }
    if winner.is_none() {
        api.refresh(&scope, log_path).await;
        nodes = candidates(&api, cfg, &scope, log_path).await?;
        let mut checked = checked(&api, cfg, &nodes, true, log_path).await?;
        if checked.is_empty() {
            return Err("no_available_nodes_after_refresh".into());
        }
        checked.truncate(cfg.top_n);
        winner = benchmark(&api, cfg, &checked, log_path).await?;
    }
    let winner = winner.ok_or("speed_failed_fallback_retained")?;
    api.select(&cfg.target_group, &winner.0, 5).await?;
    log(log_path, "winner_selected");
    Ok(())
}

fn record(r: &Reading) -> Value {
    json!({"node":r.0,"delay_ms":r.1,"mbps":r.2})
}
pub async fn measure(
    cfg: &Config, mihomo_path: &Path, log_path: &Path, find_faster: bool, mcp_profile: bool, apply: bool,
) -> Result<Value> {
    if apply && (!find_faster || mcp_profile) {
        return Err("invalid_measure_options".into());
    }
    let mut cfg = cfg.clone();
    if mcp_profile && cfg.speed_test_provider == "cloudflare" {
        cfg.download_bytes = 10_000_000;
        cfg.parallel_streams = 4;
        cfg.multi_rounds = 1;
    }
    cfg.validate()?;
    let api = Api::new(&cfg)?;
    let selected = api.current(&cfg.target_group).await?;
    let scope = providers(mihomo_path, &cfg.target_group)?;
    let nodes = candidates(&api, &cfg, &scope, log_path).await?;
    let baseline = nodes
        .iter()
        .find(|n| n.name == selected)
        .ok_or("current_not_eligible")?;
    let members = benchmark_members(&api.group(&cfg.benchmark_group).await?)?;
    if !members.contains(&selected) {
        return Err("current_not_in_benchmark".into());
    }
    api.select(&cfg.benchmark_group, &selected, cfg.speed_connect_timeout_seconds)
        .await?;
    if api.current(&cfg.benchmark_group).await? != selected {
        return Err("benchmark_sync_failed".into());
    }
    let delay = api.check(baseline, &cfg).await.ok_or("current_healthcheck_failed")?;
    let current = (selected.clone(), delay, throughput(&api, &cfg, &selected).await?);
    let mut results = vec![current.clone()];
    let mut failed = Vec::new();
    if find_faster {
        let others: Vec<_> = nodes
            .iter()
            .filter(|n| n.name != selected && members.contains(&n.name))
            .cloned()
            .collect();
        let mut checked = checked(&api, &cfg, &others, false, log_path).await?;
        checked.truncate(cfg.top_n);
        for (node, delay) in checked {
            match throughput(&api, &cfg, &node.name).await {
                Ok(speed) => results.push((node.name, delay, speed)),
                Err(_) => {
                    log(log_path, "candidate_speed_failed");
                    failed.push(node.name);
                }
            }
        }
    }
    let winner = choose_winner(&results).ok_or("no_speed_results")?;
    let faster = winner.2 > current.2;
    if api.current(&cfg.target_group).await? != selected {
        return Err("target_changed_during_measurement".into());
    }
    let applied = apply && faster;
    if applied {
        let group = api.group(&cfg.target_group).await?;
        if !eligible_members(&group)?.contains(&winner.0) {
            return Err("winner_no_longer_eligible".into());
        }
        api.select(&cfg.target_group, &winner.0, 5).await?;
        if api.current(&cfg.target_group).await? != winner.0 {
            return Err("selection_verification_failed".into());
        }
    }
    Ok(
        json!({"group":cfg.target_group,"measurement":"download_throughput","unit":"Mbps","measured_at":chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),"current":record(&current),"winner":record(&winner),"results":results.iter().map(record).collect::<Vec<_>>(),"failed_speed_nodes":failed,"faster":faster,"improvement_percent":(winner.2/current.2-1.0)*100.0,"applied":applied,"selected_after":if applied {winner.0} else {selected},"latency_window_ms":50}),
    )
}

pub async fn select(
    cfg: &Config, mihomo_path: &Path, log_path: &Path, node: &str, expected_current: &str,
) -> Result<Value> {
    if node.trim().is_empty() || expected_current.trim().is_empty() {
        return Err("invalid_selection_request".into());
    }
    cfg.validate()?;
    let api = Api::new(cfg)?;
    if api.current(&cfg.target_group).await? != expected_current {
        return Err("target_selection_changed".into());
    }
    let scope = providers(mihomo_path, &cfg.target_group)?;
    let nodes = candidates(&api, cfg, &scope, log_path).await?;
    let candidate = nodes
        .iter()
        .find(|n| n.name == node)
        .ok_or("requested_node_not_eligible")?;
    api.check(candidate, cfg)
        .await
        .ok_or("requested_node_healthcheck_failed")?;
    let group = api.group(&cfg.target_group).await?;
    if api.current(&cfg.target_group).await? != expected_current {
        return Err("target_selection_changed".into());
    }
    if !eligible_members(&group)?.iter().any(|n| n == node) {
        return Err("requested_node_not_eligible".into());
    }
    let applied = node != expected_current;
    if applied {
        api.select(&cfg.target_group, node, 5).await?;
    }
    if api.current(&cfg.target_group).await? != node {
        return Err("selection_verification_failed".into());
    }
    log(log_path, "manual_selection_complete");
    Ok(json!({"group":cfg.target_group,"applied":applied,"selected_after":node}))
}

#[cfg(test)]
mod behavior_tests {
    use super::*;
    use axum::{
        Router,
        body::to_bytes,
        extract::{Request, State},
        response::IntoResponse,
    };
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    struct MockState {
        current: String,
        events: Vec<String>,
        healthy: bool,
    }
    async fn handle(State(state): State<Arc<Mutex<MockState>>>, request: Request) -> impl IntoResponse {
        let method = request.method().clone();
        let uri = request.uri().to_string();
        let body = to_bytes(request.into_body(), 4096).await.unwrap();
        let mut s = state.lock().unwrap();
        s.events.push(format!("{method} {uri}"));
        let path = uri.split('?').next().unwrap();
        let response = match (method.as_str(), path) {
            ("GET", "/proxies/production") => {
                json!({"type":"Selector","now":s.current,"all":["node","other","Россия","DIRECT","node"]})
            }
            ("PUT", "/proxies/production") => {
                s.current = serde_json::from_slice::<Value>(&body).unwrap()["name"]
                    .as_str()
                    .unwrap()
                    .into();
                Value::Null
            }
            ("GET", "/proxies/BENCHMARK") => json!({"type":"Selector","now":"node","all":[]}),
            ("GET", "/providers/proxies/owned") => {
                json!({"testUrl":"https://example.invalid/check","expectedStatus":204,"proxies":[{"name":"node","history":[{"delay":10},{"delay":0}]},{"name":"other","history":[{"delay":30}]},{"name":"Россия"},{"name":"outside"}]})
            }
            ("GET", "/providers/proxies/owned/node/healthcheck") => json!({"delay":if s.healthy {10} else {0}}),
            ("GET", "/providers/proxies/owned/other/healthcheck") => json!({"delay":20}),
            _ => Value::Null,
        };
        axum::Json(response)
    }
    struct Fixture {
        root: std::path::PathBuf,
        task: tokio::task::JoinHandle<()>,
        cfg: Config,
        state: Arc<Mutex<MockState>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    impl Fixture {
        async fn new(healthy: bool) -> Self {
            let root = std::env::temp_dir().join(format!("native-engine-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("config.yaml"),"proxy-groups:\n  - name: production\n    use: [owned]\nproxy-providers:\n  owned: {}\n  unrelated: {}\n").unwrap();
            let state = Arc::new(Mutex::new(MockState {
                current: "node".into(),
                healthy,
                ..Default::default()
            }));
            let app = Router::new().fallback(handle).with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                root,
                task,
                cfg: Config {
                    controller: format!("http://{address}"),
                    target_group: "production".into(),
                    ..Default::default()
                },
                state,
            }
        }
    }
    #[tokio::test]
    async fn healthy_current_exits_after_owned_check_without_benchmark_or_refresh() {
        let f = Fixture::new(true).await;
        automatic(&f.cfg, &f.root.join("config.yaml"), &f.root.join("log"))
            .await
            .unwrap();
        let s = f.state.lock().unwrap();
        assert_eq!(s.events.len(), 3);
        assert!(
            s.events[2].contains(
                "/owned/node/healthcheck?url=https%3A%2F%2Fexample.invalid%2Fcheck&timeout=5000&expected=204"
            )
        );
        assert!(
            !s.events
                .iter()
                .any(|v| v.contains("BENCHMARK") || v.contains("unrelated") || v.starts_with("PUT"))
        );
    }
    #[tokio::test]
    async fn fallback_precedes_benchmarks_and_refresh_scope_stays_owned() {
        let f = Fixture::new(false).await;
        assert_eq!(
            automatic(&f.cfg, &f.root.join("config.yaml"), &f.root.join("log"))
                .await
                .unwrap_err(),
            "speed_failed_fallback_retained"
        );
        let s = f.state.lock().unwrap();
        assert_eq!(s.current, "other");
        let fallback = s.events.iter().position(|v| v == "PUT /proxies/production").unwrap();
        let benchmark = s.events.iter().position(|v| v == "GET /proxies/BENCHMARK").unwrap();
        assert!(fallback < benchmark);
        assert_eq!(
            s.events.iter().filter(|v| *v == "PUT /providers/proxies/owned").count(),
            1
        );
        assert!(
            !s.events.iter().any(|v| v.contains("unrelated")
                || v.contains("outside/healthcheck")
                || v.contains("DIRECT/healthcheck"))
        );
    }
    #[tokio::test]
    async fn candidates_only_include_owned_concrete_nonexcluded_group_members() {
        let f = Fixture::new(true).await;
        let api = Api::new(&f.cfg).unwrap();
        let nodes = candidates(&api, &f.cfg, &["owned".into()], &f.root.join("log"))
            .await
            .unwrap();
        assert_eq!(
            nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            vec!["node", "other"]
        );
        assert_eq!(nodes[0].last_delay, None);
    }
    #[tokio::test]
    async fn explicit_selection_rejects_changed_baseline_before_any_write() {
        let f = Fixture::new(true).await;
        assert_eq!(
            select(
                &f.cfg,
                &f.root.join("config.yaml"),
                &f.root.join("log"),
                "other",
                "stale"
            )
            .await
            .unwrap_err(),
            "target_selection_changed"
        );
        assert!(f.state.lock().unwrap().events.iter().all(|e| !e.starts_with("PUT")));
    }
    #[test]
    fn speedtest_parser_rejects_invalid_results_and_converts_bytes_to_mbps() {
        assert_eq!(parse_speedtest(br#"{"servers":[{"dl_speed":1000000}]}"#).unwrap(), 8.0);
        for raw in [
            br#"{"servers":[]}"#.as_slice(),
            br#"{"servers":[{"dl_speed":true}]}"#,
            br#"{"servers":[{"dl_speed":0}]}"#,
        ] {
            assert!(parse_speedtest(raw).is_err());
        }
    }
}
