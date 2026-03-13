pub mod middleware;
pub mod v2;

use crate::api::middleware::{
    authorize_repository_access, populate_oci_claims, require_authentication,
};
use crate::api::v2::probe;
use crate::domain::user::UserRepository;
use crate::error::{AppError, OciError};
use crate::service::auth::{auth, client_id, login_url, oauth_callback};
use crate::service::repo::{change_visibility, list_visible_repos};
use crate::utils::jwt::{Claims, decode};
use crate::utils::password::check_password;
use crate::utils::state::AppState;
use axum::Json;
use axum::Router;
use axum::extract::{OptionalFromRequestParts, State};
use axum::http::request::Parts;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum_extra::TypedHeader;
use axum_extra::headers::Authorization;
use axum_extra::headers::authorization::{Basic, Bearer};
use serde::Deserialize;
use std::sync::Arc;

pub fn create_router(state: Arc<AppState>) -> Router<()> {
    // we need to handle both /v2 and /v2/
    #[allow(unused_mut)]
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/v2/", get(probe))
        .nest("/v2", v2::create_v2_router(state.clone()))
        .nest("/api/v1", custom_v1_router(state.clone()))
        .route("/auth/token", get(auth));

    #[cfg(debug_assertions)]
    {
        router = router.nest("/debug", debug_router(state.clone()));
    }
    router.with_state(state)
}

pub async fn healthz(State(_): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    Ok(Json("http ready"))
}

fn custom_v1_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/auth/{provider}/callback", post(oauth_callback))
        .route("/auth/{provider}/client_id", get(client_id))
        .route("/auth/next/login_url", get(login_url))
        .merge(v1_router_with_auth(state))
}

fn v1_router_with_auth(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/repo",
            get(list_visible_repos).layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_authentication,
            )),
        )
        .route(
            "/{*tail}",
            put(change_visibility)
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    authorize_repository_access,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state,
                    populate_oci_claims,
                )),
        )
}

#[cfg(debug_assertions)]
fn debug_router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    use crate::service::auth::create_user;

    Router::new()
        .route("/users", post(create_user))
        .with_state(state)
}

pub enum AuthHeader {
    Bearer(TypedHeader<Authorization<Bearer>>),
    Basic(TypedHeader<Authorization<Basic>>),
}

impl<S> OptionalFromRequestParts<S> for AuthHeader
where
    S: Send + Sync,
{
    type Rejection = ();

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        if let Ok(header) =
            <TypedHeader<_> as OptionalFromRequestParts<_>>::from_request_parts(parts, state).await
            && let Some(header) = header
        {
            return Ok(Some(Self::Bearer(header)));
        };
        if let Ok(header) =
            <TypedHeader<_> as OptionalFromRequestParts<_>>::from_request_parts(parts, state).await
            && let Some(header) = header
        {
            return Ok(Some(Self::Basic(header)));
        };
        Ok(None)
    }
}

/// Response from the Next program's token verification endpoint.
#[derive(Deserialize)]
pub(crate) struct NextAuthUserInfo {
    pub username: String,
}

/// Verify a Bearer token by calling the Next program's `/api/auth/verify` endpoint.
pub(crate) async fn verify_token_with_next(
    client: &reqwest::Client,
    next_auth_url: &str,
    token: &str,
) -> Result<Claims, AppError> {
    let url = format!("{}/api/auth/verify", next_auth_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| OciError::Unauthorized {
            msg: format!("Failed to verify token with auth service: {e}"),
            auth_url: None,
        })?;

    if !response.status().is_success() {
        return Err(OciError::Unauthorized {
            msg: "Token verification failed".to_string(),
            auth_url: None,
        }
        .into());
    }

    let user_info: NextAuthUserInfo =
        response.json().await.map_err(|e| OciError::Unauthorized {
            msg: format!("Invalid response from auth service: {e}"),
            auth_url: None,
        })?;

    Ok(Claims {
        sub: user_info.username.to_ascii_lowercase(),
        exp: 0,
    })
}

pub(crate) async fn extract_claims(
    auth: Option<AuthHeader>,
    jwt_secret: impl AsRef<str>,
    user_storage: &dyn UserRepository,
    auth_url: impl AsRef<str>,
    http_client: &reqwest::Client,
    next_auth_url: Option<&str>,
) -> Result<Claims, AppError> {
    match auth {
        Some(auth) => match auth {
            AuthHeader::Bearer(bearer) => {
                if let Some(next_url) = next_auth_url {
                    // In delegated mode, try decoding as a distribution-issued JWT
                    // first (issued by /auth/token). Only delegate to Next when the
                    // local decode fails, so the standard OCI token-exchange path
                    // (/v2 challenge → /auth/token → /v2/* with JWT) keeps working.
                    match decode(&jwt_secret, bearer.token()) {
                        Ok(claims) => Ok(claims),
                        Err(_) => {
                            verify_token_with_next(http_client, next_url, bearer.token()).await
                        }
                    }
                } else {
                    // Fallback: decode JWT locally.
                    decode(jwt_secret, bearer.token())
                }
            }
            AuthHeader::Basic(basic) => {
                if next_auth_url.is_some() {
                    return Err(OciError::Unauthorized {
                        msg: "Basic auth is not supported with delegated authentication"
                            .to_string(),
                        auth_url: Some(auth_url.as_ref().to_string()),
                    }
                    .into());
                }
                let user = user_storage.query_user_by_name(basic.username()).await?;
                check_password(&user.salt, &user.password, basic.password())?;
                Ok(Claims {
                    sub: basic.username().to_string(),
                    exp: 0,
                })
            }
        },
        None => Err(OciError::Unauthorized {
            msg: "Missing `authorization` header or invalid `authorization` header".to_string(),
            auth_url: Some(auth_url.as_ref().to_string()),
        }
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that verify_token_with_next successfully returns claims
    /// when the Next program responds with valid user info.
    #[tokio::test]
    async fn verify_token_with_next_success() {
        let mock_server = axum::Router::new().route(
            "/api/auth/verify",
            axum::routing::get(|| async { Json(serde_json::json!({ "username": "testuser" })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_server).await.unwrap();
        });

        let client = reqwest::Client::new();
        let result =
            verify_token_with_next(&client, &format!("http://{addr}"), "valid-token").await;

        let claims = result.unwrap();
        assert_eq!(claims.sub, "testuser");
    }

    /// Test that verify_token_with_next returns an error
    /// when the Next program responds with 401.
    #[tokio::test]
    async fn verify_token_with_next_unauthorized() {
        let mock_server = axum::Router::new().route(
            "/api/auth/verify",
            axum::routing::get(|| async { (axum::http::StatusCode::UNAUTHORIZED, "Unauthorized") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_server).await.unwrap();
        });

        let client = reqwest::Client::new();
        let result =
            verify_token_with_next(&client, &format!("http://{addr}"), "invalid-token").await;

        assert!(result.is_err());
    }

    /// Test that verify_token_with_next trims trailing slashes from the URL.
    #[tokio::test]
    async fn verify_token_with_next_trims_trailing_slash() {
        let mock_server = axum::Router::new().route(
            "/api/auth/verify",
            axum::routing::get(|| async { Json(serde_json::json!({ "username": "slashuser" })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_server).await.unwrap();
        });

        let client = reqwest::Client::new();
        let result = verify_token_with_next(&client, &format!("http://{addr}/"), "token").await;

        let claims = result.unwrap();
        assert_eq!(claims.sub, "slashuser");
    }

    /// Test that verify_token_with_next canonicalizes (lowercases) the username.
    #[tokio::test]
    async fn verify_token_with_next_canonicalizes_username() {
        let mock_server = axum::Router::new().route(
            "/api/auth/verify",
            axum::routing::get(|| async { Json(serde_json::json!({ "username": "MixedCase" })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, mock_server).await.unwrap();
        });

        let client = reqwest::Client::new();
        let result =
            verify_token_with_next(&client, &format!("http://{addr}"), "valid-token").await;

        let claims = result.unwrap();
        assert_eq!(claims.sub, "mixedcase");
    }

    /// Test that extract_claims in delegated mode decodes distribution-issued
    /// JWTs locally instead of forwarding them to the Next program.
    #[tokio::test]
    async fn extract_claims_prefers_local_jwt_in_delegated_mode() {
        use crate::utils::jwt::gen_token;

        let secret = "test-secret";
        // Issue a distribution JWT
        let jwt = gen_token(3600, secret, "localuser");

        let bearer = TypedHeader(Authorization::bearer(&jwt).unwrap());
        let auth_header = Some(AuthHeader::Bearer(bearer));

        // Create a stub UserRepository that is never called in the Bearer path.
        struct StubUserRepo;
        #[async_trait::async_trait]
        impl crate::domain::user::UserRepository for StubUserRepo {
            async fn query_user_by_name(
                &self,
                _: &str,
            ) -> std::result::Result<crate::domain::user::User, crate::error::AppError> {
                unimplemented!()
            }
            async fn query_user_by_github_id(
                &self,
                _: i64,
            ) -> std::result::Result<crate::domain::user::User, crate::error::AppError> {
                unimplemented!()
            }
            async fn create_user(
                &self,
                _: crate::domain::user::User,
            ) -> std::result::Result<(), crate::error::AppError> {
                unimplemented!()
            }
        }

        let client = reqwest::Client::new();

        // next_auth_url is set, but the JWT was issued by distribution.
        // extract_claims should decode it locally without contacting Next.
        let claims = extract_claims(
            auth_header,
            secret,
            &StubUserRepo,
            "http://registry",
            &client,
            Some("http://next-that-does-not-exist:9999"),
        )
        .await
        .expect("should decode distribution JWT locally");

        assert_eq!(claims.sub, "localuser");
    }
}
