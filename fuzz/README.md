# Fuzzing

Coverage-guided fuzzing of everything that takes input somebody else chose:
the configuration frontend, the whole compile path, the `Range` parser, and
the matchers a request is compared against, CGI protocol boundaries, and
password hash policy parsing.

```
cargo +nightly fuzz run kdl_frontend
cargo +nightly fuzz run configuration
cargo +nightly fuzz run range_header
cargo +nightly fuzz run matchers
cargo +nightly fuzz run cgi_protocol
cargo +nightly fuzz run password_policy
cargo +nightly fuzz run request_decode
```

Its own workspace, because the targets need nightly and a sanitizer runtime,
and neither belongs in an ordinary build of the gateway. On Windows the
sanitizer runtime may be missing and the targets will not link. Disabling the
sanitizer is not a guaranteed workaround: the MSVC linker may also lack coverage
symbols. Linux CI is configured to build and run each target with the default
sanitizer; a configured job is not evidence that a particular revision passed.

CI runs each target for a few minutes on every push. That finds regressions,
not deep bugs — a long run against a corpus that grows over time is what finds
those, and there is no substitute for leaving it going.

## When it finds something

`request_decode` compiles the exact production decoder module into a shared
test harness, without adding a public fuzz API to the plugin. The first input
byte selects a codec, raw versus freshly compressed input, optional mutation
and fragmentation width. Raw candidates are capped at 4096 bytes; generated
plaintext at 256 bytes; decoded output at 16384 bytes. It checks whole/frame-
fragmented outcome equality, output bounds and exact recovery of unmodified
generated streams. The same harness executes 320 deterministic cases under
`cargo test -p gatel-decode`. It does not test HTTP routing, admission or reload;
those have separate handler and integration tests.

Local smoke evidence (2026-09-08): Linux nightly, cargo-fuzz 0.13.2, default
sanitizer, `request_decode -- -max_total_time=30 -max_len=4097 -timeout=5`:
25,757 executions in 31 seconds, exit 0, no crash/assertion failure. Started with
an empty corpus; final corpus 826 entries / 4789 bytes, coverage 5383, features
12180, fuzzer length limit reached only 11 bytes. Thus this is **short-input
smoke evidence**, not coverage up to the configured 4097-byte ceiling. Local
corpus and executable remain in the stopped task container
`gatel-decode-fuzz-20260908` (`/src/fuzz/corpus/request_decode` and `/fuzz-target`).
Promote any future failure to the stable harness, and retain minimized corpus
cases rather than committing every generated mutation.

`password_policy` enables optional Argon2id and exercises untrusted encoded
credentials without executing password verification or generation. It checks
deterministic acceptance, the encoded length bound, redacted Debug output, and
static error messages. Its shared harness also runs all seed truncations and
single-byte mutations under stable `cargo test -p gatel-password --all-features`.
That deterministic check is not coverage-guided fuzzing. Password regressions
belong in `crates/gatel-password/tests/` and its `support/fuzz_cases.rs` seeds.

Local smoke evidence (2026-09-07): Linux nightly 1.100.0 (2026-09-06),
cargo-fuzz 0.13.2, default sanitizer, `password_policy -- -max_total_time=30
-max_len=4096`: 1,075,280 executions in 31 seconds, exit 0, no crash or
assertion failure. This started with an empty corpus and does not establish
long-run coverage, production memory usage, or password-work concurrency safety.

`cgi_protocol` checks CGI header fragmentation invariance, FastCGI record/state
transitions, and bounded environment encoding into FastCGI and SCGI, checked by
independent test-side decoders. Long-value vectors cover 127/128-byte length
encoding and 64 KiB record boundaries. Its shared
invariant harness also runs under stable `cargo test -p gatel-cgi-protocol` with
fixed vectors and 1,024 deterministic generated inputs. That smoke check is not
a substitute for coverage-guided fuzzing or sanitizers. CGI regressions belong
in `crates/gatel-cgi-protocol/tests/`, alongside this harness.

`snapshot_codec` exercises raw frames and checksum-correct arbitrary bodies,
asserting byte-for-byte canonical re-encoding after every successful decode.
It covers structural parsing, not checkpoint filesystem durability or source
authorization. Regressions belong in `gatel-model/src/snapshot_codec_tests.rs`.

Snapshot smoke evidence (2026-09-09): Linux nightly, `snapshot_codec --
-max_total_time=30 -max_len=4096 -timeout=5`, 1,696,483 executions in 31 seconds,
exit 0, no crash/assertion failure. Image `7175a44da908f6c43946262479e54d9fdc4df0eb2fd4a549488187c9ce9e227a`.
The temporary run container/corpus was not retained. This 4 KiB-input smoke is
not long-run coverage or a test of near-16 MiB checkpoint memory behavior.

`fuzz/artifacts/<target>/` holds the input. Two things happen with it:

1. Fix the bug.
2. Paste the input into `SEEDS` in `crates/gatel-http/tests/robustness.rs`,
   so it is checked on every `cargo test` by everyone, for ever, on a stable
   toolchain and with no fuzzer installed.

The second matters as much as the first. The corpus here is a starting point
for a search; that file is the record of what has already gone wrong.
