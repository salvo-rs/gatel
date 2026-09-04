//! Answering the CA's question on the listener that is already there.
//!
//! An HTTP-01 challenge is a GET for `/.well-known/acme-challenge/<token>` on
//! port 80. Every ACME library offers to start a listener of its own for it,
//! and taking that offer would mean a second server, bound where the operator
//! did not ask, serving a path nothing in the configuration mentions.
//!
//! So the solver publishes the answer here, and Gatel's own dispatcher serves
//! it from the plaintext listener the configuration already declares — before
//! routing, because the challenge is not a route and must work whatever the
//! site's rules say.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use certon::error::Result as CertonResult;
use certon::solvers::Solver;

/// The path a CA asks for. Fixed by RFC 8555, not by configuration.
pub const PREFIX: &str = "/.well-known/acme-challenge/";

/// Answers the CA is waiting to be asked for.
///
/// Process-wide and outside every generation, like a socket: an order started
/// under one configuration must still be answerable after a reload, or a
/// certificate would be lost to an edit made while it was being issued.
#[derive(Debug, Default)]
pub struct Challenges {
    active: RwLock<HashMap<String, String>>,
}

impl Challenges {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The answer for a token, if one is outstanding.
    pub fn answer(&self, token: &str) -> Option<String> {
        self.active
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(token)
            .cloned()
    }

    /// The answer for a request path, if it is asking for one.
    ///
    /// Returns `None` for anything that is not a challenge, so a caller can
    /// carry on without a second check.
    pub fn answer_for_path(&self, path: &str) -> Option<String> {
        let token = certon::http_handler::extract_challenge_token(path)?;
        // A token is base64url and nothing else. Checking here means a path
        // that is merely shaped like a challenge cannot become a lookup.
        certon::http_handler::is_valid_challenge_token(token).then(|| self.answer(token))?
    }

    pub fn present(&self, token: &str, key_authorization: &str) {
        self.active
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(token.to_string(), key_authorization.to_string());
    }

    pub fn forget(&self, token: &str) {
        self.active
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(token);
    }

    /// How many challenges are outstanding, for a metric or a test.
    pub fn len(&self) -> usize {
        self.active
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The solver Gatel hands to the issuer: no listener, just an answer.
#[derive(Debug)]
pub struct RouteSolver {
    challenges: Arc<Challenges>,
}

impl RouteSolver {
    pub fn new(challenges: Arc<Challenges>) -> Self {
        Self { challenges }
    }
}

#[async_trait::async_trait]
impl Solver for RouteSolver {
    async fn present(&self, domain: &str, token: &str, key_authorization: &str) -> CertonResult<()> {
        self.challenges.present(token, key_authorization);
        tracing::debug!(
            target: "gatel::certificate",
            event = "challenge_presented",
            site = domain,
        );
        Ok(())
    }

    async fn cleanup(&self, domain: &str, token: &str, _key_authorization: &str) -> CertonResult<()> {
        self.challenges.forget(token);
        tracing::debug!(
            target: "gatel::certificate",
            event = "challenge_finished",
            site = domain,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_outstanding_challenge_is_answered_by_its_path() {
        let challenges = Challenges::new();
        challenges.present("tokenAAA", "tokenAAA.thumbprint");
        assert_eq!(
            challenges.answer_for_path(&format!("{PREFIX}tokenAAA")),
            Some("tokenAAA.thumbprint".to_string())
        );
    }

    #[test]
    fn anything_that_is_not_a_challenge_is_not_a_lookup() {
        let challenges = Challenges::new();
        challenges.present("tokenAAA", "answer");
        assert_eq!(challenges.answer_for_path("/tokenAAA"), None);
        assert_eq!(challenges.answer_for_path("/.well-known/other/tokenAAA"), None);
        assert_eq!(challenges.answer_for_path("/"), None);
    }

    #[test]
    fn a_token_that_is_not_a_token_is_refused_before_it_is_looked_up() {
        let challenges = Challenges::new();
        // Traversal, and anything else outside the token alphabet.
        assert_eq!(
            challenges.answer_for_path(&format!("{PREFIX}../../etc/passwd")),
            None
        );
        assert_eq!(challenges.answer_for_path(&format!("{PREFIX}a b")), None);
    }

    #[test]
    fn a_token_nobody_asked_for_has_no_answer() {
        let challenges = Challenges::new();
        assert_eq!(challenges.answer_for_path(&format!("{PREFIX}unknownToken")), None);
    }

    #[tokio::test]
    async fn the_solver_publishes_and_then_forgets() {
        let challenges = Challenges::new();
        let solver = RouteSolver::new(Arc::clone(&challenges));

        solver
            .present("example.com", "tokenAAA", "tokenAAA.thumbprint")
            .await
            .unwrap();
        assert_eq!(challenges.len(), 1);
        assert_eq!(
            challenges.answer("tokenAAA"),
            Some("tokenAAA.thumbprint".to_string())
        );

        solver
            .cleanup("example.com", "tokenAAA", "tokenAAA.thumbprint")
            .await
            .unwrap();
        assert!(
            challenges.is_empty(),
            "an answered challenge must not stay answerable"
        );
    }

    #[tokio::test]
    async fn two_orders_at_once_do_not_overwrite_each_other() {
        // A configuration with several sites orders for all of them.
        let challenges = Challenges::new();
        let solver = RouteSolver::new(Arc::clone(&challenges));
        solver.present("a.example.com", "tokenA", "a").await.unwrap();
        solver.present("b.example.com", "tokenB", "b").await.unwrap();
        assert_eq!(challenges.answer("tokenA"), Some("a".to_string()));
        assert_eq!(challenges.answer("tokenB"), Some("b".to_string()));

        solver.cleanup("a.example.com", "tokenA", "a").await.unwrap();
        assert_eq!(challenges.answer("tokenB"), Some("b".to_string()));
    }
}
