//! OAuth2 authorization code login for the UI (enabled with AUTHORIZE_URL and ACCESS_TOKEN_URL).
//!
//! Client credentials come from `<CREDENTIALS_DIR>/authcode-client-id` and
//! `<CREDENTIALS_DIR>/authcode-client-secret`; they are read on each login, so a mounted secret can rotate.

use std::path::PathBuf;

use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, SignedCookieJar};
use rand::Rng;
use rand::distr::Alphanumeric;
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::web::{AppState, SESSION_COOKIE, SESSION_OAUTH, session_cookie};

const STATE_COOKIE: &str = "oauth_state";
pub const LOGIN_PATH: &str = "login/oauth";
const AUTHORIZED_PATH: &str = "login/oauth/authorized";

#[derive(Clone, Debug)]
pub struct OAuthConfig {
    pub authorize_url: String,
    pub access_token_url: String,
    pub scope: Option<String>,
    pub credentials_dir: PathBuf,
}

impl OAuthConfig {
    fn read_credential(&self, name: &str) -> anyhow::Result<String> {
        let path = self.credentials_dir.join(name);
        let value = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read OAuth credential {}", path.display()))?;
        Ok(value.trim().to_string())
    }
}

/// External URL of this app: ROUTE_PREFIX if it is an absolute URL, else from the request headers.
fn external_url(state: &AppState, headers: &HeaderMap, path: &str) -> String {
    let prefixed = state.prefixed(path);
    if prefixed.starts_with("http://") || prefixed.starts_with("https://") {
        return prefixed;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let scheme = header("x-forwarded-proto").unwrap_or("http");
    let host = header("x-forwarded-host")
        .or_else(|| header("host"))
        .unwrap_or("localhost");
    format!("{scheme}://{host}{prefixed}")
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: SignedCookieJar,
) -> Result<Response, StatusCode> {
    let oauth = state.config.oauth.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let client_id = oauth.read_credential("authcode-client-id").map_err(|e| {
        error!("OAuth login failed: {e:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let oauth_state: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(30)
        .map(char::from)
        .collect();
    let mut url = url::Url::parse(&oauth.authorize_url).map_err(|e| {
        error!("Invalid AUTHORIZE_URL {}: {e}", oauth.authorize_url);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair(
                "redirect_uri",
                &external_url(&state, &headers, AUTHORIZED_PATH),
            )
            .append_pair("state", &oauth_state);
        if let Some(scope) = &oauth.scope {
            query.append_pair("scope", scope);
        }
    }
    let jar = jar.add(session_cookie(STATE_COOKIE, oauth_state));
    Ok((jar, Redirect::to(url.as_str())).into_response())
}

#[derive(Deserialize)]
pub struct AuthorizedParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Access token (or error) from the token endpoint response: JSON, or form-urlencoded (GitHub without Accept).
fn parse_token_response(body: &str) -> anyhow::Result<String> {
    let fields: std::collections::HashMap<String, String> =
        match serde_json::from_str::<serde_json::Value>(body) {
            Ok(serde_json::Value::Object(map)) => map
                .into_iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k, v.to_string())))
                .collect(),
            _ => url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect(),
        };
    if let Some(error) = fields.get("error") {
        anyhow::bail!("token endpoint returned error: {error}");
    }
    match fields.get("access_token") {
        Some(token) if !token.is_empty() => Ok(token.clone()),
        _ => anyhow::bail!("token endpoint response has no access_token"),
    }
}

pub async fn authorized(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: SignedCookieJar,
    Query(params): Query<AuthorizedParams>,
) -> Response {
    let Some(oauth) = state.config.oauth.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(error) = params.error {
        warn!("OAuth provider returned error: {error}");
        return (StatusCode::UNAUTHORIZED, format!("OAuth error: {error}")).into_response();
    }
    let expected_state = jar.get(STATE_COOKIE).map(|c| c.value().to_string());
    let (Some(code), Some(got_state)) = (params.code, params.state) else {
        return (StatusCode::BAD_REQUEST, "Missing code or state").into_response();
    };
    if expected_state.as_deref() != Some(got_state.as_str()) {
        warn!("OAuth state mismatch, possible CSRF or expired login");
        return (
            StatusCode::BAD_REQUEST,
            "Invalid OAuth state, please log in again",
        )
            .into_response();
    }
    let redirect_uri = external_url(&state, &headers, AUTHORIZED_PATH);
    match fetch_token(&state.http, oauth, &code, &redirect_uri).await {
        Ok(()) => {
            info!("OAuth login succeeded");
            let jar = jar
                .remove(Cookie::build(STATE_COOKIE).path("/"))
                .add(session_cookie(SESSION_COOKIE, SESSION_OAUTH.to_string()));
            (jar, Redirect::to(&state.config.route_prefix)).into_response()
        }
        Err(e) => {
            error!(
                "OAuth token request to {} failed: {e:#}",
                oauth.access_token_url
            );
            (StatusCode::UNAUTHORIZED, "OAuth login failed").into_response()
        }
    }
}

async fn fetch_token(
    http: &reqwest::Client,
    oauth: &OAuthConfig,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<()> {
    let client_id = oauth.read_credential("authcode-client-id")?;
    let client_secret = oauth.read_credential("authcode-client-secret")?;
    let body = http
        .post(&oauth.access_token_url)
        .basic_auth(client_id, Some(client_secret))
        .header(http::header::ACCEPT, "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    parse_token_response(&body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_response_formats() {
        assert_eq!(
            parse_token_response(r#"{"access_token":"a1","expires_in":3600}"#).unwrap(),
            "a1"
        );
        assert_eq!(
            parse_token_response("access_token=b2&token_type=bearer").unwrap(),
            "b2"
        );
        assert!(parse_token_response("error=bad_verification_code").is_err());
        assert!(parse_token_response(r#"{"error":"invalid_grant"}"#).is_err());
        assert!(parse_token_response("{}").is_err());
    }
}
