use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use salvo::http::StatusCode;
use salvo::{Depot, FlowCtrl, Request, Response, async_trait};
use tracing::debug;

use crate::config::BasicAuthUser;
use crate::crypto::constant_time_eq;
use crate::encoding::base64_decode;

/// Basic authentication middleware.
///
/// Checks the `Authorization: Basic <base64>` header against a list of
/// configured users. Passwords may be stored as:
///   - Plaintext (if the hash does not start with a known prefix)
///   - Bcrypt hashes (starting with `$2b$`, `$2a$`, or `$2y$`)
///   - Argon2 hashes (starting with `$argon2id$` or `$argon2i$`; requires `argon2` feature)
///   - Scrypt hashes (starting with `$scrypt$`; requires `scrypt` feature)
///   - PBKDF2 hashes (starting with `$pbkdf2-sha256$`; requires `pbkdf2` feature)
///
/// Returns 401 Unauthorized with a `WWW-Authenticate` challenge if
/// authentication fails.
///
/// Optionally enforces IP-based brute-force protection: after
/// `brute_force_max` consecutive failures the client IP is blocked for
/// `brute_force_window`.  Returns 429 Too Many Requests when blocked.
pub struct BasicAuthHoop {
    users: Vec<AuthUser>,
    realm: String,
    /// Maximum number of consecutive failures before lockout.
    brute_force_max: u32,
    /// Lockout duration after exceeding `brute_force_max`.
    brute_force_window: Duration,
    /// Failure counters per client IP: (consecutive_failures, last_attempt).
    failure_map: Arc<DashMap<IpAddr, (u32, Instant)>>,
}

enum HashType {
    Plaintext,
    Bcrypt,
    Argon2,
    Scrypt,
    Pbkdf2,
}

struct AuthUser {
    username: String,
    password_hash: String,
    hash_type: HashType,
}

impl BasicAuthHoop {
    pub fn new(
        users: &[BasicAuthUser],
        brute_force_max: Option<u32>,
        brute_force_window: Option<Duration>,
    ) -> Self {
        let users = users
            .iter()
            .map(|u| {
                let hash_type = if u.password_hash.starts_with("$2b$")
                    || u.password_hash.starts_with("$2a$")
                    || u.password_hash.starts_with("$2y$")
                {
                    HashType::Bcrypt
                } else if u.password_hash.starts_with("$argon2id$")
                    || u.password_hash.starts_with("$argon2i$")
                {
                    HashType::Argon2
                } else if u.password_hash.starts_with("$scrypt$") {
                    HashType::Scrypt
                } else if u.password_hash.starts_with("$pbkdf2-sha256$") {
                    HashType::Pbkdf2
                } else {
                    HashType::Plaintext
                };
                AuthUser {
                    username: u.username.clone(),
                    password_hash: u.password_hash.clone(),
                    hash_type,
                }
            })
            .collect();
        Self {
            users,
            realm: "gatel".to_string(),
            brute_force_max: brute_force_max.unwrap_or(5),
            brute_force_window: brute_force_window.unwrap_or(Duration::from_secs(300)),
            failure_map: Arc::new(DashMap::new()),
        }
    }
}

#[async_trait]
impl salvo::Handler for BasicAuthHoop {
    async fn handle(
        &self,
        req: &mut Request,
        depot: &mut Depot,
        res: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        let client_ip = super::client_addr(req).ip();

        // Check if this IP is currently blocked by brute-force protection.
        if let Some(entry) = self.failure_map.get(&client_ip) {
            let (count, last_attempt) = *entry;
            if count >= self.brute_force_max && last_attempt.elapsed() < self.brute_force_window {
                debug!(
                    ip = %client_ip,
                    failures = count,
                    "IP blocked due to brute-force protection, returning 429"
                );
                res.status_code(StatusCode::TOO_MANY_REQUESTS);
                res.body("Too Many Requests");
                ctrl.skip_rest();
                return;
            }
        }

        // Extract and decode the Authorization header.
        let credentials = match extract_basic_credentials(req.headers()) {
            Some(creds) => creds,
            None => {
                debug!("no Authorization header, returning 401");
                record_failure(&self.failure_map, client_ip);
                set_unauthorized(res, &self.realm);
                ctrl.skip_rest();
                return;
            }
        };

        // Verify credentials against configured users.
        let authenticated = self
            .users
            .iter()
            .any(|user| verify_user(user, &credentials.0, &credentials.1));

        if !authenticated {
            debug!(
                username = credentials.0.as_str(),
                "authentication failed, returning 401"
            );
            record_failure(&self.failure_map, client_ip);
            set_unauthorized(res, &self.realm);
            ctrl.skip_rest();
            return;
        }

        // Successful auth — reset failure counter and store user in depot.
        self.failure_map.remove(&client_ip);
        debug!(username = credentials.0.as_str(), "authenticated");
        depot.insert("auth_user", credentials.0.clone());
        ctrl.call_next(req, depot, res).await;
    }
}

/// Increment the consecutive failure counter for a client IP.
fn record_failure(map: &DashMap<IpAddr, (u32, Instant)>, ip: IpAddr) {
    let mut entry = map.entry(ip).or_insert((0, Instant::now()));
    entry.0 += 1;
    entry.1 = Instant::now();
}

/// Extract (username, password) from an `Authorization: Basic <base64>` header.
fn extract_basic_credentials(headers: &http::HeaderMap) -> Option<(String, String)> {
    let header_value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let encoded = header_value.strip_prefix("Basic ")?;

    // Decode base64.
    let decoded_bytes = base64_decode(encoded)?;
    let decoded = String::from_utf8(decoded_bytes).ok()?;

    // Split on first ':' — username:password.
    let (username, password) = decoded.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

/// Set a 401 Unauthorized response with WWW-Authenticate header.
fn set_unauthorized(res: &mut Response, realm: &str) {
    res.status_code(StatusCode::UNAUTHORIZED);
    let _ = res.add_header(
        WWW_AUTHENTICATE,
        format!("Basic realm=\"{realm}\", charset=\"UTF-8\""),
        true,
    );
    res.body("Unauthorized");
}

/// Verify a user's password against their stored hash/plaintext.
fn verify_user(user: &AuthUser, username: &str, password: &str) -> bool {
    if user.username != username {
        return false;
    }

    match user.hash_type {
        HashType::Bcrypt => {
            #[cfg(feature = "bcrypt")]
            {
                bcrypt::verify(password, &user.password_hash).unwrap_or(false)
            }
            #[cfg(not(feature = "bcrypt"))]
            {
                tracing::warn!(
                    "bcrypt password hash found but bcrypt feature is not enabled, rejecting"
                );
                false
            }
        }
        HashType::Argon2 => {
            #[cfg(feature = "argon2")]
            {
                use argon2::Argon2;
                use password_hash::{PasswordHash, PasswordVerifier};
                let parsed = match PasswordHash::new(&user.password_hash) {
                    Ok(h) => h,
                    Err(_) => return false,
                };
                Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .is_ok()
            }
            #[cfg(not(feature = "argon2"))]
            {
                tracing::warn!(
                    "argon2 password hash found but argon2 feature is not enabled, rejecting"
                );
                false
            }
        }
        HashType::Scrypt => {
            #[cfg(feature = "scrypt")]
            {
                use password_hash::{PasswordHash, PasswordVerifier};
                use scrypt::Scrypt;
                let parsed = match PasswordHash::new(&user.password_hash) {
                    Ok(h) => h,
                    Err(_) => return false,
                };
                Scrypt.verify_password(password.as_bytes(), &parsed).is_ok()
            }
            #[cfg(not(feature = "scrypt"))]
            {
                tracing::warn!(
                    "scrypt password hash found but scrypt feature is not enabled, rejecting"
                );
                false
            }
        }
        HashType::Pbkdf2 => {
            #[cfg(feature = "pbkdf2")]
            {
                use password_hash::{PasswordHash, PasswordVerifier};
                use pbkdf2::Pbkdf2;
                let parsed = match PasswordHash::new(&user.password_hash) {
                    Ok(h) => h,
                    Err(_) => return false,
                };
                Pbkdf2.verify_password(password.as_bytes(), &parsed).is_ok()
            }
            #[cfg(not(feature = "pbkdf2"))]
            {
                tracing::warn!(
                    "pbkdf2 password hash found but pbkdf2 feature is not enabled, rejecting"
                );
                false
            }
        }
        HashType::Plaintext => {
            // Plaintext comparison — constant-time-ish via byte comparison.
            constant_time_eq(password.as_bytes(), user.password_hash.as_bytes())
        }
    }
}
