//! Request identity middleware
//!
//! This module attaches the viewer's identity (or `None`) to every request.
//! Under Cloudflare Access, when the `Cf-Access-Jwt-Assertion` header is
//! present, the JWT is validated against Cloudflare's public keys. If valid,
//! the user's identity is attached to the request. If the header is absent,
//! the request proceeds anonymously.
//!
//! Environment variables:
//! - `CF_ACCESS_TEAM`: Your Cloudflare Access team name
//! - `CF_ACCESS_AUD`: Your application's audience tag from CF Access dashboard
//! - `DEV_IDENTITY` (debug builds only): attach this email to every request

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Header name for the Cloudflare Access JWT
const CF_ACCESS_JWT_HEADER: &str = "Cf-Access-Jwt-Assertion";

/// Cloudflare Access identity extracted from a validated JWT
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareIdentity {
    /// User's email address
    pub email: String,
    /// Subject (unique user identifier)
    pub sub: String,
    /// Identity provider used (e.g., "google-oauth2")
    #[serde(default)]
    pub identity_nonce: Option<String>,
    /// Token issued at (Unix timestamp)
    pub iat: u64,
    /// Token expiration (Unix timestamp)
    pub exp: u64,
}

/// JWT claims structure for Cloudflare Access tokens
#[derive(Debug, Deserialize)]
struct CfAccessClaims {
    email: String,
    sub: String,
    #[serde(default)]
    identity_nonce: Option<String>,
    iat: u64,
    exp: u64,
    // aud can be a string or array of strings
    #[allow(dead_code)]
    aud: serde_json::Value,
}

/// Cache for Cloudflare's JWKS (JSON Web Key Set)
#[derive(Clone)]
pub struct JwksCache {
    inner: Arc<RwLock<Option<JwkSet>>>,
    team: String,
}

impl JwksCache {
    /// Create a new JWKS cache for the given Cloudflare Access team
    pub fn new(team: String) -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
            team,
        }
    }

    /// Create a cache already holding `jwks`, so tests need no network
    #[cfg(test)]
    pub fn with_jwks(team: String, jwks: JwkSet) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Some(jwks))),
            team,
        }
    }

    /// The Cloudflare Access team name
    pub fn team(&self) -> &str {
        &self.team
    }

    /// Get the JWKS URL for this team
    fn jwks_url(&self) -> String {
        format!(
            "https://{}.cloudflareaccess.com/cdn-cgi/access/certs",
            self.team
        )
    }

    /// Fetch JWKS from Cloudflare (called on cache miss)
    async fn fetch_jwks(&self) -> Result<JwkSet, String> {
        let url = self.jwks_url();
        tracing::info!("Fetching JWKS from: {}", url);

        let response = reqwest::get(&url)
            .await
            .map_err(|e| format!("Failed to fetch JWKS: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("JWKS fetch returned status: {}", response.status()));
        }

        let jwks: JwkSet = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse JWKS: {}", e))?;

        tracing::info!("Successfully fetched {} keys from JWKS", jwks.keys.len());
        Ok(jwks)
    }

    /// Get the cached JWKS, fetching if necessary
    pub async fn get_jwks(&self) -> Result<JwkSet, String> {
        {
            let cache = self.inner.read().await;
            if let Some(jwks) = cache.as_ref() {
                return Ok(jwks.clone());
            }
        }

        self.fetch_and_cache().await
    }

    /// Force-refresh the cache and return the updated JWKS
    async fn fetch_and_cache(&self) -> Result<JwkSet, String> {
        let jwks = self.fetch_jwks().await?;
        let mut cache = self.inner.write().await;
        *cache = Some(jwks.clone());
        Ok(jwks)
    }

    /// Look up a key by kid, refreshing the cache on cache miss
    pub async fn find_key(&self, kid: &str) -> Result<jsonwebtoken::jwk::Jwk, String> {
        let jwks = self.get_jwks().await?;

        if let Some(jwk) = jwks.find(kid) {
            return Ok(jwk.clone());
        }

        tracing::info!("Key {} not found in cached JWKS, refreshing...", kid);
        let jwks = self.fetch_and_cache().await?;

        jwks.find(kid)
            .cloned()
            .ok_or_else(|| format!("No matching key found for kid: {}", kid))
    }

    /// Force refresh the JWKS cache
    #[allow(dead_code)]
    pub async fn refresh(&self) -> Result<(), String> {
        let jwks = self.fetch_jwks().await?;
        let mut cache = self.inner.write().await;
        *cache = Some(jwks);
        Ok(())
    }
}

/// Validate a Cloudflare Access JWT and extract the identity
pub async fn validate_cf_jwt(
    token: &str,
    jwks_cache: &JwksCache,
    audience: &str,
) -> Result<CloudflareIdentity, String> {
    // Decode header to get the key ID
    let header = decode_header(token).map_err(|e| format!("Failed to decode JWT header: {}", e))?;

    let kid = header
        .kid
        .ok_or_else(|| "JWT header missing 'kid' field".to_string())?;

    tracing::debug!("JWT uses key ID: {}", kid);

    // Get JWKS and find the matching key (refreshes cache if needed)
    let jwk = jwks_cache.find_key(&kid).await?;

    // Create decoding key from JWK
    let decoding_key = DecodingKey::from_jwk(&jwk)
        .map_err(|e| format!("Failed to create decoding key from JWK: {}", e))?;

    // Set up validation
    let mut validation = Validation::new(header.alg);
    validation.set_audience(&[audience]);
    validation.set_issuer(&[format!("https://{}.cloudflareaccess.com", jwks_cache.team)]);

    // Decode and validate the token
    let token_data = decode::<CfAccessClaims>(token, &decoding_key, &validation)
        .map_err(|e| format!("JWT validation failed: {}", e))?;

    let claims = token_data.claims;

    Ok(CloudflareIdentity {
        email: claims.email,
        sub: claims.sub,
        identity_nonce: claims.identity_nonce,
        iat: claims.iat,
        exp: claims.exp,
    })
}

/// Configuration for Cloudflare Access authentication
#[derive(Clone)]
pub struct CfAccessConfig {
    pub jwks_cache: JwksCache,
    pub audience: String,
}

/// Decide how requests get their identity from `CF_ACCESS_TEAM`, `CF_ACCESS_AUD`
/// and (debug builds only) `--dev-identity`. Empty values count as unset.
///
/// CF Access is enabled only when both team and audience are set. A dev
/// identity is refused alongside either, so it can't leak into a CF deployment.
pub fn resolve_auth_mode(
    cf_team: Option<String>,
    cf_aud: Option<String>,
    #[cfg(debug_assertions)] dev_identity: Option<String>,
) -> Result<AuthMode, String> {
    let cf_team = cf_team.filter(|s| !s.is_empty());
    let cf_aud = cf_aud.filter(|s| !s.is_empty());

    #[cfg(debug_assertions)]
    if let Some(email) = dev_identity.filter(|s| !s.is_empty()) {
        if cf_team.is_some() || cf_aud.is_some() {
            return Err(
                "--dev-identity (DEV_IDENTITY) cannot be combined with CF_ACCESS_TEAM/CF_ACCESS_AUD"
                    .to_string(),
            );
        }
        return Ok(AuthMode::DevIdentity(email));
    }

    match (cf_team, cf_aud) {
        (Some(team), Some(audience)) => Ok(AuthMode::CfAccess(CfAccessConfig {
            jwks_cache: JwksCache::new(team),
            audience,
        })),
        _ => Ok(AuthMode::Anonymous),
    }
}

/// Extract the Cloudflare Access JWT from request headers
fn extract_cf_jwt(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(CF_ACCESS_JWT_HEADER)
        .and_then(|v| v.to_str().ok())
}

/// How requests get their identity
#[derive(Clone)]
pub enum AuthMode {
    /// CF Access not configured: every request is anonymous
    Anonymous,
    /// Validate CF Access JWTs when present
    CfAccess(CfAccessConfig),
    /// Local development only: attach this email to every request
    #[cfg(debug_assertions)]
    DevIdentity(String),
}

/// Authentication middleware
///
/// Behavior under `AuthMode::CfAccess`:
/// - No JWT header present: Request proceeds with no identity (anonymous)
/// - Valid JWT present: Identity is attached to request extensions
/// - Invalid JWT present: Returns 401 Unauthorized
pub async fn auth_middleware(mode: AuthMode, mut request: Request<Body>, next: Next) -> Response {
    let config = match mode {
        AuthMode::CfAccess(c) => c,
        AuthMode::Anonymous => {
            tracing::debug!("CF Access not configured, proceeding anonymously");
            request.extensions_mut().insert(None::<CloudflareIdentity>);
            return next.run(request).await;
        }
        #[cfg(debug_assertions)]
        AuthMode::DevIdentity(email) => {
            request.extensions_mut().insert(Some(CloudflareIdentity {
                email,
                sub: "dev-identity".to_string(),
                identity_nonce: None,
                iat: 0,
                exp: u64::MAX,
            }));
            return next.run(request).await;
        }
    };

    // Check for JWT header
    let token = match extract_cf_jwt(request.headers()) {
        Some(t) => t,
        None => {
            tracing::debug!("No CF Access JWT header present, proceeding anonymously");
            request.extensions_mut().insert(None::<CloudflareIdentity>);
            return next.run(request).await;
        }
    };

    // Validate the JWT
    match validate_cf_jwt(token, &config.jwks_cache, &config.audience).await {
        Ok(identity) => {
            tracing::info!("Authenticated user: {}", identity.email);
            request.extensions_mut().insert(Some(identity));
            next.run(request).await
        }
        Err(e) => {
            tracing::warn!("JWT validation failed: {}", e);
            (
                StatusCode::UNAUTHORIZED,
                format!("Invalid authentication token: {}", e),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jwks_url() {
        let cache = JwksCache::new("myteam".to_string());
        assert_eq!(
            cache.jwks_url(),
            "https://myteam.cloudflareaccess.com/cdn-cgi/access/certs"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn dev_identity_is_refused_alongside_cf_access_settings() {
        let dev = || Some("me@example.com".to_string());
        for (team, aud) in [
            (Some("myteam"), None),
            (None, Some("my-aud")),
            (Some("myteam"), Some("my-aud")),
        ] {
            let result = resolve_auth_mode(team.map(Into::into), aud.map(Into::into), dev());
            let err = result.err().expect("should refuse to start");
            assert!(err.contains("--dev-identity"), "unclear error: {err}");
        }
    }

    #[test]
    fn cf_access_needs_both_team_and_audience() {
        let resolve = |team: Option<&str>, aud: Option<&str>| {
            resolve_auth_mode(
                team.map(Into::into),
                aud.map(Into::into),
                #[cfg(debug_assertions)]
                None,
            )
            .unwrap()
        };
        assert!(matches!(resolve(None, None), AuthMode::Anonymous));
        assert!(matches!(resolve(Some("myteam"), None), AuthMode::Anonymous));
        assert!(matches!(
            resolve(Some("myteam"), Some("")),
            AuthMode::Anonymous
        ));
        assert!(matches!(
            resolve(Some("myteam"), Some("my-aud")),
            AuthMode::CfAccess(_)
        ));
    }
}
