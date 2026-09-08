//! A certificate authority small enough to run in a test.
//!
//! It speaks enough of RFC 8555 for a real ACME client to drive an order from
//! nothing to a certificate, and — the part that matters — it **actually
//! fetches the HTTP-01 challenge from the gateway** before it will sign
//! anything. So what this proves is not that the protocol is implemented
//! correctly, which is `certon`'s job, but that Gatel's side of it works: that
//! the answer is published, that the listener serves it at the right path, that
//! nothing configured gets in the way, and that what comes back is a
//! certificate this process can actually present.
//!
//! It does not verify JWS signatures. A signature check would only confirm that
//! the client signs the way this file expects it to, which is a test of the
//! test.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A running authority.
pub struct Authority {
    pub directory_url: String,
    state: Arc<State>,
}

struct State {
    base: String,
    /// Where the gateway is listening, so a challenge can be fetched from it.
    gateway: SocketAddr,
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    root_pem: String,
    orders: Mutex<HashMap<String, Order>>,
    counter: AtomicU64,
    /// Every challenge this authority fetched, and what it got.
    pub fetched: Mutex<Vec<(String, String)>>,
    registration_keys: Mutex<Vec<serde_json::Value>>,
    drop_registration: AtomicBool,
}

#[derive(Clone)]
struct Order {
    domain: String,
    token: String,
    /// `pending` until the challenge was fetched and matched.
    authorized: bool,
    certificate: Option<String>,
}

impl Authority {
    /// Start one. It signs with an authority of its own, so a certificate it
    /// issues is a real certificate — just one nothing else trusts.
    pub async fn start(gateway: SocketAddr) -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", address.port());

        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "gatel test authority");
        let root = params.self_signed(&key).unwrap();
        let root_pem = root.pem();

        let state = Arc::new(State {
            base: base.clone(),
            gateway,
            issuer: rcgen::Issuer::new(params, key),
            root_pem,
            orders: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            fetched: Mutex::new(Vec::new()),
            registration_keys: Mutex::new(Vec::new()),
            drop_registration: AtomicBool::new(false),
        });

        let serving = Arc::clone(&state);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = Arc::clone(&serving);
                tokio::spawn(async move { serve(stream, state).await });
            }
        });

        Arc::new(Self {
            directory_url: format!("{base}/directory"),
            state,
        })
    }

    /// What the authority asked the gateway for, and what it received.
    pub fn fetched(&self) -> Vec<(String, String)> {
        self.state
            .fetched
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn lose_registration_responses(&self) {
        self.state.drop_registration.store(true, Ordering::SeqCst);
    }

    pub fn registration_keys(&self) -> Vec<serde_json::Value> {
        self.state.registration_keys.lock().unwrap().clone()
    }
}

async fn serve(mut stream: TcpStream, state: Arc<State>) {
    loop {
        let Some((method, path, body)) = read_request(&mut stream).await else {
            return;
        };
        if path == "/new-account" {
            let outer: serde_json::Value = serde_json::from_str(&body).unwrap();
            let protected: serde_json::Value =
                serde_json::from_slice(&base64url(outer["protected"].as_str().unwrap())).unwrap();
            state
                .registration_keys
                .lock()
                .unwrap()
                .push(protected["jwk"].clone());
            if state.drop_registration.load(Ordering::SeqCst) {
                return; // The request arrived; the client receives no response.
            }
        }
        let response = route(&method, &path, &body, &state).await;
        if stream.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// One request, headers and declared body.
async fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let head_end = find(&buffer, b"\r\n\r\n");
        if let Some(end) = head_end {
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            let length = content_length(&head);
            if buffer.len() >= end + 4 + length {
                let body = String::from_utf8_lossy(&buffer[end + 4..end + 4 + length]).to_string();
                let mut lines = head.lines();
                let request = lines.next()?;
                let mut parts = request.split(' ');
                let method = parts.next()?.to_string();
                let target = parts.next()?.to_string();
                return Some((method, target, body));
            }
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn content_length(head: &str) -> usize {
    head.lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.split(':').nth(1))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

async fn route(method: &str, path: &str, body: &str, state: &Arc<State>) -> String {
    let base = &state.base;
    match (method, path) {
        (_, "/directory") => json(
            200,
            &format!(
                r#"{{"newNonce":"{base}/new-nonce","newAccount":"{base}/new-account",
                     "newOrder":"{base}/new-order","revokeCert":"{base}/revoke",
                     "keyChange":"{base}/key-change",
                     "meta":{{"termsOfService":"{base}/terms"}}}}"#
            ),
            None,
            state,
        ),
        ("HEAD", "/new-nonce") | ("GET", "/new-nonce") => json(200, "", None, state),
        (_, "/new-account") => json(
            201,
            r#"{"status":"valid","contact":[],"termsOfServiceAgreed":true}"#,
            Some(format!("{base}/account/1")),
            state,
        ),
        (_, "/new-order") => new_order(body, state),
        (_, path) if path.starts_with("/authz/") => authorization(path, state),
        (_, path) if path.starts_with("/challenge/") => challenge(path, state).await,
        (_, path) if path.starts_with("/finalize/") => finalize(path, body, state),
        (_, path) if path.starts_with("/certificate/") => certificate(path, state),
        _ => json(
            404,
            r#"{"type":"urn:ietf:params:acme:error:malformed"}"#,
            None,
            state,
        ),
    }
}

fn new_order(body: &str, state: &Arc<State>) -> String {
    let payload = payload_of(body);
    let domain = payload["identifiers"][0]["value"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let id = state.counter.fetch_add(1, Ordering::Relaxed).to_string();
    let token = format!("tokenGatel{id}");
    state
        .orders
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id.clone(),
            Order {
                domain: domain.clone(),
                token,
                authorized: false,
                certificate: None,
            },
        );

    let base = &state.base;
    json(
        201,
        &format!(
            r#"{{"status":"pending","identifiers":[{{"type":"dns","value":"{domain}"}}],
                 "authorizations":["{base}/authz/{id}"],"finalize":"{base}/finalize/{id}"}}"#
        ),
        Some(format!("{base}/order/{id}")),
        state,
    )
}

fn authorization(path: &str, state: &Arc<State>) -> String {
    let id = path.trim_start_matches("/authz/").to_string();
    let Some(order) = order_of(&id, state) else {
        return json(404, "{}", None, state);
    };
    let base = &state.base;
    let status = if order.authorized { "valid" } else { "pending" };
    json(
        200,
        &format!(
            r#"{{"status":"{status}","identifier":{{"type":"dns","value":"{}"}},
                 "challenges":[{{"type":"http-01","url":"{base}/challenge/{id}",
                 "token":"{}","status":"{status}"}}]}}"#,
            order.domain, order.token
        ),
        None,
        state,
    )
}

/// Respond to a challenge — and this is the part that matters: the authority
/// goes and asks the gateway for the answer, exactly as a real one would.
async fn challenge(path: &str, state: &Arc<State>) -> String {
    let id = path.trim_start_matches("/challenge/").to_string();
    let Some(order) = order_of(&id, state) else {
        return json(404, "{}", None, state);
    };

    let answer = fetch_challenge(state.gateway, &order.domain, &order.token).await;
    state
        .fetched
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((order.token.clone(), answer.clone().unwrap_or_default()));

    // A key authorization is `<token>.<account key thumbprint>`. The thumbprint
    // is the client's business; that it starts with the token this authority
    // issued is what proves the right process answered.
    let valid = answer
        .as_deref()
        .is_some_and(|answer| answer.starts_with(&order.token) && answer.len() > order.token.len() + 1);
    if valid {
        let mut orders = state
            .orders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(order) = orders.get_mut(&id) {
            order.authorized = true;
        }
    }

    let base = &state.base;
    let status = if valid { "valid" } else { "invalid" };
    json(
        200,
        &format!(
            r#"{{"type":"http-01","url":"{base}/challenge/{id}","token":"{}","status":"{status}"}}"#,
            order.token
        ),
        None,
        state,
    )
}

async fn fetch_challenge(gateway: SocketAddr, domain: &str, token: &str) -> Option<String> {
    let mut stream = TcpStream::connect(gateway).await.ok()?;
    let request = format!(
        "GET /.well-known/acme-challenge/{token} HTTP/1.1\r\nHost: {domain}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.ok()?;
    let text = String::from_utf8_lossy(&response).to_string();
    if !text.starts_with("HTTP/1.1 200") {
        return None;
    }
    text.split_once("\r\n\r\n").map(|(_, body)| body.to_string())
}

fn finalize(path: &str, body: &str, state: &Arc<State>) -> String {
    let id = path.trim_start_matches("/finalize/").to_string();
    let Some(order) = order_of(&id, state) else {
        return json(404, "{}", None, state);
    };
    if !order.authorized {
        return json(
            403,
            r#"{"type":"urn:ietf:params:acme:error:unauthorized","detail":"the challenge was not answered"}"#,
            None,
            state,
        );
    }

    // Sign the key in the request, not one of our own: the gateway holds the
    // private half, and a certificate whose key it does not have would fail
    // rustls before it ever reached a handshake.
    let payload = payload_of(body);
    let csr_der = base64url(payload["csr"].as_str().unwrap_or_default());
    let request = rcgen::CertificateSigningRequestParams::from_der(&csr_der.into()).expect("a valid CSR");
    let signed = request.signed_by(&state.issuer).expect("signing works");

    let chain = format!("{}{}", signed.pem(), state.root_pem);
    let base = &state.base;
    {
        let mut orders = state
            .orders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(order) = orders.get_mut(&id) {
            order.certificate = Some(chain);
        }
    }
    json(
        200,
        &format!(
            r#"{{"status":"valid","identifiers":[{{"type":"dns","value":"{}"}}],
             "authorizations":["{base}/authz/{id}"],"finalize":"{base}/finalize/{id}",
             "certificate":"{base}/certificate/{id}"}}"#,
            order.domain
        ),
        None,
        state,
    )
}

fn certificate(path: &str, state: &Arc<State>) -> String {
    let id = path.trim_start_matches("/certificate/").to_string();
    match order_of(&id, state).and_then(|order| order.certificate) {
        Some(chain) => {
            let nonce = nonce(state);
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/pem-certificate-chain\r\n\
                 Replay-Nonce: {nonce}\r\nContent-Length: {}\r\n\r\n{chain}",
                chain.len()
            )
        }
        None => json(404, "{}", None, state),
    }
}

fn order_of(id: &str, state: &Arc<State>) -> Option<Order> {
    state
        .orders
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(id)
        .cloned()
}

/// The JWS payload, which is where a request's actual content is.
fn payload_of(body: &str) -> serde_json::Value {
    let outer: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let payload = outer["payload"].as_str().unwrap_or_default();
    if payload.is_empty() {
        // POST-as-GET, which carries nothing.
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&base64url(payload)).unwrap_or(serde_json::Value::Null)
}

fn base64url(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .unwrap_or_default()
}

/// Every response carries a fresh nonce, because every request needs one.
fn nonce(state: &Arc<State>) -> String {
    format!("nonce{}", state.counter.fetch_add(1, Ordering::Relaxed))
}

fn json(status: u16, body: &str, location: Option<String>, state: &Arc<State>) -> String {
    let body: String = body.split('\n').map(str::trim).collect();
    let reason = match status {
        200 => "OK",
        201 => "Created",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let location = location
        .map(|url| format!("Location: {url}\r\n"))
        .unwrap_or_default();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Replay-Nonce: {}\r\n{location}Content-Length: {}\r\n\r\n{body}",
        nonce(state),
        body.len()
    )
}
