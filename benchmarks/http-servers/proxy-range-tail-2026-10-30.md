# Proxy reuse, offset Range and plaintext bulk-file output — 2026-10-30

The proxy fix addresses two reproduced HTTP/1 connection-pool races without
replaying dispatched requests. The offset-Range change substantially improves
the tested 64 KiB selection at a nonzero offset. Linux bulk-file backpressure
is restricted to plaintext HTTP/1.1; TLS retains its original TCP transport.

**The bulk policy has a latency tradeoff:** in the final combined run, 20 MiB
p99 increased 14.80%, with non-overlapping sample ranges. Its p99.99 changed
+3.45% with overlapping ranges, so that run does not establish a 20 MiB extreme
tail improvement. All unfavorable samples and rejected candidates are retained
below. The separate final-binary 10 MiB long run reduced reported p99.99 by
58.02% and p99.9 by 37.98%, with non-overlapping ranges, but reduced RPS by
5.04%, also with non-overlapping ranges. Main had five completed responses
beyond the five-second histogram limit; the candidate had zero in that run.
This is an explicit throughput/extreme-tail tradeoff, not a claim that every
large-file percentile improves or that throughput is unchanged.

The final runtime source passed all 25 CI jobs. The 32 plaintext and 60 protocol
windows completed 3,049,860 and 2,471,683 responses respectively without errors.
A separate, longer sixteen-window H1 TLS check completed 2,883,682 responses
without errors and did not repeat the short profile's small-file throughput
losses. Different hosts/profiles remain separate throughout this report.


## Proxy failure: evidence and fix

The [post-107 benchmark](https://github.com/salvo-rs/gatel/actions/runs/40826946141)
recorded one 502 among 1,172,051 proxied responses. Its four-millisecond
`operation was canceled` log omitted Hyper's source error. That message alone
cannot uniquely identify the historical failure; an earlier A/B also reproduced
a baseline 502.

Two deterministic tests fail on main's original pool: cancel a dispatched POST
and immediately drop its lease before Hyper notices cancellation, and attempt
another send while a previous response is still completing. The first parked
an in-flight sender; the second could cancel because an open sender was not
dispatcher-ready.

An exclusive send now removes reuse eligibility before awaiting readiness or
the response head. Only success restores its **previous** reuse policy.
Checkout waits for idle HTTP/1 readiness, discards closed senders, and shares
one connect budget across stale checks and replacement dialing. Elapsed
subtraction handles even `Duration::MAX` without an overflowing absolute
deadline. Cancellation and timeout release the reservation. Dispatched requests
are never replayed, and logs retain Hyper's underlying cause.

The integration regression completes 1,024 mixed GET/POST exchanges across
backend close boundaries and checks that the backend saw exactly 1,024
requests. These changes fix the reproduced cancellation/readiness class;
finite error-free load tests cannot eliminate every possible future 502 cause.

## File delivery and transport scope

Nonzero-offset responses of at most 64 KiB seek and read their exact selected
length in one blocking task. They retain the original opened file descriptor
and existing bounded 32/64 KiB buffer classes. Larger bodies retain main's
portable Tokio reader and 64 KiB frames; `file_body.rs` is identical to main.

For selected file bodies of at least 8 MiB, the file handler can emit a
zero-sized `BoundedFileOutput` response marker. The native Linux host honors
it **only for plaintext HTTP/1.1**, setting a 64 KiB `TCP_NOTSENT_LOWAT`
threshold. This applies unsent-queue backpressure separately from `SO_SNDBUF`
auto-tuning and bytes in flight. Before an unmarked response writes, the host
restores its original socket value, including zero (inherit the namespace).
H2, HTTP/1.0, smaller selected bodies and unsupported hosts keep their prior
socket policy.

**TLS keeps its original raw TCP transport type.** It disables this optional
policy before probing socket options, creates no connection hint, and unwraps
the raw stream before passing it to `TlsAcceptor`. The policy is not inserted
below the TLS layer. The regression test checks raw IO, nodelay and preservation
of a custom original low-water value.

The private hint owns no request, upgrade task or `ConnectionWork`. Its owner
is shared through the existing handler `Arc`, avoiding a separate hint clone
per request. The 8 MiB threshold is an empirical scope, not a universal optimum.

## Earlier candidates and scope decisions

Candidate `57574c9` passed all 25 CI jobs and reported no errors across its full
matrix, independent file checks, threshold checks and protocol controls. It
nevertheless repeatedly raised 8 MiB−1 p99.99 means, even though that body did
not enable the new queue threshold:

| Separate VM/run | 8 MiB−1 p99.99 mean change | Context |
| --- | ---: | --- |
| [Full matrix](https://github.com/salvo-rs/gatel/actions/runs/40920774560) | +35.26% | Three ten-second samples per binary; wide overlapping ranges. |
| [Threshold replication](https://github.com/salvo-rs/gatel/actions/runs/40931205357) | +35.98% | Three sixty-second samples per binary; wide overlapping ranges. |
| [Long symmetric replication](https://github.com/salvo-rs/gatel/actions/runs/40934778646) | +37.62% | Four 120-second samples per binary, ABBA BAAB; overlapping ranges. |

Overlapping ranges alone did not justify dismissing a repeated signal. Review
found an unnecessary hint `Arc` clone/drop in every request. Sharing the hint
through the existing handler owner removes that work and reduces the request
future's ownership state. The only runtime source difference from `57574c9`
is seven changed lines in `serve.rs` (three additions, four removals).

The [three-arm screen](https://github.com/salvo-rs/gatel/actions/runs/40938928365)
compared main, `57574c9`, and the shared-owner candidate in the palindromic
order A B C C B A on one EPYC 9V74 VM. It used exact production binaries,
the same compiler/lock, default client buffers and five-second limit. Twelve
windows completed 666,262 responses without errors. Each binary had two
120-second 8 MiB−1 windows and two twenty-second 1 MiB controls:

| Binary | 8 MiB−1 p99.99 samples, ms |
| --- | --- |
| Main | 2587.681, 1820.961 |
| Before sharing the hint | 2035.122, 3208.925 |
| Shared-owner candidate | 736.827, 790.253 |

The candidate's p99.99 mean was 65.4% below main, with non-overlapping ranges;
RPS changed −1.36%. The 1 MiB control changed −2.92% RPS / −4.05% p99.
Two samples are a screen, not acceptance. These are historical screening results of `f0bee483`, not final-source
acceptance data. The result supports removing the
extra ownership work; it does not uniquely separate atomic-operation cost,
future layout, scheduling and TCP-flow-control effects.


The [full `f0bee483` run](https://github.com/salvo-rs/gatel/actions/runs/40942637523)
completed 144 wrk windows (19,256,061 responses) and 36 protocol controls
(1,437,136 responses), with no errors. It was still **not accepted**: 10 MiB
TLS throughput fell 5.14% for H2 and 5.77% for H1, both with non-overlapping
two-sample ranges. The final change restores TLS's original raw TCP IO type
and restricts the new policy to plaintext H1, rather than dismissing those
protocol regressions.

## Final build identity and validation method

Baseline is main `f11ffccdaaae6249c1f779160b5b84af1f479ae6`.
Final source is `2df69fb23efc98812aef15c3b0d4c40d72103a5c`.
Final implementation commits preserve these runtime sources, Cargo.lock,
Dockerfile and architecture checks. One-off orchestration remains outside the
product diff, on the linked benchmark branches.

Both binaries use the production Dockerfile, identical locked dependencies
and `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, musl, LLVM 23.1.1. The baseline
was reproduced byte for byte before reuse; the final candidate was freshly
production-built. Local audits verify the archived `serve.rs`/socket sources,
both binaries, compiler/lock/Dockerfile hashes and every raw measurement.

| Binary | SHA-256 |
| --- | --- |
| Main | `8073496001314e76044a7f5d8f98549f1ff11b96934b3bcd0412a727c5a05c71` |
| Final plaintext-only policy / native TLS | `cf6ad2465a12ee54e813522fb30d9f5a5a1e18aec5d55187236046757b75d3e1` |

The [final same-host run](https://github.com/salvo-rs/gatel/actions/runs/40956117048)
rechecks plaintext after the TLS scope change: ABBA, two samples each of
4 KiB, 64 KiB, 64 KiB+1, nonzero-offset 64 KiB Range, 1 MiB, 4 MiB,
8 MiB−1 and 20 MiB. The first six run ten seconds, 8 MiB−1 runs 120 seconds,
and 20 MiB ninety seconds, all after three seconds warmup. Wrk uses four
threads and 128 connections.

On the same VM, H2c uses ABBA (two samples each); H2 TLS and H1 TLS use
ABBA BAAB (four samples each). Each mode checks 64 KiB, 1 MiB and 10 MiB:
ten/ten/thirty-second h2load windows after three seconds warmup. H2 uses eight
clients × sixteen streams, H1 TLS 128 clients × one stream, four threads and
a five-second inactivity timeout. Exact-byte checks verify every payload.
These sixty protocol windows specifically recheck the rejected TLS regressions.

The default client remains `tcp_rmem = 4096 131072 33554432`; no namespace
tuning, altered timeout or source patch is used to improve the score. All raw
outputs, errors, exact-byte/status/Range preflights and environment remain in
the artifact; plaintext windows also record server CPU/RSS and client socket
memory/TCP counters. Counts from separate machines or source revisions are
never pooled.

The independent H1 TLS artifact reuses the verified build manifest, whose
`phase_order`/`scenarios` fields still describe an earlier workload. Its actual
protocol script, sixteen raw outputs and `protocol-controls/comparison.json`
establish the reported ABBA BAAB order and two payload sizes; the local audit
checks that order explicitly. Build identity is verified from binary hashes.
The 10 MiB run's stock per-phase Markdown timing captions also retain earlier
duration text; the actual raw/resource durations are 120 seconds and are
checked against that value by the local audit.

Percent changes compare arithmetic means of per-window statistics, not pooled
latency quantiles. Min/max ranges are descriptive, not confidence intervals.
A control RPS loss above 5% or p99 increase above 10% with non-overlapping
ranges triggers investigation. Higher-quantile signals are retained and checked
with longer isolated measurements or negative controls.

Wrk 4.2.0's done callback observes its
[corrected histogram](https://github.com/wg/wrk/blob/4.2.0/src/wrk.c#L151-L181).
Completed responses beyond its unchanged five-second histogram limit increment
the timeout count and are omitted from quantiles; unfinished requests at window
end are also absent. These are distinct from connect/read/write failures and
h2load's inactivity-timeout semantics. Retain all slow completions when judging
tail behavior.

## Final-source measurements

The final run completed 32 plaintext windows / 32 preflights / 3,049,860 responses, followed on the same VM by 60 protocol windows / 60 exact-byte checks / 2,471,683 responses. Candidate HTTP/socket/over-limit errors were zero in these final validation windows.

Host: Model name:                              AMD EPYC 7763 64-Core Processor. Samples remain separate from all prior machines/source revisions.

### Plaintext controls

| Scenario | Main RPS | Candidate RPS | RPS change | p99 change | p99.9 change | p99.99 change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| static-4k | 22344.46 | 22846.69 | +2.25% | -1.90% | -0.66% | +1.44% |
| static-64k | 17765.92 | 17922.30 | +0.88% | -3.16% | -12.45% | -34.75% |
| static-64k-plus1 | 13765.72 | 14090.74 | +2.36% | -3.60% | +0.93% | +21.85% |
| range-offset-10m | 9265.78 | 18042.85 | +94.73% | -56.55% | -47.99% | -37.41% |
| static-1m | 2225.51 | 2324.36 | +4.44% | -1.98% | +2.20% | -1.73% |
| static-4m | 631.74 | 633.66 | +0.30% | -12.84% | -5.25% | +18.70% |
| static-8m-minus1 | 312.79 | 317.44 | +1.49% | -1.92% | -53.62% | -53.08% |
| static-20m | 129.64 | 130.00 | +0.28% | +14.80% | -1.45% | +3.45% |

### Final-binary ten MiB long windows

The [additional final-binary plaintext run](https://github.com/salvo-rs/gatel/actions/runs/40965504941) uses the same verified pair for six ABBAAB 120-second 10 MiB windows after ten seconds warmup, three per binary, with four threads/128 connections and the default client/five-second histogram limit. It completed 185,939 responses; errors are retained below. These VM samples are separate from the main run.

| Case | RPS change | p99 change | p99.9 change | p99.99 change |
| --- | ---: | ---: | ---: | ---: |
| static-10m | -5.04% | -1.86% | -37.98% | -58.02% |

`range-offset-10m` selects 64 KiB at offset 1 MiB in the 10 MiB fixture;
the response is 64 KiB, not 10 MiB.

### Retained error windows

| Phase / binary | Case | HTTP errors | Socket errors including over-limit | Over-limit |
| --- | --- | ---: | ---: | ---: |
| Combined-run 32 plaintext windows | All | 0 | 0 | 0 |
| Long 10 MiB 5 / base | static-10m | 0 | 5 | 5 |

### Protocol controls

| Mode / payload | RPS change | Main samples | Candidate samples | Sample ranges overlap |
| --- | ---: | --- | --- | --- |
| h2c / 64k | +6.84% | 13434.90, 12962.60 | 14160.10, 14042.80 | no |
| h2c / 1m | +4.98% | 1529.70, 1445.90 | 1582.70, 1541.10 | no |
| h2c / 10m | +1.53% | 173.43, 162.83 | 177.47, 163.93 | yes |
| h2-tls / 64k | +8.96% | 7524.20, 9907.10, 7273.30, 9323.50 | 7578.70, 9711.10, 10211.50, 9574.20 | yes |
| h2-tls / 1m | -0.90% | 996.70, 954.50, 1049.10, 1013.40 | 997.20, 976.20, 978.50, 1025.80 | yes |
| h2-tls / 10m | +0.05% | 118.50, 117.30, 120.63, 120.97 | 118.57, 120.97, 117.03, 121.07 | yes |
| h1-tls / 64k | -7.56% | 10431.80, 10441.80, 12218.50, 12294.80 | 10661.70, 10646.40, 10442.10, 10203.20 | yes |
| h1-tls / 1m | -10.03% | 1259.20, 1340.90, 1549.00, 1522.20 | 1339.40, 1300.80, 1186.20, 1276.30 | yes |
| h1-tls / 10m | -0.85% | 179.13, 183.30, 185.13, 182.40 | 179.23, 182.73, 184.20, 177.57 | yes |

### Independent longer H1 TLS small-file check

The short combined run's H1 TLS 64 KiB/1 MiB means were unfavorable (−7.56% / −10.03%) despite overlapping sample ranges. A [separate check](https://github.com/salvo-rs/gatel/actions/runs/40963588810) retained the exact same final binaries and measured sixteen ABBA BAAB windows: thirty seconds per payload after ten seconds warmup, four samples per binary, same 128 clients/four threads. It completed 2,883,682 responses without errors. This is a longer profile, not a replacement for the original short samples.

| H1 TLS payload | RPS change | Main samples | Candidate samples |
| --- | ---: | --- | --- |
| 64k | -0.26% | 10285.30, 10263.33, 10324.03, 12249.17 | 10261.10, 12075.90, 10266.33, 10407.90 |
| 1m | -0.24% | 1153.37, 1158.97, 1166.67, 1521.73 | 1136.63, 1518.37, 1175.63, 1158.30 |

### Plaintext resource observations

CPU values are mean sampled server cores; memory values are maxima across
the listed windows. This is not a general client-memory reduction.

| Case / binary | Server CPU cores | Peak server RSS MiB | Peak client socket MiB |
| --- | ---: | ---: | ---: |
| static-1m / base | 2.74 | 31.4 | 30.4 |
| static-1m / candidate | 2.72 | 32.1 | 33.9 |
| static-4m / base | 2.62 | 31.8 | 119.2 |
| static-4m / candidate | 2.55 | 32.4 | 140.3 |
| static-8m-minus1 / base | 2.76 | 32.3 | 601.6 |
| static-8m-minus1 / candidate | 2.76 | 32.8 | 666.9 |
| static-20m / base | 2.71 | 32.8 | 742.2 |
| static-20m / candidate | 2.74 | 33.7 | 931.8 |

### Assessment and remaining costs

The final 10 MiB check completed 185,939 responses. Its only error counter was
five over-limit completions in main's fifth phase: connect/read/write and HTTP
status errors were zero for both binaries. Those five completions are absent
from main's corrected quantiles. Thus the table compares reported histogram
statistics, not uncensored end-to-end distributions.

| 10 MiB sample | Main RPS | Candidate RPS | Main p99.99 ms | Candidate p99.99 ms |
| --- | ---: | ---: | ---: | ---: |
| First per binary | 259.55 | 252.75 | 2220.954 | 1194.214 |
| Second per binary | 265.39 | 243.38 | 1606.295 | 1009.916 |
| Third per binary | 269.26 | 258.04 | 4205.763 | 1168.172 |

These rows list observations in each binary's phase order, not independent
paired trials. Three samples per binary remain limited evidence. The RPS
loss of 5.04% and the separate 20 MiB p99 increase of 14.80% are material costs
of the proposed bulk policy. Neither is dismissed as noise or counted as an
unaffected-control pass. The 10 MiB result supports proposing this scoped
policy for extreme-tail-sensitive downloads; throughput-sensitive deployment
needs to weigh that tradeoff. This proposal has **not** demonstrated a uniform
improvement over main.

The final-source unaffected controls did not establish a persistent regression
under the stated descriptive guards. Original unfavorable short TLS means and
higher plaintext quantiles remain visible; the longer TLS check uses different
duration/warmup and does not erase them. The former full matrix, including
proxy throughput controls, belongs to historical `f0bee483`; it is not relabeled
as a final-binary full-matrix run. Final-source functional proxy coverage is
provided by the cancellation/readiness regressions and the 1,024-exchange test.

Server RSS remains small, but this is not evidence of general client-memory
savings: the separate 10 MiB run peaked at about 704 MiB client socket memory
for main and 760 MiB for the candidate. The new threshold bounds the server's
unsent queue, not the receiver's total queue or memory pressure.

The source is frozen after these checks. Retain the proxy and offset-Range
fixes independently when assessing whether the bulk-policy tradeoff fits the
project's latency and throughput priorities. Historical slow completions,
finite final samples and censored histograms preclude a universal five-second
completion guarantee.


## Historical long-window and measurement controls

The preceding `f0bee483` candidate had the same plaintext queue algorithm but
still wrapped TLS. Its [independent long run](https://github.com/salvo-rs/gatel/actions/runs/40942645896)
recorded one completed 20 MiB response beyond the five-second histogram limit,
versus 30 for main, and therefore **failed its strict zero-over-limit guard**.
Connect/read/write/HTTP-status errors were zero. This event remains evidence
against a universal completion-time guarantee; the TLS scope fix did not
change the plaintext policy and must not be presented as eliminating every
possible slow completion.

That run's 8 MiB−1 p99.99 mean decreased 49.2% with overlapping ranges,
whereas the preceding short matrix had increased 54.3%. For 1 MiB, mixed-sequence
short/long means increased about 29%/34%. An
[isolated fresh-server control](https://github.com/salvo-rs/gatel/actions/runs/40951457272)
measured only 1 MiB in eight sixty-second windows after ten seconds warmup,
four per binary: 1,105,678 responses, no errors, RPS +0.89%, p99 −0.35%,
p99.99 +0.19%, all with overlapping ranges. This did not repeat the steady
slowdown, but does not erase the original mixed-sequence observations.

The [identical-main A/A control](https://github.com/salvo-rs/gatel/actions/runs/40947333296)
executed main's exact `80734960` binary/configuration in both groups: forty
ABBA BAAB windows, 2,260,200 responses, no actual IO/status errors. Its
4 MiB / 8 MiB−1 p99.99 means differed by about 40% / 44%, with overlapping
ranges; 20 MiB over-limit counts were ten versus zero. This demonstrates
material variability in that setup, not a candidate improvement or proof that
all A/B differences are noise. It is kept separate from final-source validation.

## Root-cause limits and rejected alternatives

The [body-timing diagnostic](https://github.com/salvo-rs/gatel/actions/runs/40903262584)
separated producer wait from the time between emitting a frame and the next
body poll. Captured slow 20 MiB bodies, including warmups, spent about 90–93%
of body-emission time in the consumer gap. This narrows the dominant stall
phase to downstream backpressure or connection scheduling, rather than just
disk reading. It is not final wire completion or proof of a particular TCP bug.
The probe itself changed the tail: the uninstrumented experimental reader
recorded 41/45 over-limit completions, whereas instrumented measurement
windows had none. That apparent improvement is not acceptance evidence.

Aggregate memory-pressure, drop, retransmission and zero-window counters are
not per-request causal traces. Zero `TCPTimeouts` does not exclude flow-control
stalls: Linux handles a
[zero-window retransmission](https://github.com/torvalds/linux/blob/v7.0/net/ipv4/tcp_timer.c#L561)
before the ordinary counter path. The
[TCP documentation](https://docs.kernel.org/networking/ip-sysctl.html)
distinguishes unsent-queue backpressure from a total send-buffer cap. Client
kernel socket memory and pressure can remain large despite a small server RSS.

These experiments use separate source revisions/hosts and are not pooled with
the final candidate. Their failed windows remain in their artifacts:

| Experiment | Rejection evidence |
| --- | --- |
| [Direct 64 KiB reads](https://github.com/salvo-rs/gatel/actions/runs/40838324642) | Nine candidate over-limit completions; extended 10 MiB p99.9/p99.99 worsened 114%/164%. |
| [Yielding / 32 KiB frames](https://github.com/salvo-rs/gatel/actions/runs/40848606127) | Yielding still worsened 10 MiB p99.99; smaller frames lost 1 MiB throughput and worsened 10 MiB p99. |
| [Global socket policy plus direct reads](https://github.com/salvo-rs/gatel/actions/runs/40864156611) | 108 error-free windows, but proxy 10 MiB RPS fell 9.5% and 4 MiB p99 rose 12.3%. A later protocol-fixture package error was separate from the completed measurements. |
| [Sixteen concurrent direct reads](https://github.com/salvo-rs/gatel/actions/runs/40894667409) | Independent 20 MiB samples recorded 19/4/1 over-limit completions, versus baseline zero. |
| [Cooperative byte budgets](https://github.com/salvo-rs/gatel/actions/runs/40900992649) | One budget produced 88 over-limit 20 MiB completions; another avoided errors but worsened 10 MiB p99.99 by 125%. |
| [128/256 KiB read batches](https://github.com/salvo-rs/gatel/actions/runs/40909208303) | 10 MiB RPS rose 23–26%, but p99.99 worsened 147%/82% with non-overlapping ranges. |

The separate [same-binary mechanism check](https://github.com/salvo-rs/gatel/actions/runs/40927262932)
compared the old `57574c9` binary with itself. Isolated diagnostic seccomp
profiles either allowed the selected `TCP_NOTSENT_LOWAT` setter or returned
success without executing that syscall, preserving the software policy path.
Smoke tests first verified getter/successful-setter/getter as `0→0` versus
`0→65536`. Both profiles otherwise shared a default-allow diagnostic policy;
they are not production security settings. Eight 90-second windows completed
133,553 responses without errors. Real-policy 20 MiB p99.99 was 8.8% lower
and p99 4.5% higher, with non-overlapping ranges. This supports a kernel-policy
effect, but is not final-source acceptance data or a universal improvement.

## Lessons from nginx 1.27.5

The benchmark's `nginx:1.27` image reports **1.27.5**. Source review used that
exact release, commit `6ac8b69f06bc10d5503f636da888fa70095b151c`:

| Mechanism | Source and implication |
| --- | --- |
| File-backed buffers | The [static module](https://github.com/nginx/nginx/blob/6ac8b69f06bc10d5503f636da888fa70095b151c/src/http/modules/ngx_http_static_module.c#L261) retains the descriptor and file offsets instead of always materializing byte frames. |
| Offset-aware Range delivery | The [Range filter](https://github.com/nginx/nginx/blob/6ac8b69f06bc10d5503f636da888fa70095b151c/src/http/modules/ngx_http_range_filter_module.c#L778) adjusts `file_pos`/`file_last`. |
| Kernel file-to-socket transfer | The [Linux sendfile chain](https://github.com/nginx/nginx/blob/6ac8b69f06bc10d5503f636da888fa70095b151c/src/os/unix/ngx_linux_sendfile_chain.c#L261) supplies an explicit offset and handles short writes, EAGAIN and premature EOF. |
| Bounded event-loop work | The [write filter](https://github.com/nginx/nginx/blob/6ac8b69f06bc10d5503f636da888fa70095b151c/src/http/ngx_http_write_filter_module.c#L291) limits one send and [posts remaining ready work to the next event iteration](https://github.com/nginx/nginx/blob/6ac8b69f06bc10d5503f636da888fa70095b151c/src/http/ngx_http_write_filter_module.c#L335). The [default sendfile chunk limit](https://github.com/nginx/nginx/blob/6ac8b69f06bc10d5503f636da888fa70095b151c/src/http/ngx_http_core_module.c#L3827) is 2 MiB. |

The benchmark enables nginx `sendfile`; it does not enable `aio`,
`open_file_cache` or `tcp_nopush`. Those cannot explain this reference result.
Preserving file identity/offsets, honoring short writes and bounding work under
backpressure are useful principles. The current unsent-queue threshold is a
different mechanism from nginx's 2 MiB event-loop limit.

Native sendfile needs a file-region-aware host/HTTP transport contract: the
current Salvo/Hyper body exposes `Bytes`, not descriptors and regions. Bypassing
the dispatcher would risk framing, TLS, upgrades and cancellation. That
architectural work needs separate plaintext/TLS/H2, cold-file, slow-client and
nonzero-offset validation before adoption.

A follow-up should first profile copies, read/write syscalls and CPU, then
prototype descriptor/offset regions behind that contract. Fairness budgets
should count actual transport progress rather than adding a yield to every
body frame. Preserve the published colocated baseline, and also measure with
a separate load-generator host to distinguish receiver pressure from server
cost. `sendfile` can reduce copying; it does not by itself prove that the
observed downstream stalls disappear.

## Regression checks

- Pool cancellation before driver progress, busy-dispatcher readiness, idle
  closure, bounded/shared connect budgets, huge durations, permit release,
  already-poisoned leases and underlying error text.
- Exact bytes and headers at 32/64 KiB selected lengths and nonzero offsets;
  original-handle ownership, pathname replacement, truncation and growth;
  output-marker boundaries at 8 MiB−1 / 8 MiB / 8 MiB+1.
- File → proxy → file HTTP/1 pipeline; cache/encoding through 8 MiB+17; H2
  whole files and boundary Ranges; existing H2 proxy and upgrade cancellation.
- Linux original low-water values zero/256 KiB restored across repeated
  file/non-file transitions; unchanged send-buffer size; ordinary/vectored IO
  and shutdown passthrough; marker survival through response conversion;
  no retained upgrade ownership; disabled-policy raw TCP preserves nodelay
  and a custom low-water value for TLS. Non-Linux builds exercise passthrough.

The source passed 134 state / 311 standard-bundle tests; the final native-TLS
change reran 78 HTTP library tests, three file-buffer tests, ten proxy tests,
two H2-body tests and six upgrade tests. Strict all-target Clippy and formatting
checks passed, as did architecture layer checks. The existing unrelated
generation-file size advisory remains unchanged.

[CI on the final runtime source](https://github.com/salvo-rs/gatel/actions/runs/40956238039)
passed all 25 jobs on the first attempt, including the platform matrix and
Linux socket-option tests. Local build products/caches used D:; Docker builds
and load tests ran only on temporary remote runners.

These finite colocated-runner observations do not establish a universal
production latency guarantee. Retain unfavorable samples, errors, resource
costs and the documented scope when assessing the change.
