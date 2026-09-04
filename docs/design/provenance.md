# Provenance

Where this code came from, and what may and may not be taken from elsewhere.

This document exists because there is a second project with the same name, and
because the answer to "can we use any of it?" is one that has to be written
down rather than remembered.

## The other `gatel`

`https://github.com/tyreseluo/gatel` is an unrelated project of the same name.
It was examined once, deliberately, to decide whether v2 should absorb anything
from it.

**It carries no licence.** As measured on 2026-09-04:

- no `LICENSE` file in the repository,
- no `license` or `license-file` field in any `Cargo.toml`.

Under copyright law that is not permission withheld by oversight — it is the
default, and the default is **all rights reserved**. Nobody may copy, adapt or
redistribute any part of it.

### The rule

**Its code is never copied, in any amount, in any form.** Not a function, not a
line, not a translated-into-different-words version of one. Only its *design* —
the shape of an idea, which copyright does not cover — may be considered, and
then only by describing the idea in prose and implementing it from that prose.

If any of it is ever actually wanted, the only route is to obtain an explicit
Apache-2.0 grant from its author first, in writing, and record it here. Until
that exists, the answer to every "could we just take…" is no.

### What was measured

The comparison was made to establish that the two codebases are, in fact,
independent — so that this claim is a measurement rather than an assertion.
Significant lines are those over 30 characters after removing blanks, comments,
`use` statements and lone braces.

| Comparison | Result |
|---|---|
| Shared git commits | 0 |
| This project's significant lines appearing there | 154 / 7,943 = 1.9% |
| Its significant lines appearing here | 176 / 23,461 = 0.8% |
| Shared unique lines | 54, every one boilerplate (`fn fmt`, `Poll<Option<Result<Frame>>>`, `tokio::time::timeout`) |
| Shared four-line windows | **0** |
| Shared prose in documentation | 0.2% |

Zero shared four-line windows is the number that matters. Two files can share a
great many single lines by both being idiomatic Rust; sharing no four
consecutive lines anywhere across fifty thousand of them means no passage was
copied.

The 1.9% and 0.8% figures are what independent implementations of the same kind
of program look like, not evidence of contact.

To re-run it: strip both trees as described, compare line sets, then compare the
set of four-line windows. The four-line window comparison is the one to trust.

## What v2 was allowed to take, and did

Ideas, described in prose and then built from scratch:

- that a configuration language should be KDL rather than a bespoke grammar,
- that a reload should be a transaction with a name for each way it can fail,
- that directives should declare a phase rather than rely on file order,
- that a file over about 800 lines has stopped being reviewable.

None of those is expressible as code that could be copied, and each is
independently arrived at elsewhere. The architecture in `ARCHITECTURE.md` — the
five nouns, two hashes and one counter, the three lifetimes, state keyed by
owning resource — is this project's own.

## This project's own licence

Apache-2.0, declared in the workspace `Cargo.toml` and inherited by every crate.
A contribution is offered under the same terms.

## Third-party code

Every dependency is a normal crates.io dependency with a compatible licence, used
as a library rather than vendored. The two worth naming, because they are close
to this project:

- **`certon`** (`salvo-rs/certon`, Apache-2.0) provides the ACME protocol. Same
  organisation, same licence, and already what v1 uses — one implementation
  rather than two. Only its issuer is used; see `FEATURES.md`.
- **`salvo_core`** (Apache-2.0/MIT) supplies `Request`, `Response`, `Handler` and
  `FlowCtrl`. It is the execution vocabulary, not the router: routing decisions
  are this project's, for the reasons in `ARCHITECTURE.md`.

`cargo run -p xtask` enforces which crate may depend on what. It does not check
licences; that is `cargo deny`'s job and is not yet wired up — see `FEATURES.md`.
