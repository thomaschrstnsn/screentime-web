//! Cloudflare Access authentication middleware
//!
//! This module provides optional JWT validation for requests coming through
//! Cloudflare Access. When the `Cf-Access-Jwt-Assertion` header is present,
//! the JWT is validated against Cloudflare's public keys. If valid, the user's
//! identity is attached to the request. If the header is absent, the request
//! proceeds anonymously.
//!
//! Environment variables:
//! - `CF_ACCESS_TEAM`: Your Cloudflare Access team name
//! - `CF_ACCESS_AUD`: Your application's audience tag from CF Access dashboard

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
        // Check cache first
        {
            let cache = self.inner.read().await;
            if let Some(jwks) = cache.as_ref() {
                return Ok(jwks.clone());
            }
        }

        // Fetch and cache
        let jwks = self.fetch_jwks().await?;
        {
            let mut cache = self.inner.write().await;
            *cache = Some(jwks.clone());
        }

        Ok(jwks)
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

    // Get JWKS and find the matching key
    let jwks = jwks_cache.get_jwks().await?;

    let jwk = jwks
        .find(&kid)
        .ok_or_else(|| format!("No matching key found for kid: {}", kid))?;

    // Create decoding key from JWK
    let decoding_key = DecodingKey::from_jwk(jwk)
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

impl CfAccessConfig {
    /// Create config from environment variables
    ///
    /// Returns None if CF_ACCESS_TEAM or CF_ACCESS_AUD are not set
    pub fn from_env() -> Option<Self> {
        let team = std::env::var("CF_ACCESS_TEAM").ok()?;
        let audience = std::env::var("CF_ACCESS_AUD").ok()?;

        if team.is_empty() || audience.is_empty() {
            return None;
        }

        tracing::info!(
            "Cloudflare Access authentication enabled for team: {}",
            team
        );

        Some(Self {
            jwks_cache: JwksCache::new(team),
            audience,
        })
    }
}

/// Extract the Cloudflare Access JWT from request headers
fn extract_cf_jwt(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(CF_ACCESS_JWT_HEADER)
        .and_then(|v| v.to_str().ok())
}

/// Authentication middleware for Cloudflare Access
///
/// Behavior:
/// - No JWT header present: Request proceeds with no identity (anonymous)
/// - Valid JWT present: Identity is attached to request extensions
/// - Invalid JWT present: Returns 401 Unauthorized
pub async fn cf_access_middleware(
    config: Option<CfAccessConfig>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // If CF Access is not configured, proceed without auth
    let config = match config {
        Some(c) => c,
        None => {
            tracing::debug!("CF Access not configured, proceeding anonymously");
            request.extensions_mut().insert(None::<CloudflareIdentity>);
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

    #[test]
    fn test_config_from_env_missing() {
        // Should return None when env vars are not set
        unsafe { std::env::remove_var("CF_ACCESS_TEAM") };
        unsafe { std::env::remove_var("CF_ACCESS_AUD") };
        assert!(CfAccessConfig::from_env().is_none());
    }
}
