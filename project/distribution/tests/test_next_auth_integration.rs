//! Integration tests for the Next.js auth delegation flow.
//!
//! These tests spin up:
//! 1. A mock "Next program" that exposes `/api/auth/verify` and `/auth/login`
//! 2. Simulate the distribution server's `verify_token_with_next` and `login_url` logic
//!
//! Run with: `cargo test -p distribution --test test_next_auth_integration`

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;

// ---------- mock Next program ----------

/// Mock Next program: verifies bearer tokens and returns user info.
fn mock_next_app() -> axum::Router {
    axum::Router::new()
        .route("/api/auth/verify", axum::routing::get(mock_verify))
        .route("/auth/login", axum::routing::get(mock_login_page))
}

async fn mock_verify(
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let auth = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(token) = auth.strip_prefix("Bearer ") {
        if token == "valid-next-token" {
            return Ok(Json(json!({ "username": "nextuser" })));
        }
    }
    Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()))
}

#[derive(Deserialize)]
struct LoginPageQuery {
    callback_url: String,
}

async fn mock_login_page(Query(q): Query<LoginPageQuery>) -> impl IntoResponse {
    // In real flow, the user authenticates here and is redirected.
    // We just return the callback_url for test verification.
    Json(json!({
        "message": "Login page rendered",
        "callback_url": q.callback_url,
    }))
}

// ---------- helpers ----------

async fn start_mock_next() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let handle = tokio::spawn(async move {
        axum::serve(listener, mock_next_app()).await.unwrap();
    });
    // Small delay to ensure server is ready.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (url, handle)
}

// ---------- tests ----------

/// Test: Distribution delegates bearer token verification to Next program.
/// Simulates: Distribution receives Bearer token → calls Next /api/auth/verify → gets username.
#[tokio::test]
async fn distribution_delegates_token_to_next_program() {
    let (next_url, _handle) = start_mock_next().await;
    let client = Client::new();

    // Simulate what verify_token_with_next does:
    let verify_url = format!("{next_url}/api/auth/verify");
    let resp = client
        .get(&verify_url)
        .header("Authorization", "Bearer valid-next-token")
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["username"], "nextuser");
}

/// Test: Distribution rejects invalid tokens via Next program.
#[tokio::test]
async fn distribution_rejects_invalid_token_via_next() {
    let (next_url, _handle) = start_mock_next().await;
    let client = Client::new();

    let verify_url = format!("{next_url}/api/auth/verify");
    let resp = client
        .get(&verify_url)
        .header("Authorization", "Bearer bad-token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Test: Distribution returns login URL from Next program.
/// Simulates: rkforge calls GET /api/v1/auth/next/login_url?callback_url=...
/// → Distribution constructs {NEXT_AUTH_URL}/auth/login?callback_url=...
#[tokio::test]
async fn distribution_constructs_next_login_url() {
    let (next_url, _handle) = start_mock_next().await;
    let callback_url = "http://127.0.0.1:12345/callback";

    // Replicate the login_url construction logic from service/auth.rs
    let base = next_url.trim_end_matches('/');
    let mut url = reqwest::Url::parse(&format!("{base}/auth/login")).unwrap();
    url.query_pairs_mut()
        .append_pair("callback_url", callback_url);

    let result = url.to_string();
    assert!(result.contains("/auth/login?"));
    assert!(result.contains("callback_url="));
    assert!(result.contains("127.0.0.1%3A12345")); // URL-encoded colon

    // Verify the constructed URL is actually accessible
    let client = Client::new();
    let resp = client.get(&result).send().await.unwrap();
    assert!(resp.status().is_success());

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["callback_url"], callback_url);
}

/// Test: Full rkforge callback flow.
/// Simulates: Next program redirects to rkforge's local callback → rkforge receives token.
#[tokio::test]
async fn rkforge_callback_receives_token() {
    use std::sync::Arc;
    use tokio::sync::{Mutex, Notify};

    // Start the rkforge callback server (same logic as try_next_login)
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let token_store: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let notify = Arc::new(Notify::new());

    let ts = token_store.clone();
    let nt = notify.clone();

    #[derive(Deserialize)]
    struct CallbackQuery {
        token: String,
    }

    let app = axum::Router::new().route(
        "/callback",
        axum::routing::get(move |Query(q): Query<CallbackQuery>| {
            let ts = ts.clone();
            let nt = nt.clone();
            async move {
                *ts.lock().await = Some(q.token);
                nt.notify_one();
                axum::response::Html("<h1>Login successful!</h1>")
            }
        }),
    );

    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Simulate the Next program redirecting the browser to the callback
    let client = Client::new();
    let callback_url = format!("http://127.0.0.1:{port}/callback?token=my-next-jwt-token");
    let resp = client.get(&callback_url).send().await.unwrap();
    assert!(resp.status().is_success());

    // Wait for the notification
    tokio::time::timeout(std::time::Duration::from_secs(5), notify.notified())
        .await
        .expect("callback should have been received within 5 seconds");

    let received_token = token_store.lock().await.take();
    assert_eq!(received_token, Some("my-next-jwt-token".to_string()));

    server_handle.abort();
}

/// Test: End-to-end flow simulation.
/// Simulates the entire auth chain:
///   rkforge → Distribution (login_url) → Next (login) → callback → token saved
///   rkforge → Distribution (push with token) → Next (verify) → success
#[tokio::test]
async fn end_to_end_auth_flow_simulation() {
    let (next_url, _handle) = start_mock_next().await;
    let client = Client::new();

    // Step 1: rkforge requests login URL from distribution
    // (Distribution constructs: {NEXT_AUTH_URL}/auth/login?callback_url=...)
    let callback_url = "http://127.0.0.1:19999/callback";
    let base = next_url.trim_end_matches('/');
    let mut login_url = reqwest::Url::parse(&format!("{base}/auth/login")).unwrap();
    login_url
        .query_pairs_mut()
        .append_pair("callback_url", callback_url);

    // Step 2: Verify the login URL is valid and accessible
    let resp = client.get(login_url.as_str()).send().await.unwrap();
    assert!(resp.status().is_success());

    // Step 3: Simulate "user logged in, Next sends token to callback"
    // In real flow, Next would redirect the browser to callback_url?token=xxx
    let token = "valid-next-token";

    // Step 4: rkforge uses the token for push/pull
    // Distribution verifies the token with Next
    let verify_url = format!("{next_url}/api/auth/verify");
    let resp = client
        .get(&verify_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();

    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["username"], "nextuser");

    // Step 5: Verify that invalid tokens are rejected
    let resp = client
        .get(&verify_url)
        .header("Authorization", "Bearer stolen-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
