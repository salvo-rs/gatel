# HTTP Server Benchmarks

This benchmark suite compares `gatel` with `nginx` and `caddy` under two
equivalent HTTP workloads:

- Static file serving
- Reverse proxying to the same upstream server

The suite is containerized so the same benchmark topology can be repeated on
different machines with minimal host-specific setup.

## Requirements

- Docker
- Docker Compose
- Python 3.11+

## Usage

```powershell
python benchmarks/http-servers/run.py
```

Optional arguments:

```text
--rounds 3
--duration 10
--threads 4
--connections 128
--warmup 3
```

Results are written to:

```text
target/benchmarks/http-servers/<timestamp>/
```

The generated report is saved as `report.md` in that directory.
