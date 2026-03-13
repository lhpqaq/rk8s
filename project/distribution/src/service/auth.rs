use crate::api::{AuthHeader, verify_token_with_next};
use crate::domain::user::User;
use crate::error::{AppError, BusinessError, InternalError, MapToAppError};
use crate::utils::jwt::gen_token;
use crate::utils::password::{check_password, gen_password, gen_salt, hash_password};
use crate::utils::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use chrono::Utc;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

fn canonical_namespace(value: &str) -> String {
    value.to_ascii_lowercase()
}

#[derive(Deserialize)]
pub struct OAuthCallbackRequest {
    access_token: String,
    token_type: String,
    scope: String,
}

pub async fn oauth_callback(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    Json(req): Json<OAuthCallbackRequest>,
) -> Result<impl IntoResponse, AppError> {
    match provider.as_str() {
        "github" => {
            let user_info = request_user_info(&req.access_token)
                .await
                .map_to_internal()?;

            match state
                .user_storage
                .query_user_by_github_id(user_info.id)
                .await
            {
                Ok(user) => {
                    let canonical_username = canonical_namespace(&user.username);
                    let pat = gen_token(
                        state.config.jwt_lifetime_secs,
                        &state.config.jwt_secret,
                        &canonical_username,
                    );

                    Ok((
                        StatusCode::OK,
                        Json(json!({
                            "pat": pat,
                        })),
                    ))
                }
                Err(_) => {
                    let salt = gen_salt();
                    let original_password = gen_password();
                    let hashed = hash_password(&salt, &original_password)?;
                    let canonical_username = canonical_namespace(&user_info.login);

                    let pat = gen_token(
                        state.config.jwt_lifetime_secs,
                        &state.config.jwt_secret,
                        &canonical_username,
                    );
                    let res = Json(json!({
                        "pat": pat,
                    }));

                    let user = User::new(user_info.id, canonical_username, hashed, salt);
                    state.user_storage.create_user(user).await?;
                    Ok((StatusCode::CREATED, res))
                }
            }
        }
        _ => Err(BusinessError::BadRequest("Only support github provider".to_string()).into()),
    }
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct RequestAccessTokenResponse {
    #[serde(rename = "access_token")]
    access_token: String,
    #[serde(rename = "token_type")]
    token_type: String,
    #[serde(rename = "scope")]
    scope: String,
}

async fn request_access_token(
    code: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<RequestAccessTokenResponse, reqwest::Error> {
    let mut params = HashMap::new();
    params.insert("code", code);
    params.insert("client_id", client_id);
    params.insert("client_secret", client_secret);

    let client = reqwest::Client::new();
    let res = client
        .post("https://github.com/login/oauth/access_token")
        .form(&params)
        .header("Accept", "application/json")
        .send()
        .await?;
    res.json().await
}

#[derive(Deserialize, Debug)]
pub struct UserInfo {
    login: String,
    id: i64,
}

async fn request_user_info(access_token: &str) -> Result<UserInfo, reqwest::Error> {
    let client = reqwest::Client::new();
    let res = client
        .get("https://api.github.com/user")
        .header("User-Agent", "distribution")
        .header("Authorization", format!("token {access_token}"))
        .send()
        .await?;
    res.json().await
}

#[derive(Serialize)]
pub struct AuthResponse {
    token: String,
    #[serde(rename = "access_token")]
    access_token: String,
    #[serde(rename = "expires_in")]
    expires_in: i64,
    #[serde(rename = "issued_at")]
    issued_at: String,
}

pub(crate) async fn auth(
    State(state): State<Arc<AppState>>,
    auth: Option<AuthHeader>,
) -> Result<impl IntoResponse, AppError> {
    let token = match auth {
        Some(AuthHeader::Bearer(bearer)) => {
            if let Some(next_url) = &state.config.next_auth_url {
                // Verify the bearer token with the Next program.
                let claims =
                    verify_token_with_next(&state.http_client, next_url, bearer.token()).await?;
                gen_token(
                    state.config.jwt_lifetime_secs,
                    &state.config.jwt_secret,
                    &claims.sub,
                )
            } else {
                // Fallback: treat as local JWT.
                let claims = crate::utils::jwt::decode(&state.config.jwt_secret, bearer.token())?;
                gen_token(
                    state.config.jwt_lifetime_secs,
                    &state.config.jwt_secret,
                    &claims.sub,
                )
            }
        }
        Some(AuthHeader::Basic(basic_header)) => {
            let username = basic_header.username();
            let user = state.user_storage.query_user_by_name(username).await?;
            let canonical_username = canonical_namespace(&user.username);
            let token = gen_token(
                state.config.jwt_lifetime_secs,
                &state.config.jwt_secret,
                &canonical_username,
            );
            {
                // Check password is a rather time-consuming operation. So it should be executed in `spawn_blocking`.
                tokio::task::spawn_blocking(move || {
                    check_password(&user.salt, &user.password, basic_header.password())
                })
                .await
                .map_err(|e| InternalError::Others(e.to_string()))??;
            }
            token
        }
        None => gen_token(
            state.config.jwt_lifetime_secs,
            &state.config.jwt_secret,
            "anonymous",
        ),
    };
    Ok(Json(AuthResponse {
        token: token.clone(),
        access_token: token,
        expires_in: state.config.jwt_lifetime_secs,
        issued_at: Utc::now().to_rfc3339(),
    }))
}

#[derive(Deserialize)]
pub struct LoginUrlQuery {
    callback_url: String,
}

/// Returns the Next program's login URL for browser-based authentication.
pub async fn login_url(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LoginUrlQuery>,
) -> Result<impl IntoResponse, AppError> {
    let next_auth_url = state
        .config
        .next_auth_url
        .as_deref()
        .ok_or_else(|| BusinessError::BadRequest("Next auth is not configured".to_string()))?;

    let base = next_auth_url.trim_end_matches('/');
    let mut url = reqwest::Url::parse(&format!("{base}/auth/login")).map_err(|e| {
        InternalError::Others(format!(
            "Failed to construct login URL from NEXT_AUTH_URL: {e}"
        ))
    })?;
    url.query_pairs_mut()
        .append_pair("callback_url", &params.callback_url);

    Ok(Json(json!({
        "login_url": url.to_string(),
    })))
}

pub async fn client_id(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    match path.as_str() {
        "github" => Ok(Json(json!({
            "client_id": state.config.github_client_id,
        }))),
        _ => Err(BusinessError::BadRequest("Only support github provider".to_string()).into()),
    }
}

#[cfg(debug_assertions)]
#[derive(Deserialize)]
pub struct CreateUserRequest {
    username: String,
    password: String,
}

#[cfg(debug_assertions)]
pub async fn create_user(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, AppError> {
    use rand::{Rng, SeedableRng};

    let mut rng = rand::rngs::StdRng::from_os_rng();

    let salt = gen_salt();
    let password = hash_password(&salt, &req.password)?;
    let canonical_username = canonical_namespace(&req.username);
    let user = User::new(rng.random(), canonical_username, password, salt);
    state.user_storage.create_user(user).await?;
    Ok(StatusCode::CREATED)
}

#[cfg(test)]
mod tests {
    use super::canonical_namespace;

    #[test]
    fn canonical_namespace_is_lowercase() {
        assert_eq!(canonical_namespace("LingBou"), "lingbou");
    }

    #[test]
    fn login_url_constructs_correct_url() {
        let next_auth_url = "https://auth.example.com";
        let callback_url = "http://127.0.0.1:12345/callback";

        let base = next_auth_url.trim_end_matches('/');
        let mut url = reqwest::Url::parse(&format!("{base}/auth/login")).unwrap();
        url.query_pairs_mut()
            .append_pair("callback_url", callback_url);

        let result = url.to_string();
        assert!(result.starts_with("https://auth.example.com/auth/login?"));
        assert!(result.contains("callback_url="));
        assert!(result.contains("127.0.0.1"));
    }

    #[test]
    fn login_url_trims_trailing_slash() {
        let next_auth_url = "https://auth.example.com/";
        let base = next_auth_url.trim_end_matches('/');
        let url = reqwest::Url::parse(&format!("{base}/auth/login")).unwrap();
        assert_eq!(url.path(), "/auth/login");
    }
}
