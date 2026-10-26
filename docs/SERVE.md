# `protoledger serve`

```sh
protoledger serve --store <dir> [--listen 127.0.0.1:7417] [--allow-remote] [--poll-ms 1000] [--debounce-ms 150] [--no-watcher]
```

Serves one store (SPEC §9): every append is streamed over a WebSocket, the
index is queried over HTTP, and project/attach/unbind are performed over HTTP.
Once it accepts connections it prints one line to stdout, `listening on
<addr>` (with `--listen 127.0.0.1:0` the address names the port it picked).
It runs until it is killed.

Built with the cargo feature `serve` (in `default`). The library without it
(`--no-default-features --features store,sync`) pulls in no tokio.

## What it owns

`serve` opens the store and is its writer for as long as it runs. It runs the
sync engine for every active binding, exactly as `protoledger watch` does: at
start it syncs every active binding once (whatever the locations gained while
nobody followed them), then follows each with a filesystem watcher plus a poll
(`--debounce-ms`, `--poll-ms`; `--no-watcher` polls only).

A store's writer holds an exclusive lock on each ledger it has appended to, so
while `serve` runs, `protoledger unbind` (or any other writer) from another
process is refused as *locked* for those ledgers. Use `POST /v1/unbind` (and
`/v1/project`, `/v1/attach`, `/v1/import`) instead: `serve` performs them
itself, as the lock holder, and starts or stops following at once.

## Loopback only, by default

`serve` has **no authentication**: anyone who can reach its port can read
every ledger and import, project, attach and unbind. So `--listen` (an
`ip:port`, `[ipv6]:port`, or `host:port`) must be a **loopback** address:
`127.0.0.1:<port>` (any of 127.0.0.0/8 is accepted, though only the
addresses your loopback interface carries can be bound: on macOS by default
only 127.0.0.1), `[::1]:<port>`, or a host name such
as `localhost` that resolves to one (the first loopback address it resolves
to is used). Anything else (`0.0.0.0`, `[::]`, a LAN or public address) is
refused at start, before the store is opened, with a message that says so and
why; `serve` exits non-zero without listening.

`--allow-remote` lifts the refusal and listens wherever `--listen` says: an
`ip:port` as given, and a host name at the **first** address it resolves to,
loopback or not, in the resolver's order (without `--allow-remote` a host
name's first *loopback* address is preferred; with it there is no
preference). To listen on one particular address of a host with several, give
that address as `ip:port`. Put something that authenticates in front of it
if anything but you can reach that address.

## Conventions

- Every body, request and response, is JSON (`Content-Type: application/json`).
- Every error is `{"error": "<message>"}` with one of these statuses:

  | status | when |
  |---|---|
  | 400 | malformed request: bad JSON, a missing field, a bad query parameter, a bad `from` |
  | 404 | unknown agent or binding, unknown endpoint |
  | 409 | unbinding a binding that is not active; any request naming an agent whose ledger is damaged: subscribing to it, its row, its events, its children, projecting it, an import or attach that reaches its ledger, unbinding a binding of it (the store names the ledger, and refuses every operation on it until it is repaired; the body is the same for each) |
  | 422 | the store refused the operation for any other reason (unknown format, unreadable input, a location already bound, a projection that does not come back as itself, ...) |
  | 500 | anything else (the index is stale, the engine has stopped, ...) |

- **The resume rule** (stated here only; every other mention points here).
  A `from` seq, for an agent, is the **last seq the client has seen** of that
  agent, and the answer holds exactly the events with a seq **strictly
  after** it: `seq > from`. `0` means from the start (seqs begin at 1). It is
  the one rule for the WebSocket's `from` (its backfill and its live events
  alike) and for `/events`' `from`, and the server decides it in one function
  for all of them. A client that keeps the last seq it received and
  reconnects with it gets the next event and nothing twice.
- Paths in request bodies are read by the `serve` process; give them absolute
  (a relative path is resolved against `serve`'s working directory).
- Agent ids in URL paths are percent-encoded as usual.

## Shapes

**Agent** (an index row, SPEC §9.1):

```json
{"id": "main-1.sub-1", "path": "ledgers/main-1.sub-1.jsonl", "last_seq": 4,
 "byte_len": 1830, "size": 1830, "mtime_ns": 1760600000000000000,
 "created_at": "2026-10-16T10:10:04.000Z", "source": null,
 "parent": {"agent": "main-1", "event": "main-1:call:1", "relation": "subagent"},
 "links": [], "title": null}
```

`source`, `parent` and `links` are as in the ledger header (SPEC §2.1);
`title` is the text of the newest `title` event.

**Damaged agent** (in a listing, in place of its `Agent`): an agent whose
ledger is damaged is listed with its id, its ledger and why, and none of the
counts the index read before the damage (as `protoledger store ls` marks it):

```json
{"id": "peer-a", "path": "ledgers/peer-a.jsonl",
 "damaged": "ledger ledgers/peer-a.jsonl (agent peer-a) is damaged and was skipped: …"}
```

**Binding** (SPEC §9.2):

```json
{"binding": "005f14d9-…", "agent": "019f0634-…", "format": "codex",
 "variant": "rollout-legacy", "location": "/abs/path/rollout-….jsonl",
 "native_id": "019f0634-…", "mode": "attachment", "active": true, "ended": null}
```

`mode` is `projection` or `attachment`; `ended` is `unbound` or `diverged`
once `active` is false.

**Published event** (SPEC §9.4), the one shape events take everywhere:

```json
{"agent": "hello-1", "seq": 3, "offset": 412, "event": { …the ledger event, SPEC §2.2… }}
```

`offset` is the byte offset of the event's line in its ledger.

## WebSocket

### `GET /v1/subscribe?from=<agent>:<seq>,...` / `?from=all`

- `from=a:5,b:0`: agents `a` and `b` only, each by the resume rule (`a`'s
  events after seq 5, all of `b`'s). The seq is split off at the **last**
  colon, so an agent id may hold colons; it may not hold a comma. An agent not in the store yet is
  allowed: its events arrive as it gains them.
- `from=all`: every agent from its start, and every agent created later.
- A missing or malformed `from` (no colon, a seq that is not a number, an
  agent listed twice) is refused with 400 before the upgrade; one naming an
  agent whose ledger is damaged, with 409 (`from=all` leaves it out).

After the upgrade the server sends one **text frame per published event**
and nothing else. The client sends nothing (anything it sends is ignored;
pings are answered). Order:

1. **Backfill**: for each agent asked for, the ledger's events the resume
   rule owes the client, up to the point at which the client was registered,
   read from the ledger file, in seq order.
2. **Live**: every event appended afterwards (by sync, or by
   import/attach/project/unbind through this server), in the order it was
   appended.

Within an agent the frames are in seq order, with no gap and no repeat:
backfill covers `(from, snap]` and live covers `(snap, …)`, where `snap` is
the agent's last indexed seq when the client was registered. Across agents the
order is not specified during backfill; live frames are in append order.

When the server is stopping it closes the socket with code 1011; a backfill
it cannot read also closes with 1011 and the reason. Either way: reconnect
with the last seq received per agent.

Transient states (SPEC §9.4 `{agent, transient}`) are not sent: no adapter
produces them yet.

## HTTP queries

| request | answer |
|---|---|
| `GET /v1/agents` | `[Agent]`, ordered by id; a damaged agent's entry is a `Damaged agent` |
| `GET /v1/agents/{id}` | `Agent`; 404 if unknown, 409 if its ledger is damaged |
| `GET /v1/agents/{id}/events?from=<seq>&limit=<n>` | `[Published event]`: up to `limit` (default 1000) of the events the resume rule gives for `from` (default 0), in seq order; `[]` past the end; 404 if the agent is unknown, 409 if its ledger is damaged |
| `GET /v1/agents/{id}/children` | `[Agent]`: direct structural children (agents whose `parent` names it), a damaged one as a `Damaged agent`; 404 if unknown, 409 if its ledger is damaged |
| `GET /v1/bindings` | `[Binding]`, active and ended |

To page through `/events`, pass the last `seq` of one page as the next
`from`.

## HTTP operations

| request | body | answer |
|---|---|---|
| `POST /v1/import` | `{"format": "<name>\|auto", "path": "<file or dir>"}` | `{"format", "variant", "created": [id], "extended": [id], "events_appended", "events_skipped", "losses": [{"code", "kind"?, "count", "detail"?}]}` |
| `POST /v1/project` | `{"agent": "<id>", "format": "<name>", "dir": "<dir>"}` | `Binding` (mode `projection`); 404 if the agent is unknown, 409 if its ledger is damaged |
| `POST /v1/attach` | `{"format": "<name>\|auto", "location": "<file or dir>"}` | `{"binding": Binding, "created": [id], "events_appended": n}` |
| `POST /v1/unbind` | `{"binding": "<id>"}` | `Binding`, now `active: false, ended: "unbound"`; 404 if unknown, 409 if not active or if its agent's ledger is damaged |

These are the library's `Store::import`, `sync::project`, `sync::attach` and
`sync::unbind` (SPEC §9.2), with the same refusals (422; 409 when the ledger
an import or attach reaches is damaged). Every event they
append is published to subscribers like any other append. After project,
attach and unbind the server follows exactly the active bindings at once.
