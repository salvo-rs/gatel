//! Adversarial input into every parser a request or a file can reach.
//!
//! This is not a substitute for the coverage-guided fuzzing in `fuzz/`, which
//! goes deeper and runs in CI. It is the part that runs *everywhere, every
//! time*: the same entry points, the same invariants, driven by a seed corpus
//! and a deterministic mutation loop. A fuzzer nobody has run this month still
//! leaves these guarded, and a crash found by the fuzzer can be pasted into
//! `SEEDS` and become a permanent test.
//!
//! Deterministic on purpose. A randomised test that fails once and never again
//! is worse than no test, because it teaches people to re-run the suite.

use std::sync::Arc;

use gatel_model::{Hasher, Snapshot, SourceId, SourceRev, SourceSnapshot};
use gatel_plan::Registry;

/// How many mutations each seed gets. Enough to be worth the second it takes.
const ROUNDS: usize = 400;

/// Text worth starting from: valid, nearly valid, and hostile.
const SEEDS: &[&str] = &[
    "",
    " ",
    "\0",
    "\u{feff}",
    "global { http \":8080\" }",
    "global { http \":8080\" }\nsite \"*\" { route \"/\" { respond \"hi\" } }",
    "site \"*\" { route \"/*\" { file-server root=\"/srv\" } }",
    "upstream \"api\" { endpoint \"127.0.0.1:8080\" }",
    "upstream \"api\" { endpoint \"api.internal:8080\" resolve interval=\"5s\" }",
    "route \"/\" { respond status=999 }",
    "bytes=0-499",
    "bytes=-1",
    "bytes=99999999999999999999-",
    "bytes=18446744073709551615-18446744073709551615",
    "bytes=-18446744073709551615",
    "bytes=1-2,3-4",
    "bytes= - ",
    "*.example.com",
    "..",
    "../../etc/passwd",
    "/a/*/b",
    "env:SECRET",
    "file:../outside.pem",
    "[::1]:8080",
    "api..internal:80",
    // Nesting deep enough to find a recursive parser that grows the stack.
    "a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{a{",
];

/// A generator with no entropy from the machine, so a failure here reproduces
/// on the machine it is reported from.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*: small, and good enough to pick mutation sites.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One mutation of `seed`, chosen by `rng`.
fn mutate(seed: &str, rng: &mut Rng) -> String {
    let mut bytes = seed.as_bytes().to_vec();
    match rng.next() % 6 {
        0 if !bytes.is_empty() => {
            let at = rng.below(bytes.len());
            bytes[at] ^= 1 << (rng.below(8));
        }
        1 => {
            let at = rng.below(bytes.len() + 1);
            bytes.insert(at, b"{}\"\\ \n=*/.:-0"[rng.below(13)]);
        }
        2 if !bytes.is_empty() => {
            bytes.truncate(rng.below(bytes.len()));
        }
        3 => {
            // Splice in another seed, which is how a fuzzer finds the
            // combinations nobody would write.
            let other = SEEDS[rng.below(SEEDS.len())];
            let at = rng.below(bytes.len() + 1);
            let mut spliced = bytes[..at].to_vec();
            spliced.extend_from_slice(other.as_bytes());
            spliced.extend_from_slice(&bytes[at..]);
            bytes = spliced;
        }
        4 if !bytes.is_empty() && bytes.len() < 4096 => {
            // Repetition finds the quadratic loop and the unbounded recursion.
            let repeated = bytes.clone();
            for _ in 0..(1 + rng.below(8)) {
                bytes.extend_from_slice(&repeated);
            }
        }
        _ => bytes.push(rng.below(256) as u8),
    }
    // Everything downstream takes text, so an invalid sequence is the
    // frontend's problem rather than these tests'.
    String::from_utf8_lossy(&bytes).into_owned()
}

fn registry() -> Registry {
    Registry::builder()
        .bundle(gatel_bundle_std::bundle())
        .build()
        .unwrap()
}

/// Every string a mutation produces, so each test covers the same inputs.
fn inputs(salt: u64) -> impl Iterator<Item = String> {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ salt);
    let mut produced = Vec::new();
    for seed in SEEDS {
        produced.push((*seed).to_string());
        for _ in 0..ROUNDS {
            produced.push(mutate(seed, &mut rng));
        }
    }
    produced.into_iter()
}

#[test]
fn the_configuration_frontend_refuses_rather_than_panics() {
    // A panic here is a gateway that dies on a reload, which is the failure
    // the whole transaction design exists to prevent.
    for text in inputs(1) {
        let _ = gatel_kdl::lower(&text, "robustness.kdl");
    }
}

#[test]
fn compiling_anything_that_normalises_never_panics() {
    // Normalising is where names are resolved and references checked;
    // compiling is not allowed to fail, so it has to survive everything that
    // normalising let through.
    let registry = registry();
    for text in inputs(2) {
        let Ok(nodes) = gatel_kdl::lower(&text, "robustness.kdl") else {
            continue;
        };
        let mut hasher = Hasher::new();
        hasher.update_str(&text);
        let snapshot = Snapshot::new().with(SourceSnapshot::new(
            SourceId::file("main"),
            SourceRev(1),
            hasher.finish(),
            nodes,
        ));
        let Ok(spec) = gatel_plan::normalize(&snapshot, &registry) else {
            continue;
        };
        let _ = gatel_plan::compile(Arc::new(spec), &registry);
    }
}

#[test]
fn a_range_never_reads_past_the_end_of_a_file() {
    use gatel_bundle_std::ranges::{Wanted, wanted};
    use salvo_core::Request;

    // Sizes chosen for the arithmetic edges: nothing, one byte, and a length
    // where "the last n bytes" underflows if it is written subtractively.
    for size in [0u64, 1, 2, 1024, u64::MAX] {
        for text in inputs(3) {
            let mut request = Request::default();
            if let Ok(value) = http::HeaderValue::from_str(&text) {
                request.headers_mut().insert(http::header::RANGE, value);
            }
            if let Wanted::Part { start, end } = wanted(&request, size, "\"etag\"") {
                assert!(start <= end, "empty range {start}-{end} from {text:?}");
                assert!(end < size, "range {start}-{end} past {size} from {text:?}");
            }
        }
    }
}

#[test]
fn an_if_range_that_does_not_match_gives_back_the_whole_file() {
    use gatel_bundle_std::ranges::{Wanted, wanted};
    use salvo_core::Request;

    // Otherwise a resumed download splices the second half of a new file onto
    // the first half of an old one, and the result is corrupt without any
    // error anywhere.
    for text in inputs(4) {
        let mut request = Request::default();
        request
            .headers_mut()
            .insert(http::header::RANGE, http::HeaderValue::from_static("bytes=0-9"));
        let Ok(value) = http::HeaderValue::from_str(&text) else {
            continue;
        };
        request.headers_mut().insert(http::header::IF_RANGE, value);
        if text.trim() != "\"etag\"" {
            assert!(
                matches!(wanted(&request, 1024, "\"etag\""), Wanted::Whole),
                "a stale precondition served a part: {text:?}"
            );
        }
    }
}

#[test]
fn the_matchers_survive_being_asked_about_anything() {
    // These are compared against attacker-chosen text on every request.
    for text in inputs(5) {
        if let Ok(host) = gatel_plan::HostMatch::parse(&text) {
            let _ = host.describe();
            let _ = host.local_name();
            let _ = host.specificity();
            let _ = host.matches(&text);
        }
        if let Ok(path) = gatel_plan::PathMatch::parse(&text) {
            let _ = path.describe();
            let _ = path.specificity();
            let _ = path.matches(&text);
        }
        let _ = gatel_plan::Target::parse(&text);
    }
}

#[test]
fn a_secret_reference_never_renders_as_anything_but_a_reference() {
    // The redacted form is what an error message carries. If a mutation ever
    // produced one that printed its material, every diagnostic would be a
    // place a key could appear.
    for text in inputs(6) {
        let Ok(reference) = gatel_model::SecretRef::parse(&text) else {
            continue;
        };
        let shown = reference.display();
        assert!(
            shown.starts_with("env:") || shown.starts_with("file:"),
            "a reference rendered as something else: {shown:?} from {text:?}"
        );
    }
}
