use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Uri};
use axum::middleware::Next;
use axum::response::Response;
use dashmap::DashMap;
use ipnet::IpNet;
use rustyfin_model::auth::Claims;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use crate::error::ApiError;
use crate::state::AppState;

/// Reads the credentials of a request and returns the claims they prove, plus the user token
/// (`None` for the configured API key, which has no token to revoke).
///
/// The token comes from the Bearer header, `X-Emby-Token`, `X-MediaBrowser-Token` or the `api_key`
/// query parameter. The configured API key gives administrator claims. Any other token must be a
/// live user token. Takes the request's headers and URI rather than the request: axum's `Body` is
/// not `Sync`, so a `&Request` cannot be held across the await.
async fn request_claims(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<(Claims, Option<String>), ApiError> {
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string())
        .or_else(|| {
            // Jellyfin clients also send token via X-Emby-Token
            headers
                .get("X-Emby-Token")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            // Also check X-MediaBrowser-Token
            headers
                .get("X-MediaBrowser-Token")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            // Check query parameter api_key
            uri.query()
                .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("api_key=")))
                .map(|s| s.to_string())
        })
        .ok_or(ApiError::Unauthorized)?;

    // Check if the token matches a configured API key
    if let Some(ref api_key) = state.api_key
        && token == **api_key
    {
        // API key auth: create admin claims
        return Ok((
            Claims {
                sub: uuid::Uuid::nil(),
                name: "ApiKey".to_string(),
                is_admin: true,
                iat: 0,
                exp: i64::MAX,
            },
            None,
        ));
    }

    // Otherwise the token must be a live user token
    let claims = rustyfin_auth::validate_token(&state.db_ro, &token)
        .await
        .map_err(|_| ApiError::Unauthorized)?;
    Ok((claims, Some(token)))
}

pub async fn require_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let (claims, token) = request_claims(&state, req.headers(), req.uri()).await?;
    req.extensions_mut().insert(claims);
    if let Some(t) = token {
        req.extensions_mut().insert(rustyfin_auth::RequestToken(t));
    }
    Ok(next.run(req).await)
}

/// Guards the setup wizard routes. Until an owner exists, anyone may use them. After that, only an
/// administrator may.
pub async fn require_setup_or_admin(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if !rustyfin_db::repo_user::owner_exists(&state.db).await? {
        return Ok(next.run(req).await);
    }
    let (claims, _) = request_claims(&state, req.headers(), req.uri()).await?;
    if !claims.is_admin {
        return Err(ApiError::Forbidden(
            "Administrator access is required.".to_string(),
        ));
    }
    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}

#[allow(dead_code)]
pub trait ClaimsExt {
    fn claims(&self) -> Option<&Claims>;
}

impl<T> ClaimsExt for axum::extract::Request<T> {
    fn claims(&self) -> Option<&Claims> {
        self.extensions().get::<Claims>()
    }
}

/// True if `ip` is on a local/private network: loopback, RFC1918, link-local (169.254/16),
/// IPv6 loopback/ULA (fc00::/7)/link-local (fe80::/10). Used to gate who may receive a reset PIN.
pub fn is_local_network(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return true;
            }
            let o = v6.octets();
            (o[0] & 0xfe) == 0xfc || (o[0] == 0xfe && (o[1] & 0xc0) == 0x80)
        }
    }
}

/// Simple in-memory rate limiter using a sliding window per IP.
/// Uses DashMap for lock-striping to reduce contention under high concurrency.
#[derive(Clone)]
pub struct RateLimiter {
    /// Map of IP → list of request timestamps
    state: Arc<DashMap<String, Vec<Instant>>>,
    /// Maximum requests allowed within the window
    max_requests: usize,
    /// Time window duration
    window: std::time::Duration,
}

impl RateLimiter {
    pub fn new(max_requests: usize, window_secs: u64) -> Self {
        Self {
            state: Arc::new(DashMap::new()),
            max_requests,
            window: std::time::Duration::from_secs(window_secs),
        }
    }

    /// Periodically clean up expired entries to prevent memory leaks.
    pub fn start_cleanup(&self) {
        let state = self.state.clone();
        let window = self.window;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let now = Instant::now();
                state.retain(|_, timestamps| {
                    timestamps.retain(|t| now.duration_since(*t) < window);
                    !timestamps.is_empty()
                });
            }
        });
    }

    pub async fn check(&self, key: &str) -> bool {
        // A limit of 0 disables rate limiting
        if self.max_requests == 0 {
            return true;
        }

        let now = Instant::now();
        let mut entry = self.state.entry(key.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.len() >= self.max_requests {
            return false;
        }
        entry.push(now);
        true
    }
}

/// Default login-endpoint budget: requests per window, per resolved client IP.
pub const DEFAULT_LOGIN_LIMIT: usize = 10;
/// Default login-endpoint window, in seconds.
pub const DEFAULT_LOGIN_WINDOW_SECS: u64 = 60;

/// Parse a comma-separated `RUSTYFIN_KNOWN_PROXIES` value into networks. Accepts CIDR blocks
/// (`10.0.0.0/8`) and bare host IPs (`192.168.1.1` → a /32 or /128). Unparseable entries are dropped.
pub fn parse_known_proxies(csv: &str) -> Vec<IpNet> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| {
            s.parse::<IpNet>().ok().or_else(|| {
                s.parse::<IpAddr>().ok().and_then(|ip| {
                    let bits = if ip.is_ipv4() { 32 } else { 128 };
                    IpNet::new(ip, bits).ok()
                })
            })
        })
        .collect()
}

/// Exactly 127.0.0.1 or ::1. NOT all of 127.0.0.0/8 — 127.0.0.2 is treated as a normal client.
fn is_exempt_loopback(ip: IpAddr) -> bool {
    ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST)
}

/// A peer we trust to set `X-Forwarded-For`: exact loopback, or inside a configured known-proxy net.
fn is_trusted_proxy(ip: IpAddr, known: &[IpNet]) -> bool {
    is_exempt_loopback(ip) || known.iter().any(|n| n.contains(&ip))
}

/// Resolve the real client IP. `X-Forwarded-For` is honored ONLY when the connecting peer is a
/// trusted proxy; we then return the right-most forwarded address that is not itself a trusted proxy.
/// An untrusted peer's XFF is ignored entirely, so a spoofed header cannot change or hide the client.
pub fn client_ip(peer: IpAddr, headers: &HeaderMap, known: &[IpNet]) -> IpAddr {
    if !is_trusted_proxy(peer, known) {
        return peer;
    }
    let forwarded: Vec<IpAddr> = headers
        .get("X-Forwarded-For")
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse::<IpAddr>().ok())
                .collect()
        })
        .unwrap_or_default();
    for ip in forwarded.iter().rev() {
        if !is_trusted_proxy(*ip, known) {
            return *ip;
        }
    }
    forwarded.first().copied().unwrap_or(peer)
}

/// Per-client-IP budget for the login endpoints only (AuthenticateByName, ForgotPassword,
/// ForgotPassword/Pin, ResetPassword), mounted on those routes in `routes::build_router`. The
/// client IP is resolved via `client_ip` (X-Forwarded-For is trusted only from a trusted proxy).
/// A request is exempt only when both the peer and the resolved client are exact loopback, and a
/// budget of 0 disables the limit.
pub async fn rate_limit(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let client = client_ip(addr.ip(), req.headers(), &state.known_proxies);

    // Exempt only when both the peer and the resolved client are exact loopback; every other client
    // draws on a per-IP budget. A budget of 0 disables it.
    if !(is_exempt_loopback(addr.ip()) && is_exempt_loopback(client))
        && !state.rate_limiter.check(&client.to_string()).await
    {
        tracing::warn!(ip = %client, "rate limit exceeded");
        return Err(ApiError::RateLimited);
    }

    Ok(next.run(req).await)
}
