# HTTP server benchmark

Compares `gatel` with `nginx` and `caddy` under two workloads they can all do
identically: serving a file from disk, and proxying to a shared origin.

Everything runs in containers, including the load generator, so the same
topology can be repeated on another machine without host-specific setup — and
so that `wrk` competes with every server for CPU in the same way.

## What it measures, and what it does not

It measures three servers against each other, on one machine, at one moment.
That is the only kind of performance number worth writing down, and it is still
weaker than it looks: a container on a laptop is not a server under production
traffic, and the ordering here can change with the host, the kernel, the file
system and how many other things are running.

So the report is for deciding whether a change made this project faster or
slower than it was, and for noticing when it is wildly out of line with the
other two. It is not a claim about anybody else's deployment, and nothing in
this repository quotes a number from it.

Performance comparisons and throughput thresholds are deliberately not CI
gates. A shared runner's throughput moves by tens of
percent between runs for reasons that have nothing to do with the commit, so a
threshold there would either never fire or fire constantly — and both teach
people to ignore it. Run this on a machine that is otherwise idle, twice, and
believe it only if the two runs agree. CI runs only harness regressions and a
short nginx/Caddy smoke check of payloads, range controls and result parsing;
it does not compare performance numbers against a threshold.

The gateway is built from the repository's own `Dockerfile` — the image that
would actually be published — rather than from a benchmark-only build. Measuring
something compiled differently from what people run would say more about the
build than about the gateway.

## Requirements

- Docker with Compose
- Python 3.11 or newer

## Running it

```bash
python benchmarks/http-servers/run.py
```

The first run builds three images and generates the files under test, so it
takes a while. Later runs reuse both.

Useful arguments:

```text
--rounds 3            rounds per target
--duration 10         seconds per round
--threads 4           wrk threads
--connections 128     concurrent connections
--warmup 3            seconds of warmup per target
--scenario proxy      run only this scenario (repeatable)
--target gatel        measure only this target (repeatable)
--no-build            reuse prebuilt images; never implicitly build missing images
--output-dir PATH     write results somewhere other than the default
```

Results land in `target/benchmarks/http-servers/<timestamp>/`:

- `report.md` — the comparison, per scenario
- `summary.json` — every round, machine-readable
- `raw/*.txt` — what `wrk` actually printed
- `payload-checks.json` — status, exact bytes and range headers checked before load
- `containers.log` — service logs, captured before cleanup even on failure

Container cleanup still runs if collecting or saving the service logs fails.

Each completed round is saved immediately. Reported p90/p99 values are means
of the individual rounds' percentiles, not percentiles of pooled requests.
Warmup uses the measured concurrency and Range script, including nonzero offsets.
Ratios are `N/A` when Gatel is not among the measured targets. These small
fixtures fit in memory, and preflight checks read them before load, so the
comparison exercises warm files rather than cold-storage latency.

## Scenarios

| | What it exercises |
|---|---|
| `static-4k` | the per-request path: parse, route, open, send, keep alive |
| `static-1m` | streaming a body larger than any buffer |
| `static-10m` | sustained throughput, where the file system starts to matter |
| `range-10m` | a `Range` request for the first 64 KiB; its zero offset does not exercise seeking |
| `proxy` | the proxy path: connection reuse, header rewriting, and the pool |

Four additional controls are opt-in with `--scenario`: `static-64k`,
`static-65k`, `range-offset-10m` (64 KiB starting at 1 MiB), and `range-65k`.
They separate response size, file positioning, and Gatel's inline/streamed
boundary. The original five scenarios remain the default.

For performance changes, use the same harness/configuration to alternate a base
and candidate image on one host. Override the Gatel image with `COMPOSE_FILE`
and use `--target gatel --no-build`. Record both source revisions and actual
image IDs separately: the report's checkout revision alone does not identify
an externally supplied image. Prefer longer windows and several alternating
pairs over comparing a new runner against an old report.

## Fairness

Every server is configured to do the same work and no more: no access log, no
compression, no cache, `Accept-Encoding: identity` on every request. A server
asked to do less would look faster for a reason that has nothing to do with how
fast it is.

The proxy scenario points all three at the same nginx origin, so what is being
compared is the proxying rather than the backend.

Connection budgets still differ: Gatel defaults to 32 active / 8 idle upstream
connections, nginx here allows 128 idle connections per worker, and Caddy 2.9
defaults to 32 idle connections per host with no active limit. Idle capacity
is not an active limit. Preserve this default-profile comparison and use a
separate, explicitly budgeted profile before attributing differences to pool
implementation. Capture connection reuse, errors and resource use as well as
throughput. Gatel's image healthchecks are disabled only in this Compose setup;
the harness checks readiness without injecting periodic traffic into the load.

Harness regression tests (without Docker):

```bash
python -B -m unittest discover -s benchmarks/http-servers -p 'test_*.py'
```

## The files under test

Generated on first run into `fixtures/www/`, which is not tracked: eleven
megabytes of incompressible noise is not source, and a repository that carries
it pays for it on every clone. The bytes come from a fixed seed, so two
machines are reading the same thing — and the large files really are
incompressible, which keeps a server that decides to compress one from looking
fast.
