# Events

Every structured event Gatel emits, by target. `cargo run -p xtask` fails if
this list and the code disagree in either direction — an event that stopped
being emitted, or one added without being written down. A name here is part of
the interface: something is alerting on it.

Access logs are not here. They are one line per request, in their own format, to
their own destination — see `access-log` in `README.md`.

## `gatel::process`

| Event | When |
|---|---|
| `process_started` | The binary started and is about to read its configuration. |
| `shutdown_started` | A signal arrived. In-flight requests are being finished. |
| `shutdown_complete` | Everything drained, or the grace period ran out. |

## `gatel::config`

The configuration transaction. Exactly one of the first four follows every
attempt.

| Event | When |
|---|---|
| `config_committed` | A generation was published. Carries the generation, `spec_hash` and `plan_hash`. |
| `config_rejected` | The configuration is invalid. The previous generation still serves. |
| `prepare_failed` | Valid but unbuildable — a port in use, a certificate that will not load. No generation was consumed. |
| `candidate_stale` | A source moved while this candidate compiled. Something newer will report for itself. |
| `watching` | The set of files being watched, after a successful load. |
| `watch_failed` | The file watcher reported an error; the polling fallback carries on. |
| `watch_unavailable` | No watcher could be started at all. Reloads depend on polling and signals. |

## `gatel::listener`

| Event | When |
|---|---|
| `serving` | The process is accepting on its first generation's addresses. |
| `listener_started` | An address began accepting. |
| `listener_stopped` | An address stopped, because a reload no longer names it. |
| `accept_failed` | One accept failed — usually file descriptors. The loop backs off and continues. |

## `gatel::connection`

| Event | When |
|---|---|
| `connection_closed` | A connection ended with an error worth seeing. |
| `tls_handshake_failed` | A handshake did not complete. Ordinary on a public port. |
| `tls_handshake_timeout` | A client opened a connection and never finished the handshake. |
| `tunnel_closed` | An upgraded connection ended, with why: closed, idle, expired or error. |
| `upgrade_failed` | An upgrade was agreed and then could not be established. |
| `client_upgrade_unavailable` | The backend agreed to upgrade, but the client connection cannot be. |

## `gatel::request`

| Event | When |
|---|---|
| `route_timeout` | A route did not produce a response head within its `timeout`. |
| `rewrite_failed` | A `rewrite` produced something that is not a valid path. The request continues unchanged. |

## `gatel::upstream`

| Event | When |
|---|---|
| `upstream_failed` | A proxied request got no usable answer. Carries the status the client was given. |
| `checkout_failed` | A connection to a backend could not be obtained. Nothing had been sent, so another endpoint may be tried. |
| `endpoint_ejected` | Passive health took an endpoint out of rotation. |
| `upstream_upgrade_unavailable` | The backend answered 101 without giving a connection to take over. |

## `gatel::runtime`

| Event | When |
|---|---|
| `upstream_state_resolved` | A commit resolved an upstream's state, and something was not reused. |
| `endpoint_health_changed` | A probe changed an endpoint's verdict. Only a change, never a round. |
| `probe_started` | A health check worker started for an upstream. |
| `probe_stopped` | A worker stopped, because a reload no longer asks for it. |
| `state_retired` | State nothing in the new generation needs was released. |

## `gatel::certificate`

| Event | When |
|---|---|
| `certificate_loaded` | A certificate was read and installed for a site. |
| `certificate_issued` | The development authority issued one. |
| `certificate_forgotten` | A site no longer exists, so its certificate was released. |
| `internal_ca_created` | The development authority was generated, on first use. |

## `gatel::file`

| Event | When |
|---|---|
| `read_failed` | `file-server` could not read a file it had already found. |
