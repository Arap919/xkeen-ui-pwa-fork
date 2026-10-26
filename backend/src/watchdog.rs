use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::LazyLock, time::Duration};
use tokio::{process::Command, sync::Mutex};

const CLI: &str = "/opt/bin/router-watchdog";
static INSTALL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Deserialize, Serialize)]
struct UpdateStatus {
    installed_version: Option<String>,
    latest_version: String,
    update_available: bool,
    release_url: String,
    checked_at: String,
    #[serde(default)]
    release_notes: Option<String>,
}

fn valid_version(version: &str) -> bool {
    let Some((base, revision)) = version.rsplit_once('-') else {
        return false;
    };
    !revision.is_empty()
        && revision.bytes().all(|c| c.is_ascii_digit())
        && semver::Version::parse(base).is_ok_and(|v| v.pre.is_empty() && v.build.is_empty())
}

fn parse_check(bytes: &[u8]) -> Result<UpdateStatus, String> {
    let status: UpdateStatus = serde_json::from_slice(bytes)
        .map_err(|_| "router-watchdog не поддерживает проверку обновлений. Обновите пакет вручную.".to_string())?;
    let url = reqwest::Url::parse(&status.release_url).map_err(|_| "Неверная ссылка релиза".to_string())?;
    if !valid_version(&status.latest_version)
        || url.scheme() != "https"
        || url.host_str() != Some("github.com")
        || chrono::DateTime::parse_from_rfc3339(&status.checked_at).is_err()
        || status.installed_version.as_ref().is_some_and(|v| !valid_version(v))
    {
        return Err("Некорректный ответ router-watchdog".into());
    }
    Ok(status)
}

async fn check() -> Result<UpdateStatus, String> {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(CLI).arg("--check-update").kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| "Проверка обновлений превысила время ожидания".to_string())?
    .map_err(|_| "router-watchdog не установлен или недоступен".to_string())?;
    if !output.status.success() {
        return Err(
            "Проверка обновлений недоступна. Проверьте версию router-watchdog и доступ к серверу релизов.".into(),
        );
    }
    parse_check(&output.stdout)
}

fn failure(error: String) -> Json<Value> {
    Json(json!({"success": false, "error": error}))
}

pub async fn get_status() -> Json<Value> {
    match check().await {
        Ok(status) => Json(
            json!({"success": true, "data": status, "installing": INSTALL.try_lock().is_err(), "push_available": true}),
        ),
        Err(error) => failure(error),
    }
}

#[derive(Deserialize)]
pub struct InstallRequest {
    expected_version: String,
}

pub async fn install(Json(request): Json<InstallRequest>) -> Json<Value> {
    if !valid_version(&request.expected_version) {
        return failure("Некорректная версия".into());
    }
    // The task owns the lock and process even if the HTTP request is cancelled.
    let task = tokio::spawn(async move {
        let _guard = INSTALL
            .try_lock()
            .map_err(|_| "Обновление уже выполняется".to_string())?;
        let status = check().await?;
        if status.installed_version.is_none()
            || !status.update_available
            || status.latest_version != request.expected_version
        {
            return Err("Версия изменилась или обновление недоступно. Повторите проверку.".into());
        }
        let output = Command::new(CLI)
            .args(["--install-update", "--expected-version", &request.expected_version])
            .output()
            .await
            .map_err(|_| "Не удалось запустить обновление".to_string())?;
        if !output.status.success() {
            return Err("Установка не завершена. Проверьте состояние пакета перед повторной попыткой.".into());
        }
        let result: Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| "Неизвестный результат установки. Проверьте установленную версию.".to_string())?;
        if result.get("installed_version").and_then(Value::as_str) != Some(request.expected_version.as_str())
            || !result.get("updated").is_some_and(Value::is_boolean)
        {
            return Err("Неизвестный результат установки. Проверьте установленную версию.".into());
        }
        Ok(result)
    });
    match task.await {
        Ok(Ok(result)) => Json(json!({"success": true, "data": result})),
        Ok(Err(error)) => failure(error),
        Err(_) => failure("Неизвестный результат установки. Повторите проверку версии.".into()),
    }
}

fn valid_endpoint(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.fragment().is_none()
        && (host == "fcm.googleapis.com" || host == "updates.push.services.mozilla.com" || host == "web.push.apple.com")
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SubscriptionInfo {
    endpoint: String,
    keys: SubscriptionKeys,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
struct SubscriptionKeys {
    auth: String,
    p256dh: String,
}
#[cfg(test)]
impl SubscriptionInfo {
    fn new(endpoint: &str, p256dh: &str, auth: &str) -> Self {
        Self {
            endpoint: endpoint.into(),
            keys: SubscriptionKeys {
                auth: auth.into(),
                p256dh: p256dh.into(),
            },
        }
    }
}
#[derive(Clone, Deserialize, Serialize)]
struct SubscriptionRecord {
    subscription: SubscriptionInfo,
    last_notified: Option<String>,
}
impl SubscriptionRecord {
    fn needs_version(&self, version: &str) -> bool {
        self.last_notified.as_deref() != Some(version)
    }
}

mod push {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use p256::{
        SecretKey,
        elliptic_curve::{rand_core::OsRng, sec1::ToEncodedPoint},
    };
    use std::{
        fs::OpenOptions,
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    use web_push_native::{Auth, WebPushBuilder, jwt_simple::algorithms::ES256KeyPair};
    const PRIVATE_STATE: &str = "/opt/etc/xkeen/watchdog-push.json";
    static STORE: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    #[derive(Default, Serialize, Deserialize)]
    struct PushState {
        private_key: String,
        subscriptions: Vec<SubscriptionRecord>,
    }
    fn load_from(path: &str) -> Result<PushState, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| "Не удалось прочитать настройки Push".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PushState::default()),
            Err(_) => Err("Не удалось прочитать настройки Push".into()),
        }
    }
    fn save_to(path: &str, state: &PushState) -> Result<(), String> {
        let temp = format!("{path}.tmp");
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temp)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(&serde_json::to_vec(state)?)?;
            file.sync_all()?;
            std::fs::rename(temp, path)
        })();
        result.map_err(|_| "Не удалось сохранить настройки Push".into())
    }
    pub async fn key() -> Json<Value> {
        let _guard = STORE.lock().await;
        let result = (|| {
            let mut state = load_from(PRIVATE_STATE)?;
            if state.private_key.is_empty() {
                state.private_key = URL_SAFE_NO_PAD.encode(SecretKey::random(&mut OsRng).to_bytes());
                save_to(PRIVATE_STATE, &state)?;
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(&state.private_key)
                .map_err(|_| "Ошибка ключа Push".to_string())?;
            let key = SecretKey::from_slice(&bytes).map_err(|_| "Ошибка ключа Push".to_string())?;
            Ok::<_, String>(URL_SAFE_NO_PAD.encode(key.public_key().to_encoded_point(false).as_bytes()))
        })();
        match result {
            Ok(key) => Json(json!({"success": true, "public_key": key})),
            Err(error) => failure(error),
        }
    }
    pub async fn subscribe(Json(subscription): Json<SubscriptionInfo>) -> Json<Value> {
        if !valid_endpoint(&subscription.endpoint)
            || URL_SAFE_NO_PAD.decode(&subscription.keys.auth).is_err()
            || URL_SAFE_NO_PAD
                .decode(&subscription.keys.p256dh)
                .ok()
                .and_then(|bytes| p256::PublicKey::from_sec1_bytes(&bytes).ok())
                .is_none()
            || URL_SAFE_NO_PAD
                .decode(&subscription.keys.auth)
                .map_or(true, |bytes| bytes.len() != 16)
        {
            return failure("Некорректная или неподдерживаемая подписка Push".into());
        }
        let _guard = STORE.lock().await;
        let result = (|| {
            let mut state = load_from(PRIVATE_STATE)?;
            if state.private_key.is_empty() {
                return Err("Сначала получите ключ Push".into());
            }
            if !state
                .subscriptions
                .iter()
                .any(|record| record.subscription.endpoint == subscription.endpoint)
            {
                if state.subscriptions.len() >= 32 {
                    return Err("Достигнут лимит подписок Push".into());
                }
                state.subscriptions.push(SubscriptionRecord {
                    subscription,
                    last_notified: None,
                });
            }
            save_to(PRIVATE_STATE, &state)
        })();
        match result {
            Ok(()) => Json(json!({"success": true})),
            Err(error) => failure(error),
        }
    }
    #[derive(Deserialize)]
    pub struct EndpointRequest {
        endpoint: String,
    }
    pub async fn unsubscribe(Json(request): Json<EndpointRequest>) -> Json<Value> {
        let _guard = STORE.lock().await;
        let result = (|| {
            let mut state = load_from(PRIVATE_STATE)?;
            state
                .subscriptions
                .retain(|record| record.subscription.endpoint != request.endpoint);
            save_to(PRIVATE_STATE, &state)
        })();
        match result {
            Ok(()) => Json(json!({"success": true})),
            Err(error) => failure(error),
        }
    }
    pub(super) fn build_request(
        subscription: &SubscriptionInfo, private_key: &[u8], payload: &[u8],
    ) -> Result<http::Request<Vec<u8>>, String> {
        let key = ES256KeyPair::from_bytes(private_key).map_err(|_| "Invalid push key".to_string())?;
        let public = URL_SAFE_NO_PAD
            .decode(&subscription.keys.p256dh)
            .ok()
            .and_then(|bytes| p256::PublicKey::from_sec1_bytes(&bytes).ok())
            .ok_or("Invalid subscription key")?;
        let auth = URL_SAFE_NO_PAD
            .decode(&subscription.keys.auth)
            .map_err(|_| "Invalid subscription auth")?;
        if auth.len() != 16 || !valid_endpoint(&subscription.endpoint) {
            return Err("Invalid subscription".into());
        }
        let endpoint = subscription.endpoint.parse().map_err(|_| "Invalid endpoint")?;
        WebPushBuilder::new(endpoint, public, Auth::clone_from_slice(&auth))
            .with_valid_duration(Duration::from_secs(86400))
            .with_vapid(&key, "https://github.com/Arap919/xkeen-ui-pwa-fork")
            .build(payload)
            .map_err(|_| "Unable to encrypt push".into())
    }
    async fn notify_with_sender<F, Fut>(path: &str, status: &UpdateStatus, mut send: F) -> Result<(), String>
    where
        F: FnMut(http::Request<Vec<u8>>) -> Fut,
        Fut: std::future::Future<Output = Result<u16, String>>,
    {
        if !status.update_available {
            return Ok(());
        }
        let snapshot = {
            let _guard = STORE.lock().await;
            load_from(path)?
        };
        if snapshot.subscriptions.is_empty() {
            return Ok(());
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(&snapshot.private_key)
            .map_err(|_| "Invalid push key".to_string())?;
        let payload = json!({"title": "Обновление router-watchdog", "body": format!("Доступна версия {}. Откройте панель для установки.", status.latest_version)}).to_string();
        for record in snapshot.subscriptions {
            if !record.needs_version(&status.latest_version) {
                continue;
            }
            let Ok(request) = build_request(&record.subscription, &bytes, payload.as_bytes()) else {
                continue;
            };
            // Never hold subscription storage across encryption or network awaits.
            let Ok(code) = send(request).await else { continue };
            if !(200..300).contains(&code) && !matches!(code, 404 | 410) {
                continue;
            }
            let _guard = STORE.lock().await;
            let mut current = load_from(path)?;
            if current.private_key != snapshot.private_key {
                continue;
            }
            let Some(index) = current.subscriptions.iter().position(|entry| {
                entry.subscription == record.subscription && entry.last_notified == record.last_notified
            }) else {
                continue;
            };
            if (200..300).contains(&code) {
                current.subscriptions[index].last_notified = Some(status.latest_version.clone());
            } else {
                current.subscriptions.remove(index);
            }
            // Merge only into a still-current subscription, never restore removed records.
            save_to(path, &current)?;
        }
        Ok(())
    }
    pub async fn notify(client: &reqwest::Client, status: &UpdateStatus) -> Result<(), String> {
        notify_with_sender(PRIVATE_STATE, status, |request| async move {
            let mut outgoing = client.post(request.uri().to_string());
            for (name, value) in request.headers() {
                outgoing = outgoing.header(name.as_str(), value.as_bytes());
            }
            outgoing
                .body(request.into_body())
                .send()
                .await
                .map(|response| response.status().as_u16())
                .map_err(|_| "Push service unavailable".into())
        })
        .await
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[tokio::test]
        async fn slow_push_does_not_block_unsubscribe_or_restore_removed_subscription() {
            let path = std::env::temp_dir().join(format!("xkeen-push-{}.json", uuid::Uuid::new_v4()));
            let path = path.to_str().unwrap();
            let browser = SecretKey::random(&mut OsRng);
            let state = PushState {
                private_key: URL_SAFE_NO_PAD.encode(SecretKey::random(&mut OsRng).to_bytes()),
                subscriptions: vec![SubscriptionRecord {
                    subscription: SubscriptionInfo::new(
                        "https://fcm.googleapis.com/fcm/send/test",
                        &URL_SAFE_NO_PAD.encode(browser.public_key().to_encoded_point(false).as_bytes()),
                        &URL_SAFE_NO_PAD.encode([7u8; 16]),
                    ),
                    last_notified: None,
                }],
            };
            save_to(path, &state).unwrap();
            let status = UpdateStatus {
                installed_version: Some("1.0.10-1".into()),
                latest_version: "1.0.11-1".into(),
                update_available: true,
                release_url: "https://github.com/o/r/releases/tag/v1.0.11".into(),
                checked_at: "2026-10-27T00:00:00Z".into(),
                release_notes: None,
            };
            let started = tokio::sync::Notify::new();
            let release = tokio::sync::Notify::new();
            let notify = notify_with_sender(path, &status, |_| async {
                started.notify_one();
                release.notified().await;
                Ok(201)
            });
            let unsubscribe = async {
                started.notified().await;
                let guard = tokio::time::timeout(Duration::from_millis(100), STORE.lock()).await;
                let responsive = guard.is_ok();
                if let Ok(_guard) = guard {
                    let mut state = load_from(path).unwrap();
                    state.subscriptions.clear();
                    save_to(path, &state).unwrap();
                }
                release.notify_one();
                responsive
            };
            let (result, responsive) = tokio::join!(notify, unsubscribe);
            let remaining = load_from(path).unwrap().subscriptions.len();
            std::fs::remove_file(path).unwrap();
            result.unwrap();
            assert!(
                responsive,
                "subscription storage was locked during a slow provider request"
            );
            assert_eq!(remaining, 0, "delivery resurrected an unsubscribed browser");
        }
    }
}

pub use push::{key as push_key, subscribe, unsubscribe};
pub fn start_checker() {
    tokio::spawn(async {
        let Ok(client) = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
        else {
            return;
        };
        loop {
            if let Ok(status) = check().await {
                let _ = push::notify(&client, &status).await;
            }
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_stable_and_shell_versions() {
        for version in ["", "v1.2.3;reboot", "1.2.3-beta.1", "--help"] {
            assert!(!valid_version(version));
        }
        assert!(valid_version("1.2.3-1"));
        assert!(!valid_version("1.2.3"));
    }
    #[test]
    fn validates_cli_response_and_release_link() {
        let valid = br#"{"installed_version":"1.0.8-1","latest_version":"1.0.9-1","update_available":true,"release_url":"https://github.com/o/r/releases/tag/v1.0.9","checked_at":"2026-10-27T00:00:00Z"}"#;
        assert!(parse_check(valid).is_ok());
        assert!(parse_check(br#"{"latest_version":"1.0.9-1"}"#).is_err());
        let unsafe_url = String::from_utf8(valid.to_vec())
            .unwrap()
            .replace("https://github.com/o/r/releases/tag/v1.0.9", "javascript:alert(1)");
        assert!(parse_check(unsafe_url.as_bytes()).is_err());
    }
    #[test]
    fn push_is_once_per_version_and_subscription() {
        let mut record = SubscriptionRecord {
            subscription: SubscriptionInfo::new("https://fcm.googleapis.com/fcm/send/abc", "key", "auth"),
            last_notified: None,
        };
        assert!(record.needs_version("1.0.9-1"));
        record.last_notified = Some("1.0.9-1".into());
        assert!(!record.needs_version("1.0.9-1"));
        assert!(record.needs_version("1.0.10-1"));
    }
    #[test]
    fn encrypted_push_is_readable_by_subscription_and_has_vapid() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        use p256::{
            SecretKey,
            elliptic_curve::{rand_core::OsRng, sec1::ToEncodedPoint},
        };
        let browser = SecretKey::random(&mut OsRng);
        let signing = SecretKey::random(&mut OsRng);
        let auth = [7u8; 16];
        let subscription = SubscriptionInfo::new(
            "https://fcm.googleapis.com/fcm/send/test",
            &URL_SAFE_NO_PAD.encode(browser.public_key().to_encoded_point(false).as_bytes()),
            &URL_SAFE_NO_PAD.encode(auth),
        );
        let request = push::build_request(&subscription, &signing.to_bytes(), b"version 1.0.11-1").unwrap();
        assert_eq!(request.uri().host(), Some("fcm.googleapis.com"));
        assert_eq!(request.headers()["content-encoding"], "aes128gcm");
        assert!(
            request.headers()["authorization"]
                .to_str()
                .unwrap()
                .starts_with("vapid t=")
        );
        let plaintext = web_push_native::decrypt(
            request.into_body(),
            &browser,
            &web_push_native::Auth::clone_from_slice(&auth),
        )
        .unwrap();
        assert_eq!(plaintext, b"version 1.0.11-1");
    }
    #[test]
    fn rejects_private_or_arbitrary_push_endpoints() {
        for endpoint in [
            "http://fcm.googleapis.com/x",
            "https://127.0.0.1/x",
            "https://example.com/x",
            "https://fcm.googleapis.com.evil.com/x",
            "https://fcm.googleapis.com:8443/x",
        ] {
            assert!(!valid_endpoint(endpoint));
        }
        assert!(valid_endpoint("https://fcm.googleapis.com/fcm/send/abc"));
        assert!(valid_endpoint("https://updates.push.services.mozilla.com/wpush/v2/abc"));
        assert!(valid_endpoint("https://web.push.apple.com/abc"));
    }
}
