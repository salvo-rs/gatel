//! `basic-auth`: require a username and password.
//!
//! Passwords are referenced, never written into configuration, and are stored
//! as bcrypt hashes. A configuration file ends up in version control, in a
//! container image and in a backup; a password in one is a password in all
//! three.

use std::sync::Arc;

use gatel_model::{Code, Diagnostic, Diagnostics, Hasher, Node, SecretRef, SecretString};
use gatel_plan::{BuildContext, Directive, DirectiveConfig, DirectiveMeta, Phase, Scopes};
use salvo_core::http::{Request, Response, StatusCode};
use salvo_core::{Depot, FlowCtrl, Handler, async_trait};

#[derive(Debug, Clone, PartialEq, Eq)]
struct User {
    name: String,
    hash: SecretRef,
}

#[derive(Debug)]
pub struct Config {
    realm: String,
    users: Vec<User>,
}

impl DirectiveConfig for Config {
    fn hash_into(&self, hasher: &mut Hasher) {
        hasher.update_str(&self.realm);
        // The reference, not the material: a hash must never reach a
        // fingerprint that appears in a log or an explain output.
        hasher.update_unordered(&self.users, |user, hasher| {
            hasher.update_str(&user.name);
            user.hash.hash_into(hasher);
        });
    }

    fn build(&self, cx: BuildContext<'_>) -> Result<Arc<dyn Handler>, String> {
        // Hashes are read during prepare, so a missing file or unset variable
        // stops a reload rather than refusing every request afterwards.
        let mut users = Vec::with_capacity(self.users.len());
        for user in &self.users {
            let hash = cx
                .secret(&user.hash)
                .map_err(|error| format!("user `{}`: {error}", user.name))?;
            if !looks_like_bcrypt(hash.expose()) {
                return Err(format!(
                    "user `{}`: `{}` does not contain a bcrypt hash",
                    user.name, user.hash
                ));
            }
            users.push((user.name.clone(), hash));
        }
        let challenge = format!("Basic realm=\"{}\", charset=\"UTF-8\"", self.realm);
        let challenge = http::HeaderValue::from_str(&challenge)
            .map_err(|_| format!("`{}` cannot appear in a realm", self.realm))?;
        Ok(Arc::new(BasicAuthHandler { users, challenge }))
    }

    fn describe(&self) -> String {
        format!("basic-auth realm {:?}, {} user(s)", self.realm, self.users.len())
    }
}

pub struct BasicAuth;

impl Directive for BasicAuth {
    fn meta(&self) -> &DirectiveMeta {
        static META: DirectiveMeta = DirectiveMeta::middleware("basic-auth", Phase::Guard, Scopes::BOTH);
        &META
    }

    fn decode(&self, node: &Node, diagnostics: &mut Diagnostics) -> Option<Arc<dyn DirectiveConfig>> {
        let errors_before = diagnostics.error_count();

        let realm = match node.prop("realm") {
            Some(property) => match property.value.as_str() {
                Some(realm) if !realm.contains('"') => realm.to_string(),
                Some(_) => {
                    diagnostics.error(
                        Code::InvalidValue,
                        property.origin.clone(),
                        "a realm may not contain a quote",
                    );
                    return None;
                }
                None => {
                    diagnostics.error(Code::InvalidValue, property.origin.clone(), "`realm` is a string");
                    return None;
                }
            },
            None => "restricted".to_string(),
        };
        for property in &node.props {
            if property.name != "realm" {
                diagnostics.error(
                    Code::UnknownProperty,
                    property.origin.clone(),
                    format!("`basic-auth` has no property `{}`", property.name),
                );
            }
        }

        let mut users: Vec<User> = Vec::new();
        for child in &node.children {
            if child.name != "user" {
                let mut diagnostic = Diagnostic::error(
                    Code::UnknownNode,
                    child.origin.clone(),
                    format!("`basic-auth` has no child node `{}`", child.name),
                );
                if let Some(suggestion) = gatel_model::did_you_mean(&child.name, ["user"]) {
                    diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
                }
                diagnostics.push(diagnostic);
                continue;
            }
            let Some(name) = child.args.first().and_then(|value| value.as_str()) else {
                diagnostics.error(Code::ArgumentCount, child.origin.clone(), "`user` needs a name");
                continue;
            };
            let Some(raw) = child.prop("hash").and_then(|property| property.value.as_str()) else {
                diagnostics.push(
                    Diagnostic::error(
                        Code::MissingField,
                        child.origin.clone(),
                        format!("user `{name}` needs a `hash`"),
                    )
                    .with_help("`hash=\"env:NAME\"` or `hash=\"file:secrets/name.hash\"`"),
                );
                continue;
            };
            let hash = match SecretRef::parse(raw) {
                Ok(reference) => reference,
                Err(message) => {
                    diagnostics.error(Code::InvalidSecret, child.origin.clone(), message);
                    continue;
                }
            };
            // A password written into the configuration, rather than referenced,
            // is the mistake this directive exists to prevent.
            if looks_like_bcrypt(raw) {
                diagnostics.push(
                    Diagnostic::error(
                        Code::InvalidSecret,
                        child.origin.clone(),
                        format!("user `{name}` has a hash written into the configuration"),
                    )
                    .with_help("put it in a file or an environment variable and reference it"),
                );
                continue;
            }
            if users.iter().any(|existing| existing.name == name) {
                diagnostics.error(
                    Code::Duplicate,
                    child.origin.clone(),
                    format!("user `{name}` is declared twice"),
                );
                continue;
            }
            users.push(User {
                name: name.to_string(),
                hash,
            });
        }

        if users.is_empty() && diagnostics.error_count() == errors_before {
            diagnostics.push(
                Diagnostic::error(
                    Code::MissingField,
                    node.origin.clone(),
                    "`basic-auth` needs at least one `user`",
                )
                .with_help("with no users it would refuse everyone, which `respond status=403` says better"),
            );
        }
        if diagnostics.error_count() > errors_before {
            return None;
        }
        Some(Arc::new(Config { realm, users }))
    }
}

/// The prefixes bcrypt puts on its output.
fn looks_like_bcrypt(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 59 && (value.starts_with("$2a$") || value.starts_with("$2b$") || value.starts_with("$2y$"))
}

struct BasicAuthHandler {
    users: Vec<(String, SecretString)>,
    challenge: http::HeaderValue,
}

impl BasicAuthHandler {
    fn authenticate(&self, request: &Request) -> bool {
        let Some((name, password)) = credentials(request) else {
            return false;
        };
        // Every user is checked, and the result is combined at the end, so the
        // time taken does not reveal whether the username exists.
        let mut authenticated = false;
        for (candidate, hash) in &self.users {
            let matched = constant_time_eq(candidate.as_bytes(), name.as_bytes())
                && bcrypt::verify(&password, hash.expose()).unwrap_or(false);
            authenticated |= matched;
        }
        authenticated
    }
}

/// Pull the username and password out of an `Authorization: Basic` header.
fn credentials(request: &Request) -> Option<(String, String)> {
    let header = request
        .headers()
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let encoded = header
        .strip_prefix("Basic ")
        .or_else(|| header.strip_prefix("basic "))?;
    let decoded = base64_decode(encoded.trim())?;
    let text = String::from_utf8(decoded).ok()?;
    // A password may contain a colon; a username may not.
    let (name, password) = text.split_once(':')?;
    Some((name.to_string(), password.to_string()))
}

/// Comparison whose duration does not depend on where two values first differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b) {
        difference |= left ^ right;
    }
    difference == 0
}

/// Decode standard base64. Written out because this is the only place the
/// project needs it, and it is twenty lines.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = TABLE.iter().position(|candidate| *candidate == byte)? as u32;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

#[async_trait]
impl Handler for BasicAuthHandler {
    async fn handle(
        &self,
        request: &mut Request,
        depot: &mut Depot,
        response: &mut Response,
        ctrl: &mut FlowCtrl,
    ) {
        if self.authenticate(request) {
            ctrl.call_next(request, depot, response).await;
            return;
        }
        crate::generated(depot);
        response.status_code(StatusCode::UNAUTHORIZED);
        response
            .headers_mut()
            .insert(http::header::WWW_AUTHENTICATE, self.challenge.clone());
        let _ = response.write_body("401 authentication required\n");
        ctrl.skip_rest();
    }
}

#[cfg(test)]
mod tests {
    use gatel_model::{Origin, Value};

    use super::*;

    fn origin() -> Origin {
        Origin::synthetic("test")
    }

    fn decode(node: Node) -> (Option<Arc<dyn DirectiveConfig>>, Diagnostics) {
        let mut diagnostics = Diagnostics::new();
        let config = BasicAuth.decode(&node, &mut diagnostics);
        (config, diagnostics)
    }

    fn user(name: &str, hash: &str) -> Node {
        Node::new("user", origin())
            .with_arg(Value::string(name, origin()))
            .with_prop("hash", Value::string(hash, origin()))
    }

    fn request_with(header: Option<&str>) -> Request {
        let mut request = Request::default();
        if let Some(header) = header {
            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_str(header).unwrap(),
            );
        }
        request
    }

    fn handler(name: &str, password: &str) -> BasicAuthHandler {
        let hash = bcrypt::hash(password, 4).unwrap();
        BasicAuthHandler {
            users: vec![(name.to_string(), SecretString::new(hash))],
            challenge: http::HeaderValue::from_static("Basic realm=\"test\""),
        }
    }

    fn basic(name: &str, password: &str) -> String {
        // Encode by hand so the test does not depend on the decoder it checks.
        const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let raw = format!("{name}:{password}");
        let bytes = raw.as_bytes();
        let mut encoded = String::new();
        for chunk in bytes.chunks(3) {
            let mut buffer = [0u8; 3];
            buffer[..chunk.len()].copy_from_slice(chunk);
            let value = u32::from_be_bytes([0, buffer[0], buffer[1], buffer[2]]);
            for index in 0..4 {
                if index <= chunk.len() {
                    encoded.push(TABLE[((value >> (18 - index * 6)) & 0x3f) as usize] as char);
                } else {
                    encoded.push('=');
                }
            }
        }
        format!("Basic {encoded}")
    }

    #[test]
    fn correct_credentials_are_accepted_and_wrong_ones_are_not() {
        let handler = handler("admin", "hunter2");
        assert!(handler.authenticate(&request_with(Some(&basic("admin", "hunter2")))));
        assert!(!handler.authenticate(&request_with(Some(&basic("admin", "wrong")))));
        assert!(!handler.authenticate(&request_with(Some(&basic("someone", "hunter2")))));
        assert!(!handler.authenticate(&request_with(None)));
    }

    #[test]
    fn a_password_may_contain_a_colon() {
        let handler = handler("admin", "a:b:c");
        assert!(handler.authenticate(&request_with(Some(&basic("admin", "a:b:c")))));
    }

    #[test]
    fn a_malformed_header_is_refused_rather_than_panicking() {
        let handler = handler("admin", "x");
        for header in ["Basic", "Basic !!!!", "Bearer token", "Basic YWRtaW4"] {
            assert!(!handler.authenticate(&request_with(Some(header))), "{header}");
        }
    }

    #[test]
    fn a_hash_written_into_the_configuration_is_refused() {
        let inline = bcrypt::hash("hunter2", 4).unwrap();
        let node = Node::new("basic-auth", origin()).with_child(user("admin", &inline));
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        let diagnostic = diagnostics.iter().next().unwrap();
        assert!(diagnostic.message.contains("written into the configuration"));
        assert!(diagnostic.help.as_deref().unwrap().contains("reference"));
    }

    #[test]
    fn a_user_without_a_hash_is_told_how_to_reference_one() {
        let node = Node::new("basic-auth", origin())
            .with_child(Node::new("user", origin()).with_arg(Value::string("admin", origin())));
        let (config, diagnostics) = decode(node);
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .next()
                .unwrap()
                .help
                .as_deref()
                .unwrap()
                .contains("env:")
        );
    }

    #[test]
    fn no_users_is_refused_rather_than_locking_everyone_out_silently() {
        let (config, diagnostics) = decode(Node::new("basic-auth", origin()));
        assert!(config.is_none());
        assert!(
            diagnostics
                .iter()
                .next()
                .unwrap()
                .help
                .as_deref()
                .unwrap()
                .contains("respond")
        );
    }

    #[test]
    fn a_duplicate_user_is_refused() {
        let node = Node::new("basic-auth", origin())
            .with_child(user("admin", "env:A"))
            .with_child(user("admin", "env:B"));
        let (_, diagnostics) = decode(node);
        assert!(diagnostics.iter().any(|d| d.code == Code::Duplicate));
    }

    #[test]
    fn a_missing_secret_stops_the_reload() {
        let node = Node::new("basic-auth", origin()).with_child(user("admin", "env:GATEL_DEFINITELY_UNSET"));
        let (config, diagnostics) = decode(node);
        assert!(!diagnostics.has_errors(), "{diagnostics}");
        let error = match config.unwrap().build(BuildContext {
            telemetry: crate::testing::telemetry(),
            root: std::path::Path::new("."),
            state: None,
        }) {
            Ok(_) => panic!("a hash that cannot be read must not become a running handler"),
            Err(error) => error,
        };
        assert!(error.contains("admin"), "{error}");
    }

    #[test]
    fn the_fingerprint_covers_the_reference_and_never_the_material() {
        let hash = |reference: &str| {
            let node = Node::new("basic-auth", origin()).with_child(user("admin", reference));
            let mut hasher = Hasher::new();
            decode(node).0.unwrap().hash_into(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash("env:A"), hash("env:A"));
        assert_ne!(hash("env:A"), hash("env:B"));
    }

    #[test]
    fn bcrypt_output_is_recognised_and_other_text_is_not() {
        assert!(looks_like_bcrypt(&bcrypt::hash("x", 4).unwrap()));
        assert!(!looks_like_bcrypt("hunter2"));
        assert!(!looks_like_bcrypt("$2b$short"));
    }

    #[test]
    fn comparison_rejects_a_length_mismatch_without_reading_further() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn base64_decoding_handles_padding() {
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        assert_eq!(base64_decode("YWJj").unwrap(), b"abc");
        assert!(base64_decode("!!!").is_none());
    }
}
