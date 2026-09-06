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

It is deliberately not in CI. A shared runner's throughput moves by tens of
percent between runs for reasons that have nothing to do with the commit, so a
threshold there would either never fire or fire constantly — and both teach
people to ignore it. Run this on a machine that is otherwise idle, twice, and
believe it only if the two runs agree.

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
--output-dir PATH     write results somewhere other than the default
```

Results land in `target/benchmarks/http-servers/<timestamp>/`:

- `report.md` — the comparison, per scenario
- `summary.json` — every round, machine-readable
- `raw/*.txt` — what `wrk` actually printed

## Scenarios

| | What it exercises |
|---|---|
| `static-4k` | the per-request path: parse, route, open, send, keep alive |
| `static-1m` | streaming a body larger than any buffer |
| `static-10m` | sustained throughput, where the file system starts to matter |
| `range-10m` | a `Range` request for the first 64 KiB — what a video player does on a seek |
| `proxy` | the proxy path: connection reuse, header rewriting, and the pool |

## Fairness

Every server is configured to do the same work and no more: no access log, no
compression, no cache, `Accept-Encoding: identity` on every request. A server
asked to do less would look faster for a reason that has nothing to do with how
fast it is.

The proxy scenario points all three at the same nginx origin, so what is being
compared is the proxying rather than the backend.

## The files under test

Generated on first run into `fixtures/www/`, which is not tracked: eleven
megabytes of incompressible noise is not source, and a repository that carries
it pays for it on every clone. The bytes come from a fixed seed, so two
machines are reading the same thing — and the large files really are
incompressible, which keeps a server that decides to compress one from looking
fast.
