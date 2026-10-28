# Architecture

Connections pin a generation by default. The opt-in `global { reload-drain "30s" }`
policy retires older connections after successful Server reconciliation: HTTP
stops accepting requests/streams gracefully, and the deadline also bounds
unfinished handshakes and upgrade tunnels. Registration reads the current
generation under the retirement lock so publication/reconciliation cannot miss
an older reader. Weak per-generation entries do not retain runtimes; each
connection owns its signal and capacity permit. Once announced, a deadline is
not cancelled or extended by another reload. See CLI.md and reload_drain.rs.

The optional loopback control-plane HTTP listener/handler and its security /
transaction boundaries are documented in [ADMIN.md](ADMIN.md).

Plugin contracts and extension sequencing are documented in [PLUGINS.md](PLUGINS.md).
Pure metadata lives in `gatel-plugin-api`; the version-coupled handler bridge
lives in `gatel-plugin-runtime`, independently of `gatel-plan`. Optional packages live under `plugins/` and the
host has one feature-gated assembly point shared by all CLI commands.
Middleware composition validates ordering requirements and conflicts before
prepare. Upstream grants are enforced per directive, with single-resource handles.
Background factories activate only after commit and their tasks are cancelled
when the owning generation is released. This is not yet a complete body-effect,
worker supervision or external-process lifecycle system.

HTTP upgrade does not detach ownership: a once-only connection handoff runs
the tunnel after Hyper completes its HTTP phase. The original connection task
keeps its generation and capacity permit until the tunnel ends. Listener drain
owns all such tasks and cancels/joins them at the configured grace deadline.

## Request path identity

Before selecting a route or its guards, the dispatcher decodes the URL path
exactly once and normalizes repeated separators and dot components. Backslash
is treated as a separator on every platform. Route declarations use the same
identity, and overlap checks run on those normalized patterns. Only a literal
final `*` in the declaration selects prefix syntax; an encoded asterisk remains
a literal character. Malformed percent escaping, invalid UTF-8 and decoded
control characters produce a 400 response before running route handlers.

The original escaped URI and query remain unchanged for the terminal, cache key
and redirect construction. In particular, `%25` does not cause a second decode.
File containment and public filename checks continue to apply after routing.
Filesystem links and aliases are still part of the trusted operator-provided
file tree; a URL guard does not establish a separate filesystem sandbox.

On Windows, file requests must additionally use the exact canonical relative
filename spelling. This rejects case aliases, short names and internal
symlink/junction aliases, including indexes, precompressed sidecars and browse
entries. Configure index/fallback filenames with their actual on-disk case.
The configured root itself is canonicalized first and may still name a link.
Unix public symlinks retain their existing behavior; hard-link aliases and
concurrent filesystem replacement are outside this spelling check.

## The one path

`GenerationStore::source_writer` can mint a `SourceWriter` only for an explicitly
registered identity in an unsealed store. Even unrestricted legacy stores do not
implicitly mint capabilities. The handle binds its source, immutable Registry
and host-supplied preparation function at issuance; callers provide only a whole
Version, content identity and lowered nodes. They cannot select another source,
replace validation/prepare, or obtain the store/other source trees through the
handle. Normal namespace/ownership validation still applies. Latest source
revision is assigned internally, with rejected intent and empty tombstones
retained; concurrent same-version writers conflict, never automatically retry.

This is a synchronous plan-layer capability for trusted frontend code, not a
network credential, sandbox, body limit, durable writer or App activation API.
The host must compute a fingerprint covering every semantic input, never trust
a client-supplied fingerprint, authorize issuance and reads of Version metadata,
filter outgoing diagnostics, and arrange bounded execution/resource refresh and
activation. A caller can still submit invalid intent for its granted source.
The bound preparation function is trusted host code; it must not be supplied by
the grantee. A Weak store reference prevents this handle itself from retaining
the store, but host callbacks/registry implementations must separately avoid
strong cycles. Dropped/sealed stores refuse writes; sealing during an already
admitted transaction can instead return the normal Stale outcome. No CLI or HTTP
adapter currently issues this handle to remote users.

`SourceSnapshot::observe` now rejects reused content fingerprints with different
lowered trees as `ObservationError::ContentMismatch`. This also protects
Snapshot::observe_if and SourceWriter before they retain intent or prepare a
runtime. Comparison includes node/argument/property order and all origins; float
identity uses exact bits (including NaN payloads and signed zero), not numerical
PartialEq. The tree comparison walks iteratively and runs outside the generation
write lock. This adds comparison work for repeated observations, not a new input
size bound. Frontends still compute the complete content identity; manually
constructed SourceSnapshot::new and full-union low-level submissions retain
their trusted-caller contract and are not made safe for arbitrary remote input.

Within gatel-plan, `generation_observe.rs` owns source admission and intent
observation: seal/precondition checks, registered-source and capacity policy,
equal-revision content validation, and whole-union head merging. It uses the
same GenerationStore write lock and then hands a CandidateBase to the existing
prepare/publication path in `generation.rs`. This is an internal module boundary,
not an alternate frontend transaction API or an additional synchronization layer.

`App::source_writer` now issues an `AppSourceWriter` for explicitly registered
sources while the store is open. This host handle binds a Weak App and source;
its async `reload_if` accepts only Version, complete content identity and nodes.
Observation/revision assignment uses the retained source inside bounded
configuration work, including exact-tree validation on reused content identity.
Resource refresh, compilation, publication and activation stay on the existing
App path. It does not invoke a second CertManager or provide a custom prepare
callback to its holder. Accepted tasks remain owned and capacity-counted after
the waiting caller or handle is dropped; a dormant handle does not keep App alive.
Already sealed/dropped owners return Unavailable; races with closure or capacity
limits can return Stale, as with ordinary reload. Conflict/observation errors are
typed and do not activate a runtime. Input bytes/tree size, authentication,
diagnostic disclosure and persistence remain the frontend's responsibility.
`AppSourceWriter::reload_kdl_if` accepts inline KDL text instead of a caller's
fingerprint/tree. The host derives a domain-separated identity from exact text
and a stable bound-source origin label. Parsing occurs in the same bounded
worker, after Version validation. Inline text is limited to 1 MiB and 10,000
nodes; existing KDL pre-parse recursion guards apply. No file imports are
expanded, and import nodes are explicitly rejected at any depth. Text limits
are not a request-body allocator or a hard process memory/CPU deadline: the
String already exists at entry, parsing allocates before the node count check,
and retained intent is composed by the normal transaction path. A future HTTP
adapter must impose streaming byte limits before allocation and authenticate
before submission. Parse/limit failures use conditional observation rejection:
they affect configuration health but do not invent a revision or replace intent.
Returning to the same valid text can recover health through NoOp. Syntax error
diagnostics still require filtering before exposure to untrusted clients.

The optional CLI loopback Admin adapter now privately owns an AppSourceWriter;
authenticated HTTP clients receive only the constrained text protocol, never
the Rust handle or App. Explicit `run --admin-listen ... --admin-token-file ...`
registers Admin alongside file sources; the default run path does not. The host
loads the private token and reserves the socket before configuration startup,
then starts the management accept loop after initial runtime activation. On
signal it stops management requests before draining App; unexpected management
task termination also initiates shutdown with a failing exit status. No durable
intent or token hot rotation is implied. Source-registration
metadata is immutable and is not itself a write permit.

`App::reload_source_if(expected, source)` is a trusted binary-host integration
point for submitting one already-observed SourceSnapshot. It captures retained
intent and checks the whole Version inside the bounded configuration worker,
replaces only that contribution, and delegates to the ordinary resource-refresh,
prepare and publication path, which checks Version again. It shares reload task
admission, owned activation and shutdown drain with full-union reloads; queueing
does not grant a stale write permission. This is not a SourceWriter capability:
the caller still chooses source/revision and must be authorized by the host.
It neither reads file inputs nor overwrites frontend observation caches. No
network endpoint or durable acknowledgement is implied.

The binary host exposes `App::attempt_snapshot_if(expected, snapshot)` internally
for a trusted coordinator's complete source union. It checks the whole version
before external-resource reads, then uses the same conditional refresh/attempt,
Resources, plugin retirement, ACME factory and runtime publication as file reload.
File reload atomically captures the current Version and latest intent union before
source observation, replaces only the observed file contributions in that union,
and delegates here. Non-file intent, including rejected input and tombstones,
survives subsequent file edits. A concurrent update during file observation is
still a conflict, not an excuse to recapture a newer version and overwrite it.
File-source identities remain owned by their frontend; trusted runtime writers
must not impersonate them, and the CLI does not automatically register Admin.
Conflict returns the actual version without preparing the supplied candidate;
the store checks again after resource observation to close the precheck race.
The caller still invokes `activate()` after successful publication and owns
source authorization, intent serialization, audit and durable recovery. This
entry point neither updates the file observation cache nor replaces immutable
CLI input, and it is not an exposed Admin HTTP API. A coordinator can now capture
the store's latest admitted union instead of reconstructing it from the active
plan or resubmitting an obsolete frontend snapshot.

`GenerationStore::observed_snapshot()` atomically captures the whole Version and
latest admitted source trees. This is desired intent, not the last known good
runtime: rejected configuration and empty-source tombstones remain readable.
Existing admission semantics are unchanged: a stale incomplete union can advance
newer source observations without publishing a runtime; its retained trees match
that merged head. An unauthorized source, capacity violation or equal-revision
fingerprint mismatch is rejected before replacing any retained tree. Seal and
conditional conflicts do not admit new intent.

`attempt_source_if` composes one complete source contribution with this observed
union, preserving other sources even if their latest intent is invalid. It checks
the supplied whole Version and rechecks through the normal transaction after
composition, so it is not last-writer-wins or an automatic conflict retry. Empty
nodes remove contributions while retaining the source revision. This low-level
plan API does not do host external-resource refresh or worker activation; the
host's full-union reload path remains responsible for those operations.

Retained source trees are Arc-owned. A brief read of the monotonic head selects
only previously unseen revisions for retention copying; unchanged/older trees
are not copied for retention on polling reloads. Selected trees are cloned before
acquiring the transaction write lock, whose full admission checks still decide
whether to retain them. A concurrent writer can make this copy redundant but
cannot make an already observed revision new again. Outgoing trees are released
after the lock. Capturing a union
copies Arc references under the read lock and clones trees after releasing it.
This adds retained configuration memory and copy cost; no performance improvement
or payload byte bound is claimed. Source-count limits still apply. Frontends must
bound input sizes and authorize both reads and writes: trees may contain secrets
and must not become public status/debug output. Intent remains process-local;
durable journaling, recovery, and coordination of frontend observation caches are
not implemented by retaining it here.

`App::shutdown()` first calls `GenerationStore::seal()`, before waiting on async
activation or drain. Sealing is irreversible within that store lifetime and
uses the same write lock as publication. New observations (including NoOp and
refresh) and late success/failure publication from preparations already underway
return `Outcome::Stale`; the current generation and previously observed intent
remain available for inspection and existing readers. Sealing is not a new
configuration revision and does not change its Version/ETag; `is_sealed()` is
a separate host lifecycle fact, not a traffic-readiness signal. It does not
interrupt or join synchronous preparation: an admitted preparation may still
finish acquiring resources and must release its rejected candidate normally.
The App host now tracks each `attempt_snapshot_if` call with an RAII admission
covering resource observation, preparation, publication and synchronous cleanup.
Shutdown closes admission before sealing, serializes against activation, marks
readiness draining, releases the lifecycle mutex, then waits for admitted calls
to finish (including queued activations which must observe closed). It reacquires
the lifecycle mutex for resource teardown. No admission drain holds that mutex
before stopping listeners and background workers. Cancelling that wait does not
reopen admission or lose the count; retrying shutdown resumes draining. Waiters
register before inspecting the count, preventing a lost final-release wakeup.
This guarantee covers the App entry point, not direct low-level store writes or
untracked plugin work. File parsing before submission is not in this drain.
It does not forcibly interrupt synchronous code: a permanently blocked prepare
can block shutdown indefinitely; host/process-level termination policy remains
separate. Sealing the store alone still does not wait for preparation.
After admitted work and the Server's accept/retirement tasks have drained, App
clears its BindTable. Keeping a stopped App or committed generation for inspection
therefore does not by itself keep its listening sockets alive. This includes
adopted sockets never activated. External owners of cloned listener handles, or
code bypassing host admission to call BindTable directly, remain responsible for
their own handles; shutdown cannot revoke an external Arc or OS descriptor.

Startup and watcher reload invoke `reload()`, an owned async task that tracks both
preparation/publication and subsequent activation through one outer admission.
An async execution mutex, owned by that task, serializes high-level reload work
through the end of activation. The next reload cannot begin its work while the
preceding one is waiting to activate, even if either caller has disappeared.
Queued work checks the sealed state after acquiring this mutex and is refused
without executing its closure after shutdown. Future checkpoint writes must be
inside this ownership interval; they are not integrated yet. App's writable
GenerationStore and synchronous attempt/activation stages are private to its
transaction module. Admin, watcher and CLI read status copies, not that store;
their configuration writes use the owned high-level reload path. Internal host
code/tests can still call those stages, so this is a module boundary rather than
a sandbox against trusted host code. No FIFO ordering of request arrival or
hard deadline for activation is promised.
A per-App semaphore admits
one configuration worker into Tokio's blocking pool. Both its owned admission
and semaphore permit live inside the worker, so cancelling its awaiting task
cannot hide queued/running work from shutdown or free capacity prematurely.
Shutdown closes the semaphore, wakes queued callers without reading their files,
and drains admitted work normally. A worker panic propagates rather than being
misreported as a preparation failure with guaranteed rollback. The synchronous
entry is now an internal stage (also exercised directly by lifecycle tests);
it bypasses this async worker capacity limit but still participates in submission
drain. Embedders needing the standalone plan-layer GenerationStore can use that
separate API, not retrieve the running App's writable store. The shared generic
`configuration_work` entry also runs directory-subscription discovery and parent
path canonicalization in the blocking pool, with the same capacity and drain
ownership. Closure is represented by None rather than a synthetic empty directory
set, so refused discovery does not silently remove active subscriptions. Actual
watch/unwatch OS calls remain on the watcher coordinator; not all watcher I/O is
off the reactor. Cancelling the caller of `reload()` does not abandon a committed
configuration's activation; shutdown can still close activation and drain this
owned task. Internal `attempt_async`/`attempt_snapshot_if` stages still require
their enclosing reload job to perform activation. This is in-process task ownership, not durable jobs
or a durable network request queue; the optional Admin transport is described above.
`reload()` admits at most eight owned tasks per App, including queued preparation
and pending activation, using a separate non-waiting semaphore before task spawn.
The permit lives in the task, not its caller; cancellation cannot manufacture
extra capacity. Overflow returns Stale with a capacity reason before file reads
or source observation. Shutdown closes this admission permanently. This fixed
host bound does not limit payload bytes or arbitrary lower-level embedding calls;
future network adapters still need request/body limits and an overload response.

`reload_snapshot_if(expected, snapshot)` provides the same owned, bounded
transaction for a complete authorized source union. It shares the eight-task
admission and single blocking worker with file reloads; it does not create a
second queue or perform synchronous preparation on the async reactor. The whole
Version is checked inside the worker, so queued requests cannot rely on an
obsolete admission-time check. Conflicts remain typed `Conflict` results;
capacity/closure refusals remain `Ok(Stale)`. Started and Reloaded outcomes
trigger host activation/reconciliation. A private atomic completed-activation
generation advances only at the end of activation, after reconciliation and
retirement/pruning. NoOp skips lifecycle work when that generation is already
active; otherwise it still activates, including publication by a lower-level
caller that did not activate. Publication alone never advances this marker.
The authoritative transaction failure source now handles NoOp health recovery,
so recovering identical config does not require another activation merely to
clear a stale pushed failure. File observation, transaction validation and
metrics still run: this is not a claim of zero work or measured latency savings.
Immutable CLI-generated input also uses this entry; ordinary
file reload still captures its version before file observation. Neither entry
persists submitted intent or replaces the frontend's source cache. This is an
internal binary-host API, not a public embedding crate or an authenticated Admin
endpoint; frontend composition and durable recovery remain separate work.

The App installs a one-time `ReloadStatus` adapter backed by a Weak reference to
its GenerationStore. Health reports sample the authoritative `last_failure`
instead of trusting delayed push notifications. A late activation cannot erase
a newer rejected intent, and a late `Outcome::record` cannot restore an obsolete
configuration failure. Hosts without this adapter retain the standalone push
API. Other health reasons (listeners, upstreams, certificates, drain) remain owned
by their existing subsystems. The adapter runs before taking the Health lock,
performs no I/O, and must not re-enter Health::report; this avoids holding the
health lock while reading the transaction store. It must not be called while
already holding that store's write lock. The weak reference avoids a runtime /
telemetry / store ownership cycle.

File read/parse failures use `reject_observation_if`: the full captured Version
and seal are checked under the same write lock as failure recording. Conflicts
become Stale at the host and cannot degrade the newer version. No tree/revision
is invented for unreadable input; therefore this does not fence a preparation
at the exact same unchanged Version. Failure attempts saturate and successful
publication/NoOp clears the authoritative failure. Reports are point-in-time
samples of independent health sources, not a globally atomic system snapshot.
Counters and generation gauges still use outcome notifications. The generation
instrument independently uses an atomic unsigned high-water mark, so late
Started/NoOp results cannot lower a more recent reported generation. It belongs
to one store/boot; a new host must create new telemetry. Counters still count each
notification, and success timestamps use notification time, not commit time.
These remain sampled metrics rather than write validators or an atomic snapshot.

The watcher's shutdown future is selected once around its complete lifetime,
including initial discovery, debounce, reload and activation. It is not recreated
only between reloads. Stop wins when both branches are ready; dropping the watcher
wait does not cancel admitted blocking work, which the caller's subsequent App
shutdown fences and drains. Synchronous OS watch/unwatch calls still cannot be
preempted inside a poll. This does not extend the signal subscription backward
to CLI startup before entering the watcher.

For multi-source frontends, `Snapshot::observe_if` conditionally composes one
source against its expected revision, retaining all other sources. `None` means
the source must be unseen; clearing contributions retains an empty observation
with a revision. Conflict and revision exhaustion leave the snapshot unchanged.
This is a local composition primitive, not authorization or publication: hosts
must authorize sources, serialize their intent snapshot, supply complete content
fingerprints, and submit through `GenerationStore::attempt_if` using the whole
store version. Cloned snapshots do not share a CAS; a losing writer must recompose
against current intent. No durable storage or Admin HTTP endpoint is implied.

`Version::etag()` produces a quoted, full-length opaque configuration write tag
from the full boot identity, generation and sorted source revision vector.
`GenerationStore::attempt_if_etag` accepts that exact single tag and delegates
to the whole-Version CAS under the transaction lock; checking a tag never grants
permission to bypass a later version conflict. Failed/no-op observations can
invalidate tags even without a new serving generation. This is neither an auth
token nor a validator for arbitrary status JSON (which may contain uptime).
HTTP adapters still own header limits/parsing, authentication and authorization;
the core method does not implement wildcard/list/weak If-Match semantics.

Boot identity generation mixes PID, full seconds/nanoseconds and a checked
process-local atomic sequence. Separate stores no longer rely on wall-clock
progress or near-zero Instant elapsed measurements to distinguish themselves.
The sequence fails closed on exhaustion instead of wrapping. This is not a
cryptographic nonce generator: exact PID/time reuse after a process restart or
cloned process images still requires a stronger host-provided identity design
before claiming those failure models are covered.

The spec fingerprint includes the source owner of listeners, sites and upstreams:
moving an explicit resource between allowed file sources publishes updated owner
metadata even if wire behavior is unchanged. Origins, source revisions, comments
and import layout are not added to the behavior fingerprint. Ownership does not
by itself invalidate transport compatibility: existing pools and other state
remain governed by their individual compatibility hashes. This is metadata
publication, not a grant allowing one source to take another namespace.

The process-owned `BindTable` accepts an already-bound standard TCP listener
through `adopt`, in a Tokio runtime with I/O enabled. Registration uses the
socket's actual local address and never closes/rebinds it; queued connections
survive the transfer. Duplicate addresses are rejected without replacing the
existing listener. Configuration must name that address, after which normal
prepare/reload/retain ownership applies. Failure drops the supplied handle;
external clones remain the caller's responsibility. This embedding hook is not
systemd/Windows socket activation, descriptor discovery, or a Unix listener.
Application lifecycle fixtures use it to avoid releasing an ephemeral port
between allocation and the initial configuration transaction.

```
 Frontends   KDL file        Admin API (later)      Kubernetes (later)
                │                   │                      │
                └──── each lowers into Node, a format-neutral value tree ────┘
                                    │
 Snapshot   immutable: what every source declared, at one moment
                                    │ normalize — resolve, validate, assign identity
 Spec       immutable resource graph, source-agnostic ─────────────► spec_hash
                                    │ compile — order, flatten, bind state
 Plan       immutable execution plan ──────────────────────────────► plan_hash
                                    │ prepare — bind sockets, build handlers
 Generation published atomically; a connection pins one at accept
                                    │
 State      typed, mutable, owned by resources rather than generations
```

Five nouns, two hashes, one counter. Everything else is named after one of them.

### Why two hashes

They answer different questions.

`spec_hash` answers *did the operator ask for something different?* Comments,
whitespace, declaration order and file layout are gone by the time it is
computed, so reformatting a file produces the same value and the reload stops
there.

`plan_hash` answers *would this behave differently?* It includes the build's
capability set, so the same configuration compiled by a binary without a given
bundle produces a different plan — and the second binary can never mistake the
first one's plan for its own.

## What each layer may not do

The layering is enforced by `cargo run -p xtask`, which reads each crate's
declared dependencies and compares them against a whitelist. The same command
enforces three other things the compiler cannot: file length, agreement between
the events the code emits and `docs/EVENTS.md` in both directions, and that
every configuration field the compiler produces is read by something above it —
because a knob that is accepted and quietly ignored sends an operator looking
in the wrong place.

| Crate | Cannot reach |
|---|---|
| `gatel-model` | any async runtime, any I/O, KDL, HTTP |
| `gatel-macros` | anything but syn/quote and the model |
| `gatel-kdl` | anything but the model and `kdl` |
| `gatel-observe` | everything but the model — it is written to from the data plane, so it must sit under all of it |
| `gatel-plan` | hyper, rustls, `tokio::net` — it may name `salvo_core::Handler`, but it cannot open a socket |
| `gatel-state` | Salvo routing; it owns transport to backends, not request dispatch |

The point is what they *cannot* do. A compiler that cannot open a socket cannot
grow a side entrance into the data plane, however convenient that would be in
the moment.

## The transaction

A Snapshot is the complete source set, not a partial update. Its own revision
vector must equal the merged observed head before compilation starts: an old
snapshot cannot borrow newer revisions and publish old content. A source whose
contributions are removed remains as an empty SourceSnapshot at a newer
revision. Omitting a previously observed source is stale, not deletion.
Frontends must assign monotonic revisions consistently; the file host serializes
read/parse, revision assignment and cursor update with one observation lock,
independent of the serving and preparation locks.

The store also pins each newest observed revision to its supplied content
fingerprint, even when that intent is rejected or a no-op. Reusing the same
source revision with another fingerprint returns Stale before compilation and
does not observe any part of that candidate. A retry with the original content
is allowed; recovery with edited content needs a newer revision. The latest
snapshot per known source is retained, including its fingerprint and lowered
tree, not a history of configuration bytes. The low-level full-union API checks
declared revision/fingerprint identity, not the honesty of its parser; manually
constructed snapshots remain trusted input. `SourceSnapshot::observe` and
`SourceWriter` additionally reject different trees under a reused fingerprint.
Neither check proves that a fingerprint covers all external semantic inputs.
No source payload or fingerprint is included in the revision-reuse error.

`SourceSnapshot::observe` provides the shared revision allocator used by KDL:
it rejects a previous snapshot from another source, carries the revision for
unchanged content and uses checked increment for changed content. SourceRev::next
now returns Option; exhaustion is a diagnostic, never wraparound or a reset.
Hosts must still serialize reading, assigning revisions and retaining observations.
This helper does not allocate revisions globally or authorize a source writer.

GenerationStore defaults to at most 256 distinct source identities over its
lifetime. Embedded hosts can choose a nonzero bound with `with_source_limit`.
Over-capacity observations return Stale before changing any head or fingerprint;
existing identities can still advance. Failed observations and empty tombstones
consume capacity because forgetting their revision could admit old content.
Do not retry an unchanged over-capacity candidate indefinitely. This bound is
on identities, not nodes, bytes, source-name length or per-tenant quotas; frontend
payload limits and source ownership policies are separate requirements.

Hosts can instead construct `GenerationStore::with_allowed_sources` with a
fixed set of exact source identities. Duplicate registrations collapse; an empty
set admits no nonempty source observation. Unknown identities are refused before
any part of an observation is retained. The lifetime capacity is the registered
set size. CLI file mode registers only file:main; its immutable snapshot mode
registers the sources supplied by the host (including file:serve for `serve`).
The lower-level default store remains capacity-bounded but dynamically admits
identities for embeddings that manage admission elsewhere.

Only File sources may declare `global`. Admin and Kubernetes sources may
contribute namespaced sites/upstreams, but cannot supply or shadow global
listeners, blocking budgets, logging and CA settings. This is an enforced
normalization rule, not an HTTP authentication system. Frontends must assign
SourceId from trusted host context, never from a request's claimed identity.
Registering a source does not restrict its route directives, filesystem/secret
access, host matching or upstream targets. Tenant isolation still needs explicit
capability policy and cannot be inferred from resource namespaces alone.

Implicit site, route and inline-upstream identities now use `derived_owned`:
File identities remain in the root namespace; Admin identities start with
`runtime/` and controller identities with `k8s/<namespace>/`. Equal resource
content from different namespaced sources no longer generates the same root ID.
Within a site, identical inline upstream addresses still share an identity,
provided their complete decoded policies also match. Address declaration order
does not change that identity. Namespaced derived IDs are recognized
by `is_derived`. Existing non-file implicit IDs and their state keys intentionally
change; no cross-identity state migration is promised.

Proxy syntax is strict in all three forms: at most one shorthand address,
only the `upstream` property, and no duplicate property. Multiple endpoints and
upstream options belong in child nodes. Previously ignored extra shorthand
arguments or properties now produce diagnostics rather than silently changing
the operator's intended routing.

Inline upstream reuse includes the full decoded policy fingerprint: timeouts,
pool limits, retries, health checks, endpoint weights, balance, protocol, TLS and
resolver settings. Address-only deduplication is forbidden because it silently
makes the first route's policy win. Equivalent shorthand/body defaults still
share; policy changes now produce a new anonymous identity and fresh state.
Use an explicitly named upstream when its logical identity must survive policy
changes under the normal state binding rules. The address-only helper returns
an identity seed for decoding, not the final reusable runtime identity.

Upstream policy blocks other than endpoint are singletons: repeating balance,
timeouts, pool, retry, health, passive-health, protocol, resolve or TLS is rejected
before decoding values, with both declaration origins. The manually decoded
balance/protocol/TLS nodes also reject surplus arguments, nested nodes and
duplicate properties; protocol accepts no properties, and a non-string TLS
server-name cannot silently fall back to endpoint-derived SNI. Named upstreams
require exactly one name. These rules apply to both named and inline upstreams
and to direct model producers, not just the KDL parser.

Numeric policies are not silently clamped: retry budget-percent is 0..100,
retry per-try is nonzero, active health interval/timeout are nonzero and both
health thresholds are at least one. HTTP probe status is 100..599 and passive
health max-fails is at least one. Invalid intent fails normalization and retains
the last good generation. A zero retry budget or zero jitter remains valid;
these are not treated as missing values or replaced by defaults.

Health probe HTTP drivers are owned by abort-on-drop guards: response timeout
or cancellation also closes H1/H2 transport instead of detaching its driver.
Probe rounds listen for stop while checks are in flight, and cancelled rounds
do not report backend failure. Concurrent probe/resolver batches poll their
children as owned futures, preserving result order; dropping a batch drops all
unfinished operations. A child panic now unwinds the batch rather than silently
omitting its result. Worker stop retains its join handle through the grace
timeout and aborts/joins on expiry. These are cooperative async cancellation
guarantees, not a way to interrupt blocking code or forcibly kill threads.

Supervisor serializes complete reconcile/shutdown operations with a lifecycle
lock. Shutdown is terminal: later reconciliation cannot restart the census,
resolver or probe workers. Repeated shutdown remains allowed to finish cleanup
if its caller cancelled an earlier shutdown future. Reconcile skips reuse for
finished handles and replaces them, while running/resolving status excludes
finished tasks. Probe, resolver and background census services additionally restart after an
unexpected return or unwinding panic, with exponential delays of 1, 2, 4, 8,
16 and at most 30 seconds; a run lasting at least 60 seconds resets the delay.
The recovery loop polls one owned child at a time and cancels it or its backoff
when stopped. No child task is detached. Running/resolving includes services in
backoff, not just actively probing ones. Panic-abort builds and process failure
cannot recover this way. Census retains the immediate synchronous publication
on follow; recovery covers subsequent background sampling, with its normal
five-second interval after each restart. A synchronous sample cannot be
preempted midway by an asynchronous stop signal.
Worker and census task ownership aborts on Drop; awaited
shutdown remains the API for joined cleanup. Census used independently retains
its existing restartable follow/shutdown behavior.

The CLI App serializes activation and the closed/teardown phases of shutdown.
Activation reads the committed generation only after taking that lock, so a
queued activation cannot later apply an earlier captured snapshot. If a commit
overtakes awaited server/ACME reconciliation, activation loops to the current
generation before pruning state, certificate entries and metric series.
Shutdown closes this App lifecycle permanently while repeated shutdown remains
allowed. Admission drain occurs between those phases with the lifecycle mutex
released, so queued activation can observe closed and complete. This does not
make all
runtime side effects an atomic transaction; direct users of Supervisor remain
responsible for submitting current committed runtimes in order.

A resolved endpoint's active probe has one time budget across all addresses.
Each sequential address attempt receives only the remaining duration measured
with Tokio's monotonic clock. Exhaustion stops before opening another transport;
an early failure may still fall back while time remains. The budget is per
logical endpoint, not multiplied by DNS address count, and cancellation still
owns and closes the current HTTP driver. This does not introduce parallel
Happy Eyeballs probing or guarantee every address receives a time slice.

HTTP health probes borrow the connection pool's prepared `ClientTls` policy but
open their own sockets: CA verification, SNI, client identity and ALPN are shared
with business traffic without borrowing a pool slot. TLS handshaking consumes
the same probe budget as connecting and reading the response head. HTTP/2 probes
use an explicit `https` scheme for TLS and `http` for plaintext. The configured
health `host` controls HTTP authority, not the certificate verification name.
TCP-connect probes intentionally stop after connecting, even on TLS upstreams;
use HTTP probes to check TLS and application health. Probe worker identity now
includes the TLS specification so changing it replaces the worker. This does not
implement automatic detection or urgent revocation of same-path secret changes.

When a reload reaches preparation, ClientTls fingerprints the validated DER
trust certificates and client certificate chain together with its configured
policy and protocol. Private-key bytes are not fingerprinted or exposed.
Connection-pool runtime compatibility combines that identity with the plan
binding, and probe worker identity includes the pool's loaded TLS identity.
Thus same-path material replacement takes effect on a prepared generation;
PEM-only whitespace changes preserve reuse. Plan hashes and same-plan NoOp
still do not observe external file contents. Existing generations retain their
old pools for in-flight work: this is rotation, not emergency revocation.

GenerationStore exposes explicit `refresh` and version-conditional `refresh_if`
operations. They bypass only the equal-plan NoOp shortcut: observation identity,
normalization, preparation, publication guards and generation/head stale checks
are unchanged. A successful refresh consumes a generation even with identical
plan hashes and source revisions; failure retains the serving generation. Hosts
must invoke this path when external resources need refreshing. Ordinary attempt
keeps its NoOp behavior; refresh is not automatic file discovery, a resource
snapshot across multiple files, or forced revocation of old connections.

App's ordinary reload attempt now observes current upstream TLS material against
the identity loaded in the serving pools. Changed, invalid or missing material
selects `refresh_if`; unchanged material selects `attempt_if`. Both use the
version captured before resource observation, so an intervening commit makes
the observation stale. The existing filesystem watcher and five-second fallback
poll call this path, including for resources outside the watched directory via
polling. Invalid material fails preparation and leaves the serving generation
intact. This covers upstream CA/client certificate/key material, manual server
certificate/key files, inbound mTLS CA files, and declared plugin secrets. Server
TLS preparation records the exact bounded bytes it parsed in a candidate-owned
observation set (up to 4096 distinct files), and rejects inconsistent duplicate
reads. Those observations publish only with the runtime; file-only edits select
the same conditional refresh path without changing source revisions or plan
hashes. Missing, malformed or permission-invalid replacements retain the last
serving generation. This is not cross-file atomic rotation or emergency
revocation: an already established connection retains its original TLS policy.
Fixed-snapshot serve mode does not start a
configuration watcher.

Prepared plugins now share generation-owned SecretDependencies through
BuildContext. The secret reader records the actual resolved value's private
digest, with a 4096-reference limit and duplicate-read consistency checks.
Runtime owns those observations; App uses their changed predicate alongside
upstream TLS observation to select conditional refresh. The plan still contains
references only, and neither values nor digests are rendered by the tracker.
Basic-auth file rotation is tested through real HTTP requests. Plugins that read
files outside BuildContext::secret need their own explicit integration; there
is no cross-file snapshot or immediate revocation guarantee.

Unix-socket health probes default their HTTP authority to `localhost`, not the
filesystem path. `health host` overrides it for both H1 and H2; this is unrelated
to TLS SNI and certificate verification. Real Unix H1/H2 tests cover both forms.
Mutual-TLS probe tests require a trusted client certificate on the server and
exercise missing, trusted, untrusted, restored and removed client identities
across reloads for both HTTP protocols, with alternating health verdicts.

Resolver workers prioritize cancellation during both an active round and the
refresh delay. Stopping or closing the control channel drops the pending round;
an already stopped worker starts no new lookup. Cancellation is not a failed DNS
result and does not publish a failure verdict. This owns the asynchronous wait,
not the operating system resolver: dropping Tokio's lookup future cannot
forcibly terminate a blocking system lookup that has already begun. Tests inject
pending rounds into the private scheduler so shutdown guarantees do not depend
on an external DNS server being slow.

Site IDs are unique across sites, and route IDs across all sites, independently
of their host/path matchers. Duplicate identities fail normalization with both
origins; they cannot intentionally alias state bindings. Resource kinds remain
separate identity domains. Implicit route IDs include additional match conditions
when present, so conditional and fallback routes cannot accidentally share
rate-limit/cache/plugin state. The matcher contribution is order-independent;
routes without additional matchers retain their previous identity. This changes
state keys for implicit conditional routes; choose unique explicit IDs when a
logical route must retain its identity across matcher edits.

Success, no-op and failure publication all re-check the source head and base
generation. An overtaken normalize/prepare error returns Stale rather than
overwriting the store's newest failure or marking a newer successful generation
degraded. This governs GenerationStore state; external consumers still must
order their own asynchronous status/telemetry updates correctly.

Conditional control-plane writers use `Status::version()` and
`GenerationStore::attempt_if`, not a separate status check followed by `attempt`.
The precondition includes BootId, generation and the complete observed head.
It is checked under the same write lock that observes the candidate: a mismatch
returns `Conflict` with the current version without changing revisions, failure
state or runtime, and without preparing resources. Failed/no-op observations
invalidate an older head token; a successful retry at the same source revision
invalidates the generation token; another store lifetime invalidates BootId.
After admission, normal stale checks still apply: passing a precondition does
not promise eventual commit. Unconditional file observations remain supported.
This is an in-process optimistic precondition, not an HTTP ETag encoding,
authorization layer, source ownership policy, revision allocator or durable
write acknowledgment. Those belong to the control-plane frontend and must not
be inferred merely from the presence of this API.

HTTP preparation now stages an independent StateRegistry registration map,
sharing compatible values but isolating replacements and additions. Successful
publication swaps registrations; failed/stale candidates cannot supply state to
later candidates. Prepared pool policies are adopted only after commit, while
the registry publication guard still serializes subsequent staging/publication.
The generation pointer is published before these guards are released; this is
not an atomic snapshot of every pool metric or in-flight request. Displaced
registration values remain owned outside the publication guard so plugin Drop
code is not invoked there. Staging is not a deep clone: plugin preparation must
not mutate reused values.

Certificate preparation now uses a separate incremental write set. Manual
certificates replace their declared sites only after successful publication;
internal issuance and ACME disk adoption only fill missing slots, rechecking
under the certificate-store lock so a concurrent renewal is not overwritten.
Unchanged sites are never copied back from an old snapshot. Resolvers retain
the live store and therefore still observe post-commit renewals. The certificate
publication guard blocks handshakes until the committed delta has landed.
Each staged TLS site now declares certificate provenance: source and hostname,
plus file references for manual certificates or ACME configuration for managed
issuance. Changed provenance clears the previous certificate at commit; compatible
reloads preserve the certificate and an opaque issuance token. ACME obtains that
token before ordering and checks it under the store ownership lock before writing
the in-memory result. Switching away and back creates a new token, so old orders
cannot win an ABA race. The complete TLS-site set is published with the generation;
removed sites reject both outstanding and newly attempted old-owner writes.
Standalone ACME stores remain usable without a generation host until managed
ownership has been enabled. Direct CertificateStore.put is a trusted-host override,
not the renewal API.
The application separately owns the active ACME manager under its lifecycle lock;
the preparation cache is not an inventory of running services. Failed candidates
cannot redirect shutdown, and preparation first reuses the committed account when
its configuration matches. Account replacement shuts down the old manager before
activating the new one. Reconciliation publishes initial readiness synchronously,
then starts the first sweep in an owned cancellable worker. Identical wanted sets
keep the worker, avoiding order cancellation on every compatible reload. Shutdown
aborts and joins the worker; cancellation while joining also aborts through Drop,
rather than detaching after a timeout. This cannot interrupt synchronous filesystem
or cryptographic work mid-poll, nor retract an order already received by a CA.
This isolates in-memory publication, not internal CA/account files created during
preparation, already-running CA orders or their disk-cache writes, emergency
connection revocation, or an atomic snapshot across credential files. Internal
authority directory/material rotation still needs an explicit identity policy.

Issued certificates now use certon 0.3's `CertStore` contract through
`AtomicCertStore`: a bounded `certificate-<hash>.identity` JSON record stores
version, resource metadata and both PEM values. Readers open one file and validate
the complete resource. Writers validate first, create a unique
same-directory temporary file (0600 from creation on Unix), write and sync it,
then rename over the previous record. Unix also syncs the parent directory;
Windows inherits directory ACLs and this does not promise power-loss durability
on every filesystem. A pre-rename error leaves the old record intact; a directory
sync error after rename means durability is uncertain, not that nothing changed.
Missing new records may use legacy `<host>.identity`, then `.crt`/`.key` files within the same account
namespace; corrupt new records never
fall back to stale legacy material. Existing legacy files are not deleted.
An abrupt process death can leave a private temporary file; CA registration
recovery, stale-order write ordering and filesystem crash
testing remain separate work. Atomic pair replacement does not solve those.
Removal writes a tombstone rather than allowing legacy fallback to resurrect it.
See [CERTON.md](CERTON.md) for the Policy/CertManager split, store contract and
migration boundaries. Workers retain only renewal state, not their owning manager;
last-owner Drop can cancel them without a strong-reference cycle. Explicit shutdown
remains necessary to await worker completion and tracked storage operations.

ACME managers sharing a CertificateStore now also share cancellable order gates
keyed by the canonical cache-parent directory and identity filename. The gate is
held from before contacting the CA through persistence and in-memory publication;
another order for the same cache target cannot overtake it. Ownership is rechecked
after waiting and after issuance, and renewal eligibility is rechecked after
waiting to avoid redundant orders. Inactive gates are weakly held and pruned.
No configuration/publication lock is held over network or filesystem I/O.
This gate alone is in-process ordering, not an atomic transaction
between configuration commit and rename: ownership can still change after the
pre-write check. Different-account cache provenance and hostile directory-link
replacement are not solved by canonicalizing a parent once.

Account-library storage and Gatel identity files now share a stable namespace
`<configured-base>/accounts-v1/<full-hash>`, derived from a versioned domain, the
exact configured CA directory URL and email. Renewal-window and agreement-policy
changes do not move the cache; CA/contact changes do. This isolates configured
origins, not cryptographic provenance of files copied by an operator. Unscoped
legacy files are retained but never automatically adopted, so upgrading from the
old layout may require fresh issuance. New Unix namespace directories are created
0700; existing group/other-accessible account directories are refused. Windows
retains inherited ACL behavior. This is not protection against an attacker who
can replace a parent directory.

Cooperating processes now serialize an account's orders with an exclusive OS
file lock on the persistent `account.lock` inside its namespace. Nonblocking
attempts with async waits permit cancellation without detached lock waiters;
unsupported locking fails the order rather than silently continuing. Never
unlink this file while any process uses the account. The lock covers account
loading, issuance, identity persistence and publication. After acquiring it,
the manager rechecks ownership and adopts a fresh cached certificate written
by another process before requesting another order. Each order builds a fresh
certon issuer so an earlier in-memory account cannot bypass disk refresh.

A private storage adapter replaces certon's nested lease-file locks with the
already-held account lock. Storage mutations run in owned tasks retaining the
lock until their calls complete, even when issuance is cancelled; pending
filesystem writes therefore are not deliberately abandoned at normal worker
shutdown. An issuer-owned task census now waits for these operations after
stopping and joining the renewal worker. The worker handle and App's active
account remain owned across cancellation points, so a cancelled shutdown or
account replacement can retry its drain. Reconciliation also drains previous
storage work before starting a replacement worker. Standalone issuer users can
stop issuance producers and call `drain_storage`; this is not admission closure
for arbitrary concurrent `issue`/`sweep` callers. A stalled filesystem can delay
shutdown; whole-runtime destruction and power-loss recovery remain separate work. This is
cooperative local filesystem exclusion, not NFS/distributed consensus, protection
from old/nonparticipating writers, remote CA/disk transactions, or shared
HTTP-01 challenge routing across replicas. Direct `Issuer::issue` also locks but
does not perform the manager's cache reuse/publication workflow.

The private storage adapter now translates certon 0.3's registration-then-key
save sequence into a single versioned `account-<hash>.key` record. Registration
is staged in memory; the key write validates both components and publishes one
bounded record using a synced private temporary file and rename. Unix also syncs
the namespace directory. Readers validate magic, exact lengths, JSON and private
key decoding; truncated, trailing or corrupt records fail closed. Certon's rollback
after a failed second write removes staging, not the last good record. Deleting
either component without a staged save removes the complete account record.

This is intentionally a scoped protocol adapter, not a general transactional
implementation of Storage: the first component is not durable until the second
write succeeds, and enumeration is explicitly unsupported. Its key allowlist
accepts only canonical certon account components and the registration-intent
file. Unknown layouts, unexpected filenames, path aliases, traversal and alternate
stream spellings fail before filesystem access instead of falling through to
ordinary file storage. Dependency changes requiring new keys need explicit review.
Certon's distributed-solver wrapper is disabled: Gatel's HTTP-01 listener reads
its own Challenges table, not that wrapper's disk-token handler. This removes
unused duplicate challenge persistence, not an implemented replica-sharing feature.
Tests invoke
certon's actual save/load/delete API to pin that contract. Existing separate
registration/key files are retained but not automatically imported; certificate
identity caches are unaffected. Upgrades can register a fresh account on the next
issuance. Before issuing, Gatel now persists a bounded private
`registration-intent.key` under the same account lock. Existing committed account
keys seed a missing intent; a corrupt intent or a key conflict fails closed.
The actual pre-registration callback supplies that durable key to certon (its
account_key_pem builder field is not consumed by this version's creation path).
An interrupted registration therefore reuses its signing identity when reopening
the issuer. RFC 8555 section 7.3.1 requires an existing account's URL to be returned
for a newAccount request signed by the same key; the HTTP fault fixture verifies
that Gatel sends the same JWK after losing a response. It does not independently
verify CA behavior or JWS signatures.

The intent remains after registration and must be backed up with the account
record; it is not rotated with leaf certificates. Operator key rotation/import,
runtime destruction and real power-loss testing remain separate work. Persisting
the intent gates network registration, but storage durability still depends on
the documented filesystem/platform guarantees, not a distributed transaction.

Runtime publication now uses the host-only `PublishRuntime` / `PublishGuard`
contract. After stale checks, the store allocates the candidate Committed value,
then obtains its publication guard under the generation write lock. The budget
guard locks admission and stages validated limits, preserving usage. The store
publishes the generation before releasing the budget guard. Admission and budget
snapshots therefore cannot observe staged limits before publication; already
admitted work remains counted. Dropping an unpublished budget guard rolls back
its limits. HTTP lock order is generation → registry → certificates → budget;
publication must not perform I/O,
await, reenter the store, or invoke plugin code. This is a trusted-host contract,
not arbitrary plugin transaction rollback. Custom embedded runtime types must
implement PublishRuntime; use the unit guard when no shared-state change exists.

```
observe  → publish the source revisions, so a slower candidate can tell it was overtaken
normalize→ Spec, or a list of every problem
compile  → Plan; if plan_hash is unchanged, stop here and consume nothing
prepare  → bind sockets (all or nothing), resolve state, build handlers
commit   → under one lock: re-check the revisions and the generation, then publish
```

Only `commit` takes the write lock, and everything expensive happens outside it.
Correctness comes from re-checking, under the lock, the two things the candidate
assumed: that no source moved, and that no other generation was committed.

Four failure modes, each leaving the running configuration exactly as it was:

- **Rejected** — the configuration is invalid. Every problem is reported.
- **PrepareFailed** — a port is in use, a handler will not build. No generation
  is consumed.
- **Stale** — something newer was observed while this candidate compiled. What
  preparation acquired is dropped, which is why preparation must clean up in
  `Drop`.
- **NoOp** — nothing would behave differently. Nothing is built and no state is
  touched.

## Three lifetimes, kept apart

This is what makes a reload invisible to traffic.

**Sockets belong to the process.** A reload that keeps an address keeps the
socket, so there is no window in which a connection is refused. A reload that
adds addresses binds all of them or none.

**Route tables belong to a generation.** Each generation holds its own immutable
handler graph. Building it cannot mutate the running one.

**A connection belongs to the generation that was current when it was
accepted.** It keeps that one until it closes, so an in-flight request never
sees configuration change underneath it, and an HTTP/2 connection's later
streams stay with the configuration their connection started under.

## State that outlives configuration

State is keyed by `(owning resource, kind)` and carries a compatibility
fingerprint. A reload asks "is what I need already running?" and gets a typed
answer.

| Change | Endpoints | Pool | Selector | Rate-limit buckets |
|---|---|---|---|---|
| Raise `max-per-endpoint` | reused | **reused** | reused | reused |
| Replace the backend list | replaced | replaced | replaced | reused |
| Change `balance` | reused | reused | **replaced** | reused |
| Raise `rate-limit max` | reused | reused | reused | **reused** |
| Change `rate-limit max-keys` | reused | reused | reused | **replaced** |
| Change `rate-limit key` | reused | reused | reused | **replaced** |
| Edit an unrelated route | reused | reused | reused | reused |

Raising a pool limit must not drop live connections; changing the balancing
strategy must not pretend the old cursor still means anything; raising a rate
limit must not hand every client a fresh allowance. `gatel diff` prints this
table for two real configurations before either is applied, and
`gatel-http/tests/stability.rs` fails if any cell of it stops being true — the
table is a test, not a description.

Changing `rate-limit max-keys` rebuilds its state with the new capacity and
resets existing counts. Diff reports an incompatible restart; a failed or stale
candidate does not replace the active state's counters or capacity.

For upstream pools, `max-per-endpoint` limits physical connections for both
HTTP/1 and HTTP/2. HTTP/2 also has two independent local reservation bounds:
`max-h2-streams-per-connection` and `max-h2-streams-per-endpoint`, both defaulting
to 128 and rejecting zero. Both enter the policy hash, including inline proxy
identity. An endpoint's stream bound spans all its physical connections. Each
connection is further constrained by the remote SETTINGS already received and
observed; before receiving it, Hyper's initial estimate is 100. Hyper applies
the updated remote limit to actual wire sends. A SETTINGS value of zero admits
no new requests. Actual Hyper I/O/body task progress wakes the dispatch driver,
including GOAWAY while no requests are queued. While any checkout in that
upstream pool waits, a 50-ms observation timer also samples capacity, so a peer
raising a zero limit does not need another request to trigger progress. With no
queued checkouts the timer stops; demand is aggregated per upstream, rather
than per endpoint. This avoids periodic idle polling, without claiming a
measured CPU or throughput improvement.

One HTTP/2 lease admits one request, and its stream reservation remains occupied until
both request upload and response body finish or are cancelled. A successful
reload updates these bounds without killing older streams; a shrink may leave
temporary usage above the new limit while existing work drains. GOAWAY retires
the connection for new requests while retaining its physical slot until older
streams finish. With `max-per-endpoint=1`, a replacement waits for those streams
instead of opening a second physical connection. Physical connection gauges and
reserved-stream gauges therefore measure different resources.

HTTP/1 admission records both allocated and target capacity. Returning leases
pay down shrink debt before waking new work; a cancelled waiter with an
already-assigned semaphore permit cannot evade the debt. Pool publication and
lease return share the current idle/lifetime policy lock: a lease created under
an older generation cannot repopulate a newly disabled idle pool. Publication
also trims existing idle sockets, retaining active requests until they finish.

Directives take part in the same table. A directive declares what it needs to
keep between requests, and its state is owned by the site or route it was
written on — so moving it gives it different state, and editing an unrelated
part of the file does not. There is no path to a global singleton.

The kinds a directive can use are not a list in the kernel. Four of them belong
to the proxy and one to TLS, because those are one correctness chain; every
other kind is named after the directive that asked for it. A bundle from
another crate declares state exactly as the standard one does, and the name
cannot collide: a binding is keyed by owning resource and kind, the owner is
the scope the directive was written on, and a directive may appear at most once
per scope.

Certificates are in the same table for the same reason, with one difference: a
renewal is not a configuration change, so it never produces a generation. The
resolver reads through the store on each handshake, which is what lets a
certificate be replaced while established connections carry on.

That is also why an automatic certificate is never ordered during a reload. An
order takes seconds when everything works and minutes when it does not, and a
reload that waited for one would be a reload a certificate authority's bad
afternoon could stall. A commit publishes the intent — these sites want
certificates — and a background task reconciles against it afterwards, exactly
as a health check does. A site with no certificate yet cannot be served over
TLS, and readiness says so; it does not get a certificate this process signed
itself, because one of those is a certificate that lies.

The ACME protocol is `certon`'s. What is Gatel's is where the challenge is
answered: the listener the configuration already declares, before routing and
before every guard, rather than a second server the library would otherwise
start on port 80. A certificate authority asking a question is not a visitor —
`force-https` must not redirect it and a rate limit must not refuse it.

## Ordering is decided once

Middleware runs in phase order, and the phase is part of a directive's identity
rather than something configuration chooses:

| Phase | Position | Examples |
|---|---|---|
| `observe` | outermost | `access-log` — may fail open |
| `guard` | | authentication, filters, limits, CORS, timeouts — where a request is refused or answered instead of served |
| `request` | | rewriting |
| `response` | innermost | header and body transformation, error pages |

Within a phase: scope depth (site before route), then `order`, then name.
Declaration position appears nowhere. Writing `basic-auth` after `proxy` in the
file cannot move authentication after the terminal.

`order` exists for the few places where alphabetical order would be wrong.
`timeout` carries a negative one so it wraps the other guards rather than
sitting among them: "this route has thirty seconds" means the whole route,
including a check that has to ask something else. Configuration can set `order`
on any directive, which is the escape hatch for a case the defaults did not
anticipate.

A site's own directives also form a fallback chain, run when a request reached
that site and none of its routes matched. Otherwise a site-wide access log would
miss every request to an unknown path — the ones most worth seeing — and a
site-wide error page could not cover its own 404. A request whose host no site
claims runs nothing, because no site's directives apply to it.

Routes are sorted by specificity at compile time — exact host over wildcard over
catch-all, exact path over longest prefix — so the request path walks a fixed
list and takes the first match. Two routes that would claim the same request are
a compile error, because otherwise the winner would depend on declaration order.

## What the process says about itself

Metrics and health are served by ordinary routes — `metrics` and `health` are
directives like any other. There is no admin port, which means there is no
second surface serving privileged information on every interface because
somebody left a default alone. A scrape route can be put on a loopback listener,
behind `ip-filter` or `basic-auth`, at a path nobody guesses, and the rules that
apply to it are the ones written next to it.

**A series is created from a name in the plan, never from a request.** A route,
an upstream and a listener each resolve their instruments during prepare and
hold them for the life of the generation. What the request path has is that
handle, with no way back to the registry — so a path, a header, a client address
or an invented status code cannot become a label, however much traffic arrives.
Status is reported by class rather than by code for the same reason. After a
commit, series the new plan no longer names are dropped, and ones it still names
keep their values: a counter that restarted at zero on every reload would make
every rate wrong precisely when someone is watching a deploy.

A request is counted where its **body ends**, not where the handler returned —
otherwise a streaming download reads as instant, a cancelled one as a success,
and a compressed body at its uncompressed size. The access log records the same
moment for the same reason, in `text`, `json` or `clf`.

An access log is not a diagnostic log, so it has its own destination — standard
error, or a file that is opened during prepare and rolled by size. An open file
is a process resource like a socket: two generations that log to the same path
share one appender, so they cannot interleave half-written lines or rotate out
from under each other. Writing happens on its own thread behind a bounded queue,
and a full queue drops lines and counts them, because nothing on the request
path may wait for a disk.

Readiness distinguishes two kinds of problem, because they call for opposite
responses:

| Condition | Readiness | Why |
|---|---|---|
| Nothing committed yet, no listener bound, draining | **not ready** | Local to this instance. Another one is fine; move the traffic. |
| Checkpoint persistence outcome unknown | **not ready**, latched for this host lifetime | Disk and memory may disagree. An in-memory commit cannot reconcile this condition; liveness remains successful. |
| Reloads failing while the last good configuration serves | **degraded**, still 200 | Every replica reads the same file. Failing readiness here turns one bad edit into a total outage. |
| Every endpoint of an upstream unusable | **degraded**, still 200 | Shared by every replica. Removing this one moves traffic to another that cannot reach the backend either. |

Both kinds are named in the body; only the first takes the instance out of
rotation. Liveness answers yes as long as it can answer at all — a liveness
probe that fails on a downstream condition restarts a process that was working,
usually across every replica at once.

## The one thing that is filled in

Nothing else in this design guesses, so the exception is worth stating exactly.
A site with no `tls` gets a development certificate when **both** of these hold:

- every listener it could use terminates TLS, so without one it would be served
  nowhere at all, and
- its host can only ever be local — `localhost`, `*.localhost`, or a name under
  `.test`, `.local`, `.internal` or `.home.arpa`, or a loopback, private or
  link-local address.

Both halves matter. The first means this only ever turns an error into the one
outcome that could have been meant; it never moves a site that was already
working. The second means it is not a guess: no public authority will issue for
those names, so a certificate from the built-in authority is the only one that
could exist. A public name in the same position is refused at compile time and
told which of the three ways to get a certificate is missing, because nothing
this process could sign would be accepted for it.

`gatel explain` says so on the site line, and `gatel trust` is still a separate
command: a certificate appearing without being asked for is a convenience, and
changing what every program on the machine trusts is not.

## Whose error is it

`error-pages` replaces the body of an error the gateway produced, and leaves
alone one a backend answered with. A 404 from a backend is very often a JSON
document an application is waiting for, and replacing it with a page would break
the client to make the browser look nicer.

Telling them apart needs a fact only the gateway has, so every refusal it makes
marks the response as its own: a route that matched nothing, a guard that said
no, a backend that could not be reached, a body over the limit. A directive that
forgets the mark costs a custom page, not correctness. `all=#true` covers
everything for the cases where that is genuinely wanted.

## Limits that bite

Every field in `global { limits ... }` maps to something a connection actually
experiences, because a bound that is written down and not applied is worse than
one that was never offered.

| | What it bounds | How |
|---|---|---|
| `header` | the request head | hyper's read buffer for HTTP/1, `max_header_list_size` for HTTP/2 |
| `body` | the request body | refused before reading when the length is declared, cut off mid-stream when it is not |
| `handshake-timeout` | a TLS handshake | a deadline around the accept |
| `idle` | HTTP protocol detection and a connection with no active request | a connection activity deadline, plus HTTP/1 header read timeout |
| `connections` | admitted work across the whole process | FIFO admission after accept; held through the connection and any upgrade |

`idle` covers two things with one number on purpose. A keep-alive connection
waiting for its next request and a client dribbling a head out one byte at a
time are the same thing seen from the socket, and separating them would mean
knowing which the peer intends. The activity deadline also covers partial H2
prefaces, SETTINGS-only peers and unfinished initial headers. Completed headers
start request activity, which continues through the response body or cancellation.
Active requests retain their own deadlines; another incomplete H2 stream does
not yet have an independent header deadline while one stream remains active.

An idle listener does not reserve a connection permit. Each accept loop holds at
most one accepted, unhandshaken socket while waiting in the shared FIFO queue;
the remaining traffic stays in the backlog. This adds at most one pending file
descriptor per listener beyond admitted work. Lowering the cap does not close
active connections: returned permits consume the outstanding shrink debt before
new work is admitted, including when an assigned waiter is cancelled.

## What may be tried again

Every retry is a guess that the backend did not act on the request. The
configuration therefore describes *when the failure happened*, not what it was,
because that is the only thing that makes the guess safe:

| | Retried | Why |
|---|---|---|
| Nothing eligible, no connection, queue full or timed out | any method, any body | the request never left, so the backend cannot have seen it — this is `mode="safe"` |
| The send failed, or no response head arrived | only under `mode="replay"`, and only when the body was buffered or absent | the backend may have acted on it, so this can duplicate a request; the configuration refuses to enable it without a `buffer` size, so the cost is stated |
| An upgrade | never | the client committed to one connection the moment it asked |

Two bounds sit on top. `per-try` gives each attempt a slice of the overall
budget, so one endpoint that accepts and then goes quiet cannot consume the
time the next attempt would have used. `budget-percent` caps retries as a
share of the upstream's live traffic, because the moment a backend starts
failing every request in flight fails at once — and a gateway that retried all
of them would answer a partial outage by doubling the load on what is left.
A budget of zero turns retries off rather than rounding up to the small floor
that keeps a quiet gateway able to retry at all.

## Why `proxy` is in the kernel

Endpoint selection, health, connection reuse and retry are one correctness
chain. A plugin able to replace any single link could break the others — a
balancer that could pick an ejected endpoint, or a pool that could resurrect a
draining one. Directives contribute middleware and terminals that need nothing
but a request and a response; the proxy needs the whole chain, so it is not a
directive.

## Asking a backend without becoming one

`proxy` being in the kernel leaves a real problem behind: `forward-auth` has to
reach a backend, and it is a directive. The two shortcuts are both bad. Giving
the directive its own HTTP client means a second dialling path with its own
timeouts, no connection reuse, no health and no TLS configuration — a copy of
the thing the paragraph above exists to prevent. Moving the directive into the
kernel means the kernel grows every time something needs a round trip.

So the seam is a capability, not a client, and it is deliberately narrow:

| | Where | What it says |
|---|---|---|
| `Ask` / `Answer` / `Upstreams` | `gatel-plan/src/subrequest.rs` | *ask one declared upstream one question about this request*, in the vocabulary of a method, a path and headers |
| the implementation | `gatel-state/src/ask.rs` | the same selector, pool, health, timeouts and retry a proxied request uses, because it is the same backend |
| `BuildContext::upstreams` | handed over when a handler is built | so the set a directive can reach is fixed at build time, not reachable from a request |
| `DirectiveConfig::upstreams()` | checked while compiling | so a name with a typo in it is a diagnostic with a position, not a 502 the first time somebody is refused |

A question carries no body and an answer carries at most 64 KiB of one, read to
the end before the call returns. That bound is what keeps this from quietly
becoming a second way to proxy: it is enough for the page an identity service
writes when it refuses somebody — which is the whole point, since a refusal is
useless without its `Location` — and nowhere near enough to stream a response
through. Nothing about a subrequest outlives it, so a directive cannot hold a
connection.

The state model has the same shape and for the same reason: a directive names
what it needs and the kernel decides how it is satisfied. Neither is an
extension point that lets a directive reach past the kernel; both are the
kernel offering something it already owns.

## Opened file representations

Static-file selection resolves the identity/index/precompressed path inside the
configured root, then creates one `ranges::OpenedFile`. Its metadata is obtained
from that handle and drives validators, preconditions, HEAD length and ranges.
Body delivery consumes the same owner instead of reopening its pathname. A
deployment that replaces the directory entry cannot switch the response body to
the replacement after its validators have been chosen.

Responses of at most 64 KiB starting at offset zero transfer the already-opened
idle Tokio file to a standard file handle. One blocking task reads exactly the
selected length into the outgoing buffer, avoiding a separate file read buffer.
This does not reopen the pathname. Nonzero ranges retain the Tokio seek/read
path. Larger responses retain the Tokio reader and 64 KiB response chunks; the
eager limit is unchanged.
Cancellation does not assume a started blocking read can be aborted.

An exact-length reader bounds growth and reports premature EOF as an error for
both buffered and streaming bodies. This prevents a truncated stream from looking
like a successfully completed cache fill. A read failure before body delivery
becomes a non-cacheable error representation with the old framing and validators
removed; a streaming failure propagates through the response body.

This is not a filesystem snapshot or a complete sandbox. In-place writes can
still change the opened file, and path containment is checked before opening.
Protection against hostile concurrent directory/symlink replacement requires
platform-specific, handle-relative resolution that is not implemented yet.

## Plugin retirement ownership

Before assembling listener services, the host prepares each directive owner
once. `PreparedDirective` returns its handler and inert background factories as
one resource bundle; inherited routes and listeners share that handler, rather
than independently rebuilding resources. Terminal owners are route-specific,
even when two routes use the same terminal name and configuration. Conflicting
configurations under one owner reject preparation rather than silently aliasing.

Preparation reserves a slot in the serving `Server`'s `Retirements` registry
for each generation with background tasks. `Resources.plugin_retirements` must
refer to that same server's registry. The registry is bounded to 64 generations;
a full registry rejects preparation and preserves the last good configuration.
Empty task sets consume no slot, and discarded candidates start no cleanup jobs.

`Runtime` owns a `PluginTasks` lifetime token, while the registry owns only its
`TaskScope`. Releasing the final runtime owner submits cooperative cleanup to
the registry without retaining the old route table or committed generation.
The reserved slot remains charged until cleanup completes. This is asynchronous
task retirement, not an OS process-tree supervisor.

After connection drain, `Server::shutdown` seals registration, concurrently
shuts down all registered scopes (including externally pinned generations), and
joins retirement jobs. Cancellation preserves join ownership for a retry.
Explicit shutdown is required: dropping the host or its Tokio runtime is an
emergency cancellation path, not a guarantee that asynchronous cleanup ran.

Listener join handles remain in `Server` while shutdown awaits connection drain.
Cancelling shutdown therefore preserves the work for a retry; it does not move
the handles into a temporary vector that can detach them. Once shutdown starts,
`sync` cannot reactivate listeners or plugin tasks. Emergency `Server` drop
aborts both active and retiring accept loops, whose connection sets are owned
by those loops. It does not perform graceful or asynchronous plugin cleanup.

## What is deliberately absent

- **No `tls-skip-verify`.** Development uses the built-in CA instead:
  `tls internal` issues short-lived certificates from an authority generated on
  first use, and `gatel trust` installs its root — as its own command, because
  adding a trust anchor changes what every program on the machine believes.
- **No `tls-skip-verify` for backends either.** An upstream that speaks TLS is
  verified, and a private authority is named with `ca`. The option exists in
  other gateways because it makes a broken deployment work today, and it is how
  deployments stay broken for years.
- **No secret material in configuration.** `env:` and `file:` references only,
  and `SecretString` is redacted in every rendering path.
- **No forwarding header believed by default.** `trusted-proxy` decides, and the
  canonical client address is computed once, where that policy lives.
- **No undocumented event.** Every structured event is in `docs/EVENTS.md`, and
  `cargo run -p xtask` fails if the code and the list disagree in either
  direction. An event name is an interface: something is alerting on it.
- **No silent fallback.** An unknown node, property or reference is an error
  with a position and, where possible, a suggestion.
