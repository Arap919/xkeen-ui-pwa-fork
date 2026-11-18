use crate::{
    types::AppState,
    watchdog_engine::{self, config},
};
use axum::{Json, extract::State};
use chrono::{Local, Timelike};
use clap::{ArgGroup, Parser};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};

#[derive(Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub log: PathBuf,
    pub lock: PathBuf,
    pub mihomo: PathBuf,
    pub app: PathBuf,
    pub crontabs: Vec<PathBuf>,
    pub hook: PathBuf,
    pub cli: PathBuf,
    pub binary: PathBuf,
    pub cron_init: PathBuf,
}
impl Default for Paths {
    fn default() -> Self {
        let env_path = |name, fallback| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(fallback))
        };
        Self {
            config: env_path("ROUTER_WATCHDOG_CONFIG", "/opt/etc/router-watchdog.json"),
            log: env_path("ROUTER_WATCHDOG_LOG", "/opt/share/router-watchdog.log"),
            lock: "/tmp/router_watchdog.lock".into(),
            mihomo: crate::types::MIHOMO_CONF.into(),
            app: crate::types::APP_CONFIG.into(),
            crontabs: vec![
                env_path("ROUTER_WATCHDOG_CRONTAB", "/opt/etc/crontab"),
                "/opt/var/spool/cron/crontabs/root".into(),
                "/opt/etc/crontabs/root".into(),
            ],
            hook: "/opt/etc/cron.1min/router-watchdog".into(),
            cli: "/opt/bin/router-watchdog".into(),
            binary: "/opt/sbin/xkeen-ui".into(),
            cron_init: "/opt/etc/init.d/S10cron".into(),
        }
    }
}

pub struct Runtime {
    pub paths: Paths,
    started: AtomicBool,
    stopped: AtomicBool,
    wake: Notify,
    pub(crate) changes: Mutex<()>,
    job: Mutex<Option<JoinHandle<()>>>,
    cron_restart_pending: AtomicBool,
}
pub static RUNTIME: LazyLock<Arc<Runtime>> = LazyLock::new(|| Arc::new(Runtime::new(Paths::default())));
impl Runtime {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            started: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            wake: Notify::new(),
            changes: Mutex::new(()),
            job: Mutex::new(None),
            cron_restart_pending: AtomicBool::new(false),
        }
    }
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let runtime = self.clone();
        tokio::spawn(async move {
            loop {
                if runtime.stopped.load(Ordering::Acquire) {
                    break;
                }
                if let Err(code) = runtime.tick().await {
                    runtime.log(&format!("Native watchdog: {code}"));
                }
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                let delay = Duration::from_secs(60) - Duration::from_secs_f64(now.as_secs_f64() % 60.0);
                tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = runtime.wake.notified() => {} }
            }
        });
    }
    pub async fn shutdown(&self) {
        let _change = self.changes.lock().await;
        self.stopped.store(true, Ordering::Release);
        self.wake.notify_one();
        self.cancel().await;
    }
    pub async fn cancel(&self) {
        if let Some(job) = self.job.lock().await.take() {
            job.abort();
            let _ = job.await;
        }
    }
    fn log(&self, message: &str) {
        if let Some(parent) = self.paths.log.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.paths.log)
        {
            let _ = writeln!(file, "{} {}", Local::now().format("%Y-%m-%d %H:%M:%S"), message);
        }
    }
    async fn tick(self: &Arc<Self>) -> Result<(), String> {
        let _change = self.changes.lock().await;
        if self.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let enabled = read_enabled(&self.paths.app)?;
        if !enabled {
            self.cancel().await;
        }
        {
            let mut job = self.job.lock().await;
            if job.as_ref().is_some_and(|j| !j.is_finished()) {
                return Ok(());
            }
            if let Some(done) = job.take() {
                let _ = done.await;
            }
        }
        let lock = match ActionLock::acquire(&self.paths.lock) {
            Ok(l) => l,
            Err(e) if e == "busy" => return Ok(()),
            Err(e) => return Err(e),
        };
        prepare(&self.paths, &self.cron_restart_pending).await?;
        // Migrate legacy jobs even when the watchdog config was removed.
        if !self.paths.config.exists() {
            return if enabled {
                Err("configuration_missing".into())
            } else {
                Ok(())
            };
        }
        if !enabled {
            return Ok(());
        }
        let cfg = config::load(&self.paths.config)?;
        let now = Local::now();
        let slot = now.timestamp().div_euclid(60);
        if now.hour() == 6 && now.minute() == 59 {
            cleanup_log(&self.paths.log, Some(cfg.log_cleanup_days), now.timestamp() as f64)?;
        }
        if !due(&cfg.schedule, (now.hour() * 60 + now.minute()) as u64) {
            return Ok(());
        }
        let slot_path = self.paths.log.with_file_name(format!(
            "{}.last-slot",
            self.paths.log.file_name().unwrap_or_default().to_string_lossy()
        ));
        let previous = read_json(&slot_path)?.get("last_minute").and_then(Value::as_i64);
        if !new_slot(previous, slot) {
            return Ok(());
        }
        write_private(&slot_path, &json!({"last_minute":slot}))?;
        let runtime = self.clone();
        let handle = tokio::spawn(async move {
            let _lock = lock;
            if let Err(code) = watchdog_engine::automatic(&cfg, &runtime.paths.mihomo, &runtime.paths.log).await {
                runtime.log(&format!("Automatic operation failed: {code}"));
            }
        });
        *self.job.lock().await = Some(handle);
        Ok(())
    }
}

fn new_slot(previous: Option<i64>, slot: i64) -> bool {
    previous.is_none_or(|old| slot > old)
}
fn due(schedule: &Value, minute: u64) -> bool {
    schedule
        .get("windows")
        .and_then(Value::as_array)
        .is_some_and(|windows| {
            windows.iter().any(|window| {
                let Some(start) = config::minute(&window["start"]) else {
                    return false;
                };
                let Some(end) = config::minute(&window["end"]) else {
                    return false;
                };
                let Some(interval) = window["every_minutes"].as_u64().filter(|i| *i > 0) else {
                    return false;
                };
                let duration = match (end + 1440 - start) % 1440 {
                    0 => 1440,
                    d => d,
                };
                let elapsed = (minute + 1440 - start) % 1440;
                elapsed < duration && elapsed % interval == 0
            })
        })
}
fn strip_jobs(text: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut managed = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed == "# BEGIN router-watchdog managed entries" {
            if managed {
                return Err("invalid_crontab_markers".into());
            }
            managed = true;
            continue;
        }
        if trimmed == "# END router-watchdog managed entries" {
            if !managed {
                return Err("invalid_crontab_markers".into());
            }
            managed = false;
            continue;
        }
        if managed {
            continue;
        }
        let tokens: Vec<_> = trimmed.split_whitespace().collect();
        let command_tokens = if !trimmed.starts_with('#') {
            let mut start = 5;
            if tokens
                .get(start)
                .is_some_and(|t| !t.contains('/') && !t.starts_with("python") && *t != "echo" && *t != "flock")
            {
                start += 1;
            }
            tokens.get(start..).unwrap_or(&[])
        } else {
            &[]
        };
        let owned = command_tokens.first().is_some_and(|command| {
            *command == "/opt/bin/router-watchdog"
                || *command == "/opt/libexec/router-watchdog/router_watchdog.py"
                || command.ends_with("/python3")
                    && command_tokens
                        .get(1)
                        .is_some_and(|script| *script == "/opt/libexec/router-watchdog/router_watchdog.py")
                || *command == "python3"
                    && command_tokens
                        .get(1)
                        .is_some_and(|script| *script == "/opt/libexec/router-watchdog/router_watchdog.py")
                || *command == "flock"
                    && command_tokens.iter().enumerate().any(|(index, token)| {
                        (*token == "/opt/bin/router-watchdog"
                            || *token == "/opt/libexec/router-watchdog/router_watchdog.py")
                            && index > 0
                    })
        });
        if !owned {
            output.push_str(line);
        }
    }
    if managed {
        return Err("invalid_crontab_markers".into());
    }
    Ok(output)
}
fn no_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err("symlink_refused".into()),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("file_unavailable".into()),
    }
}
fn write_bytes(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    no_symlink(path)?;
    let parent = path.parent().ok_or("invalid_path")?;
    fs::create_dir_all(parent).map_err(|_| "write_failed")?;
    let temp = parent.join(format!(".watchdog-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(mode)
            .open(&temp)
            .map_err(|_| "write_failed")?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "write_failed")?;
        fs::rename(&temp, path).map_err(|_| "write_failed")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
fn write_private(path: &Path, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|_| "invalid_json")?;
    bytes.push(b'\n');
    write_bytes(path, &bytes, 0o600)
}
fn read_json(path: &Path) -> Result<Value, String> {
    no_symlink(path)?;
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| "invalid_json".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(_) => Err("read_failed".into()),
    }
}
fn read_enabled(path: &Path) -> Result<bool, String> {
    let value = read_json(path)?;
    match value.get("watchdog").and_then(|v| v.get("enabled")) {
        None => Ok(true),
        Some(Value::Bool(enabled)) => Ok(*enabled),
        _ => Err("invalid_enabled".into()),
    }
}
fn cleanup_log(path: &Path, days: Option<u64>, now: f64) -> Result<(), String> {
    no_symlink(path)?;
    let state = path.with_file_name(format!(
        "{}.cleanup",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    no_symlink(&state)?;
    if let Some(days) = days {
        if !(1..=3650).contains(&days) {
            return Err("invalid_cleanup_days".into());
        }
        if !state.exists() {
            return write_private(&state, &json!(now));
        }
        let last = fs::read_to_string(&state)
            .map_err(|_| "cleanup_state_invalid")?
            .trim()
            .parse::<f64>()
            .map_err(|_| "cleanup_state_invalid")?;
        if !last.is_finite() || last < 0.0 {
            return Err("cleanup_state_invalid".into());
        }
        if now - last < days as f64 * 86400.0 {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| "cleanup_failed")?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "cleanup_failed")?;
    write_private(&state, &json!(now))
}
struct ActionLock {
    file: fs::File,
}
impl ActionLock {
    fn acquire(path: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)
            .map_err(|_| "lock_unavailable")?;
        use std::os::fd::AsRawFd;
        if unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) } != 0 {
            return Err("busy".into());
        }
        Ok(Self { file })
    }
}
impl Drop for ActionLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            nix::libc::flock(self.file.as_raw_fd(), nix::libc::LOCK_UN);
        }
    }
}
async fn prepare(paths: &Paths, restart_pending: &AtomicBool) -> Result<(), String> {
    prepare_with_restart(paths, restart_pending, || async {
        let status = tokio::process::Command::new(&paths.cron_init)
            .arg("restart")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|_| "cron_restart_failed")?;
        if status.success() {
            Ok(())
        } else {
            Err("cron_restart_failed".into())
        }
    })
    .await
}
async fn prepare_with_restart<F, Fut>(paths: &Paths, restart_pending: &AtomicBool, mut restart: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    // Validate the full set before mutating any cron file.
    let mut changed = Vec::new();
    for path in &paths.crontabs {
        no_symlink(path)?;
        if !path.exists() {
            continue;
        }
        let text = fs::read_to_string(path).map_err(|_| "crontab_unavailable")?;
        let updated = strip_jobs(&text)?;
        if updated != text {
            changed.push((
                path,
                updated,
                fs::metadata(path)
                    .map_err(|_| "crontab_unavailable")?
                    .permissions()
                    .mode()
                    & 0o777,
            ));
        }
    }
    no_symlink(&paths.hook)?;
    no_symlink(&paths.cli)?;
    let command = format!(
        "#!/bin/sh\nexec '{}' watchdog \"$@\"\n",
        paths.binary.to_string_lossy().replace('\'', "'\\''")
    );
    if fs::read_to_string(&paths.cli).ok().as_deref() != Some(&command) {
        write_bytes(&paths.cli, command.as_bytes(), 0o755)?;
    }
    if paths.hook.exists() {
        fs::remove_file(&paths.hook).map_err(|_| "cron_hook_failed")?;
    }
    let pending = paths.cli.with_extension("cron-restart-pending");
    no_symlink(&pending)?;
    if !changed.is_empty() || restart_pending.load(Ordering::Acquire) {
        write_bytes(&pending, b"pending\n", 0o600)?;
    }
    // Persist intent before editing cron, so an interrupted migration retries restart.
    for (path, text, mode) in &changed {
        write_bytes(path, text.as_bytes(), *mode)?;
    }
    if pending.exists() {
        restart_pending.store(true, Ordering::Release);
        restart().await?;
        fs::remove_file(&pending).map_err(|_| "cron_restart_failed")?;
        restart_pending.store(false, Ordering::Release);
    }
    Ok(())
}
fn user_error(code: &str) -> String {
    match code {
        "busy" => "Watchdog занят другой операцией. Повторите после её завершения.",
        "configuration_missing" => "Сначала настройте подключение и группы watchdog.",
        "symlink_refused" => "Нельзя изменять символьную ссылку вместо файла настроек.",
        "invalid_enabled" => "Состояние watchdog должно быть логическим значением.",
        "write_failed" => "Не удалось сохранить настройки watchdog.",
        _ => "Не удалось применить настройки watchdog. Проверьте значения полей и доступность файлов.",
    }
    .to_owned()
}
pub async fn get_config(State(_state): State<AppState>) -> Json<Value> {
    match config_view(&RUNTIME.paths) {
        Ok(view) => Json(view),
        Err(code) => Json(json!({"success":false,"error":user_error(&code)})),
    }
}
fn config_view(paths: &Paths) -> Result<Value, String> {
    let raw = read_json(&paths.config)?;
    let cfg: config::Config = serde_json::from_value(raw).map_err(|_| "invalid_config")?;
    let mut view = cfg.public_view();
    view["success"] = true.into();
    view["enabled"] = read_enabled(&paths.app)?.into();
    view["needs_configuration"] = cfg.validate().is_err().into();
    Ok(view)
}
pub async fn patch_config(State(state): State<AppState>, Json(patch): Json<Value>) -> Json<Value> {
    let _app_lock = state.app_config_lock.lock().await;
    match apply_settings(&RUNTIME, &patch).await {
        Ok(()) => Json(json!({"success":true})),
        Err(code) => Json(json!({"success":false,"error":user_error(&code)})),
    }
}
async fn apply_settings(runtime: &Runtime, patch: &Value) -> Result<(), String> {
    let map = patch.as_object().filter(|m| !m.is_empty()).ok_or("invalid_patch")?;
    if map.keys().any(|k| k != "enabled" && k != "config") {
        return Err("invalid_patch".into());
    }
    if map.len() > 1 {
        return Err("invalid_patch".into());
    }
    let enabled = map
        .get("enabled")
        .map(|v| v.as_bool().ok_or("invalid_enabled"))
        .transpose()?;
    let _changes = runtime.changes.lock().await;
    let current = if map.contains_key("config") || enabled == Some(true) {
        read_json(&runtime.paths.config)?
    } else {
        json!({})
    };
    let next = map
        .get("config")
        .map(|p| config::validate_patch(&current, p))
        .transpose()?;
    if enabled == Some(true) {
        let cfg: config::Config =
            serde_json::from_value(next.clone().unwrap_or_else(|| current.clone())).map_err(|_| "invalid_config")?;
        cfg.validate()?;
    }
    // Cancellation releases the in-process action lock before configuration is written.
    if enabled == Some(false) {
        runtime.cancel().await;
    }
    let _lock = ActionLock::acquire(&runtime.paths.lock)?;
    if let Some(enabled) = enabled {
        let mut app = read_json(&runtime.paths.app)?;
        if !app.is_object() {
            return Err("invalid_app_config".into());
        }
        if app.get("watchdog").is_some_and(|v| !v.is_object()) {
            return Err("invalid_app_config".into());
        }
        app["watchdog"]["enabled"] = enabled.into();
        write_private(&runtime.paths.app, &app)?;
    }
    if let Some(next) = next {
        write_private(&runtime.paths.config, &next)?;
    }
    runtime.wake.notify_one();
    Ok(())
}

#[derive(Parser)]
#[command(name="xkeen-ui watchdog", about="Native Mihomo watchdog", group(ArgGroup::new("action").multiple(false).args(["get_config","update_config","measure_selected","find_faster","select_node","clear_log","sync_cron","remove_cron","check_update","install_update"])))]
struct WatchdogCli {
    #[arg(long)]
    get_config: bool,
    #[arg(long)]
    update_config: bool,
    #[arg(long)]
    measure_selected: bool,
    #[arg(long)]
    find_faster: bool,
    #[arg(long)]
    select_node: bool,
    #[arg(long)]
    clear_log: bool,
    #[arg(long)]
    sync_cron: bool,
    #[arg(long)]
    remove_cron: bool,
    #[arg(long)]
    if_due: bool,
    #[arg(long)]
    mcp_speed_profile: bool,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    check_update: bool,
    #[arg(long)]
    install_update: bool,
    #[arg(long)]
    expected_version: Option<String>,
}
fn stdin_json() -> Result<Value, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(65537)
        .read_to_end(&mut bytes)
        .map_err(|_| "invalid_input")?;
    if bytes.len() > 65536 {
        return Err("invalid_input".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid_input".into())
}
pub async fn cli(args: Vec<String>) -> i32 {
    let parsed = WatchdogCli::try_parse_from(std::iter::once("xkeen-ui watchdog".to_owned()).chain(args));
    let args = match parsed {
        Ok(a) => a,
        Err(e) => {
            let code = e.exit_code();
            let _ = e.print();
            return code;
        }
    };
    if (args.apply && !args.find_faster)
        || (args.mcp_speed_profile && (!(args.find_faster || args.measure_selected) || args.apply))
        || (args.if_due && !args.clear_log)
        || (args.expected_version.is_some() != args.install_update)
    {
        eprintln!("Invalid watchdog action combination");
        return 2;
    }
    let paths = Paths::default();
    let result: Result<Value, String> = async {
        let _lock = ActionLock::acquire(&paths.lock)?;
        if args.check_update || args.install_update {
            return Err("integrated_component".into());
        }
        if args.sync_cron || args.remove_cron {
            prepare(&paths, &AtomicBool::new(false)).await?;
            return Ok(json!({"changed":true,"native":true}));
        }
        if args.get_config {
            if !paths.config.exists() { return Err("configuration_missing".into()); }
            let raw = read_json(&paths.config)?;
            let object = raw.as_object().ok_or("invalid_config")?;
            let public: serde_json::Map<String,Value> = object.iter()
                .filter(|(key,_)| key.as_str() != "secret" && config::EDITABLE_FIELDS.contains(&key.as_str()))
                .map(|(key,value)| (key.clone(),value.clone())).collect();
            let mut fields = config::EDITABLE_FIELDS.to_vec(); fields.sort_unstable();
            return Ok(json!({"config":public,"editable_fields":fields,"secret_configured":object.get("secret").and_then(Value::as_str).is_some_and(|s| !s.is_empty())}));
        }
        if args.update_config {
            let patch = stdin_json()?;
            let current = read_json(&paths.config)?;
            let updated = config::validate_patch(&current, &patch)?;
            let fields: Vec<_> = patch
                .as_object()
                .ok_or("invalid_patch")?
                .keys()
                .filter(|k| current.get(*k) != updated.get(*k))
                .cloned()
                .collect();
            write_private(&paths.config, &updated)?;
            return Ok(json!({"updated_fields":fields,"cron_sync_required":false}));
        }
        if args.clear_log {
            let days = if args.if_due {
                Some(
                    read_json(&paths.config)?
                        .get("log_cleanup_days")
                        .and_then(Value::as_u64)
                        .unwrap_or(3),
                )
            } else {
                None
            };
            cleanup_log(&paths.log, days, Local::now().timestamp() as f64)?;
            return Ok(json!({"cleared":true}));
        }
        let cfg = config::load(&paths.config)?;
        if args.select_node {
            let input = stdin_json()?;
            let obj = input.as_object().ok_or("invalid_input")?;
            if obj.len() != 2 {
                return Err("invalid_input".into());
            }
            return watchdog_engine::select(
                &cfg,
                &paths.mihomo,
                &paths.log,
                obj.get("node").and_then(Value::as_str).ok_or("invalid_input")?,
                obj.get("expected_current")
                    .and_then(Value::as_str)
                    .ok_or("invalid_input")?,
            )
            .await;
        }
        if args.measure_selected || args.find_faster {
            return watchdog_engine::measure(
                &cfg,
                &paths.mihomo,
                &paths.log,
                args.find_faster,
                args.mcp_speed_profile,
                args.apply,
            )
            .await;
        }
        if !read_enabled(&paths.app)? {
            return Ok(json!({"skipped":true,"enabled":false}));
        }
        watchdog_engine::automatic(&cfg, &paths.mihomo, &paths.log).await?;
        Ok(json!({"success":true}))
    }
    .await;
    match result {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(code) => {
            println!(
                "{}",
                json!({"error":if code=="busy" { "busy" } else if code=="configuration_missing" { "configuration_missing" } else { "operation_failed" },"message":if code=="integrated_component" { "Watchdog updates with the panel" } else { "Operation failed; check configuration and router state locally" }})
            );
            if code == "configuration_missing" { 2 } else { 1 }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "xkeen-native-watchdog-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn schedule_handles_midnight_boundaries_and_full_day() {
        let schedule = json!({"windows":[{"start":"23:00","end":"01:00","every_minutes":30}]});
        assert!(due(&schedule, 23 * 60));
        assert!(due(&schedule, 0));
        assert!(due(&schedule, 30));
        assert!(!due(&schedule, 60));
        assert!(!due(&schedule, 1));
        assert!(due(
            &json!({"windows":[{"start":"00:00","end":"00:00","every_minutes":1}]}),
            1439
        ));
    }
    #[test]
    fn migration_preserves_unrelated_jobs_and_rejects_broken_markers() {
        let text = "# user comment\n* * * * * root /opt/bin/other\n# BEGIN router-watchdog managed entries\n* * * * * root /opt/bin/router-watchdog\n# END router-watchdog managed entries\n0 * * * * /opt/bin/router-watchdog --clear-log\n0 0 * * * root echo /opt/bin/router-watchdog\n";
        let clean = strip_jobs(text).unwrap();
        assert_eq!(
            clean,
            "# user comment\n* * * * * root /opt/bin/other\n0 0 * * * root echo /opt/bin/router-watchdog\n"
        );
        assert_eq!(strip_jobs(&clean).unwrap(), clean);
        assert!(strip_jobs("# BEGIN router-watchdog managed entries\n* * * * * root other\n").is_err());
        let wrapped = "* * * * * root flock -n /tmp/router_watchdog.lock /usr/bin/python3 /opt/libexec/router-watchdog/router_watchdog.py --find-faster\n* * * * * root echo /opt/bin/router-watchdog\n";
        let wrapped_clean = strip_jobs(wrapped).unwrap();
        assert!(!wrapped_clean.contains("flock"));
        assert!(wrapped_clean.contains("echo /opt/bin/router-watchdog"));
        assert!(
            strip_jobs("* * * * * root echo /opt/libexec/router-watchdog/router_watchdog.py\n")
                .unwrap()
                .contains("echo")
        );
    }
    #[test]
    fn enabled_defaults_true_and_persisted_false_survives() {
        let d = Temp::new();
        let p = d.0.join("app.json");
        assert!(read_enabled(&p).unwrap());
        write_private(&p, &json!({"auth":{"enabled":true},"watchdog":{"enabled":false}})).unwrap();
        assert!(!read_enabled(&p).unwrap());
        assert_eq!(fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o600);
    }
    #[test]
    fn lock_is_nonblocking_shared_and_released_on_drop() {
        let d = Temp::new();
        let p = d.0.join("lock");
        let first = ActionLock::acquire(&p).unwrap();
        assert!(ActionLock::acquire(&p).is_err());
        drop(first);
        assert!(ActionLock::acquire(&p).is_ok());
        assert!(p.exists());
    }
    #[test]
    fn cleanup_initializes_without_truncating_then_clears_when_due() {
        let d = Temp::new();
        let log = d.0.join("watchdog.log");
        fs::write(&log, "keep").unwrap();
        cleanup_log(&log, Some(3), 1000.0).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "keep");
        cleanup_log(&log, Some(3), 1000.0 + 3.0 * 86400.0 - 1.0).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "keep");
        cleanup_log(&log, Some(3), 1000.0 + 3.0 * 86400.0).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "");
    }
    #[test]
    fn invalid_cleanup_state_keeps_log_and_symlinks_are_rejected() {
        let d = Temp::new();
        let log = d.0.join("watchdog.log");
        fs::write(&log, "keep").unwrap();
        fs::write(d.0.join("watchdog.log.cleanup"), "NaN").unwrap();
        assert!(cleanup_log(&log, Some(3), 1000.0).is_err());
        assert_eq!(fs::read_to_string(&log).unwrap(), "keep");
        let link = d.0.join("link");
        std::os::unix::fs::symlink(&log, &link).unwrap();
        assert!(write_private(&link, &json!({})).is_err());
        assert_eq!(fs::read_to_string(&log).unwrap(), "keep");
    }
    fn paths(root: &Path) -> Paths {
        Paths {
            config: root.join("watchdog.json"),
            log: root.join("watchdog.log"),
            lock: root.join("lock"),
            mihomo: root.join("mihomo.yaml"),
            app: root.join("app.json"),
            crontabs: vec![root.join("crontab")],
            hook: root.join("cron.1min/watchdog"),
            cli: root.join("bin/router-watchdog"),
            binary: root.join("sbin/xkeen-ui"),
            cron_init: root.join("cron-init"),
        }
    }
    #[tokio::test]
    async fn migration_removes_only_owned_cron_and_retries_failed_restart() {
        let d = Temp::new();
        let p = paths(&d.0);
        fs::create_dir_all(p.cli.parent().unwrap()).unwrap();
        fs::write(&p.crontabs[0],"* * * * * root /opt/bin/other\n* * * * * root /opt/bin/router-watchdog\n0 0 * * * root python3 /opt/libexec/router-watchdog/router_watchdog.py\n").unwrap();
        fs::create_dir_all(p.hook.parent().unwrap()).unwrap();
        fs::write(&p.hook, "legacy").unwrap();
        assert_eq!(
            prepare_with_restart(&p, &AtomicBool::new(false), || async {
                Err("cron_restart_failed".into())
            })
            .await
            .unwrap_err(),
            "cron_restart_failed"
        );
        let cron = fs::read_to_string(&p.crontabs[0]).unwrap();
        assert!(cron.contains("/opt/bin/other"));
        assert!(!cron.contains("router-watchdog"));
        assert!(!p.hook.exists());
        let adapter = fs::read_to_string(&p.cli).unwrap();
        assert!(adapter.contains("xkeen-ui' watchdog"));
        assert!(!adapter.contains("python"));
        let mut restarts = 0;
        prepare_with_restart(&p, &AtomicBool::new(false), || {
            restarts += 1;
            async { Ok(()) }
        })
        .await
        .unwrap();
        assert_eq!(
            restarts, 1,
            "failed restart must be retried even after cron was rewritten"
        );
    }
    #[tokio::test]
    async fn settings_api_model_keeps_secret_redacted_and_can_disable_invalid_config() {
        let d = Temp::new();
        let p = paths(&d.0);
        fs::create_dir_all(p.config.parent().unwrap()).unwrap();
        fs::write(&p.config, r#"{"secret":"private","controller":"not a url"}"#).unwrap();
        let view = config_view(&p).unwrap();
        assert_eq!(view["secret_configured"], true);
        assert!(view["config"].get("secret").is_none());
        let runtime = Runtime::new(p.clone());
        apply_settings(&runtime, &json!({"enabled":false})).await.unwrap();
        assert_eq!(read_enabled(&p.app).unwrap(), false);
        assert_eq!(read_json(&p.config).unwrap()["secret"], "private");
        assert!(apply_settings(&runtime, &json!({"enabled":true})).await.is_err());
        assert_eq!(read_enabled(&p.app).unwrap(), false);
    }
    #[tokio::test]
    async fn disable_cancels_job_and_releases_lock_before_persisting() {
        let d = Temp::new();
        let p = paths(&d.0);
        let runtime = Runtime::new(p.clone());
        let lock = ActionLock::acquire(&p.lock).unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        *runtime.job.lock().await = Some(tokio::spawn(async move {
            let _lock = lock;
            let _ = started.send(());
            std::future::pending::<()>().await;
        }));
        ready.await.unwrap();
        apply_settings(&runtime, &json!({"enabled":false})).await.unwrap();
        assert!(!read_enabled(&p.app).unwrap());
        assert!(ActionLock::acquire(&p.lock).is_ok());
        assert!(runtime.job.lock().await.is_none());
        let runtime = Arc::new(runtime);
        runtime.shutdown().await;
        runtime.tick().await.unwrap();
        assert!(!p.cli.exists(), "shutdown must prevent further migration or scheduling");
    }
    #[test]
    fn minute_slots_never_repeat_or_replay_after_clock_moves_back() {
        assert!(new_slot(None, 10));
        assert!(!new_slot(Some(10), 10));
        assert!(!new_slot(Some(10), 9));
        assert!(new_slot(Some(10), 11));
    }
}
