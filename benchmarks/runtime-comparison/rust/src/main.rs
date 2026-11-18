use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use serde_json::Value;
use std::{sync::Arc, time::Duration};

struct App {
    fixture: Value,
    client: reqwest::Client,
    token: Option<String>,
    upstream: String,
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str, default: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
            .unwrap_or_else(|| default.to_owned())
    };
    let address = arg("--listen", "127.0.0.1:18081");
    let fixture =
        serde_json::from_slice(&std::fs::read(arg("--fixture", "fixture.json")).unwrap()).unwrap();
    let token = std::fs::read("/opt/etc/xkeen/xkeen.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            value
                .get("xkeen")?
                .get("rci_token")?
                .as_str()
                .map(str::to_owned)
        });
    let state = Arc::new(App {
        fixture,
        client: reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .unwrap(),
        token,
        upstream: std::env::var("BENCH_RCI_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:79/rci/show/system".to_owned()),
    });
    let router = Router::new()
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"success": true})) }),
        )
        .route(
            "/json",
            get(|State(state): State<Arc<App>>| async move {
                (
                    [("content-type", "application/json")],
                    serde_json::to_vec(&state.fixture).unwrap(),
                )
            }),
        )
        .route("/system", get(system))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&address).await.unwrap();
    eprintln!("benchmark server {address}");
    axum::serve(listener, router).await.unwrap();
}

async fn system(State(state): State<Arc<App>>) -> axum::response::Response {
    let mut request = state.client.get(&state.upstream);
    if let Some(token) = &state.token {
        request = request.header("X-Ndma-Tkn", token);
    }
    let Ok(response) = request.send().await else {
        return (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response();
    };
    if response.status() != StatusCode::OK {
        return (StatusCode::BAD_GATEWAY, "upstream status").into_response();
    }
    let Ok(body) = response.bytes().await else {
        return (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response();
    };
    if body.len() > 1024 * 1024 {
        return (StatusCode::BAD_GATEWAY, "upstream too large").into_response();
    }
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_GATEWAY, "invalid upstream JSON").into_response();
    };
    Json(value).into_response()
}
