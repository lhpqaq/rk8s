use crate::config::auth::AuthConfig;
use crate::login::oauth::OAuthFlow;
use crate::login::types::{
    CallbackResponse, LoginUrlResponse, NextCallbackQuery, RequestClientIdResponse,
};
use crate::registry::{
    RegistryScheme, api_url, effective_skip_tls_verify, parse_registry_host_arg,
};
use crate::rt::block_on;
use crate::utils::cli::RequestBuilderExt;
use axum::http::HeaderMap;
use clap::Parser;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};

mod oauth;
mod types;

fn client(skip_tls_verify: bool) -> anyhow::Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert("Accept", "application/json".parse().unwrap());

    Client::builder()
        .default_headers(headers)
        .danger_accept_invalid_certs(skip_tls_verify)
        .build()
        .map_err(Into::into)
}

#[derive(Debug, Parser)]
pub struct LoginArgs {
    /// Registry host in `host[:port]` format (optional if only one server is configured).
    #[arg(value_parser = parse_registry_host_arg)]
    url: Option<String>,
    /// Skip TLS certificate verification for HTTPS registry.
    #[arg(long)]
    skip_tls_verify: bool,
}

pub fn login(args: LoginArgs) -> anyhow::Result<()> {
    let config = AuthConfig::load()?;
    let registry = match args.url {
        Some(url) => url,
        None => config.single_entry()?.url.to_string(),
    };
    let scheme = config.registry_scheme(&registry);
    let skip_tls_verify = effective_skip_tls_verify(args.skip_tls_verify, scheme, &registry);
    let client = client(skip_tls_verify)?;

    block_on(async move {
        // Try Next program auth flow first.
        match try_next_login(&client, &registry, scheme).await {
            Ok(token) => {
                AuthConfig::login(token, &registry)?;
                println!("Logged in successfully!");
                return Ok(());
            }
            Err(e) => {
                tracing::debug!("Next auth flow unavailable, falling back to GitHub OAuth: {e}");
            }
        }

        // Fallback to GitHub OAuth flow.
        let res = request_client_id(&client, &registry, scheme).await?;
        let client_id = &res.client_id;

        let oauth = OAuthFlow::new(client_id);
        let res = oauth.request_token().await?;

        let req_url = api_url(scheme, &registry, "api/v1/auth/github/callback");
        let res = client
            .post(req_url)
            .json(&res)
            .send_and_json::<CallbackResponse>()
            .await?;

        AuthConfig::login(res.pat, &registry)?;
        println!("Logged in successfully!");
        Ok(())
    })?
}

/// Attempt browser-based login via the Next program.
///
/// 1. Start a local HTTP server to receive the callback.
/// 2. Request the login URL from the distribution server.
/// 3. Open the browser for the user to authenticate.
/// 4. Wait for the callback with the token.
async fn try_next_login(
    client: &Client,
    registry: &str,
    scheme: RegistryScheme,
) -> anyhow::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let callback_url = format!("http://127.0.0.1:{port}/callback");

    // Request login URL from distribution.
    let url = api_url(scheme, registry, "api/v1/auth/next/login_url");
    let res: LoginUrlResponse = client
        .get(url)
        .query(&[("callback_url", &callback_url)])
        .send_and_json()
        .await?;

    println!("Open the following URL in your browser to log in:");
    println!("  {}", res.login_url);
    try_open_browser(&res.login_url);

    // Shared storage for the received token.
    let token_store: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let notify = Arc::new(Notify::new());

    let ts = token_store.clone();
    let nt = notify.clone();

    let app = axum::Router::new().route(
        "/callback",
        axum::routing::get(
            move |axum::extract::Query(q): axum::extract::Query<NextCallbackQuery>| {
                let ts = ts.clone();
                let nt = nt.clone();
                async move {
                    *ts.lock().await = Some(q.token);
                    nt.notify_one();
                    axum::response::Html(
                        "<html><body><h1>Login successful!</h1>\
                             <p>You can close this window and return to the terminal.</p>\
                             </body></html>",
                    )
                }
            },
        ),
    );

    // Wait for the callback with a 5-minute timeout.
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let timed_out = tokio::select! {
        _ = notify.notified() => false,
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => true,
    };

    // Shut down the callback server.
    server_handle.abort();

    if timed_out {
        anyhow::bail!("Login timed out after 5 minutes");
    }

    token_store
        .lock()
        .await
        .take()
        .ok_or_else(|| anyhow::anyhow!("No token received"))
}

fn try_open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn();
    }
}

async fn request_client_id(
    client: &Client,
    registry: impl AsRef<str>,
    scheme: RegistryScheme,
) -> anyhow::Result<RequestClientIdResponse> {
    let url = api_url(scheme, registry, "api/v1/auth/github/client_id");
    client.get(url).send_and_json().await
}
