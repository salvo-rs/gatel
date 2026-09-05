# Fuzzing

Coverage-guided fuzzing of everything that takes input somebody else chose:
the configuration frontend, the whole compile path, the `Range` parser, and
the matchers a request is compared against.

```
cargo +nightly fuzz run kdl_frontend
cargo +nightly fuzz run configuration
cargo +nightly fuzz run range_header
cargo +nightly fuzz run matchers
```

Its own workspace, because the targets need nightly and a sanitizer runtime,
and neither belongs in an ordinary build of the gateway. On Windows the
sanitizer runtime is usually missing and the targets will not link; they build
and run on Linux, which is where CI runs them.

CI runs each target for a few minutes on every push. That finds regressions,
not deep bugs — a long run against a corpus that grows over time is what finds
those, and there is no substitute for leaving it going.

## When it finds something

`fuzz/artifacts/<target>/` holds the input. Two things happen with it:

1. Fix the bug.
2. Paste the input into `SEEDS` in `crates/gatel-http/tests/robustness.rs`,
   so it is checked on every `cargo test` by everyone, for ever, on a stable
   toolchain and with no fuzzer installed.

The second matters as much as the first. The corpus here is a starting point
for a search; that file is the record of what has already gone wrong.
