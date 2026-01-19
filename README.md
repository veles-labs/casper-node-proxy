# Casper Node Proxy

HTTP proxy for Casper node/sidecar with per-network routing, SSE fanout, JSON-RPC proxying, binary port bridging, and Prometheus metrics.

## Features
- SQLite or Postgres database (Diesel) with network and config tables, override via `DATABASE_URL`.
- Per-network SSE ingestion using `veles_casper_rust_sdk` with resume state stored in Redis (optional).
- `/events` supports both WebSocket and SSE.
- JSON-RPC proxy with per-method metrics.
- Binary port WebSocket proxy with upstream connection pooling.
- Per-IP rate limiting (HTTP-level).
- Prometheus metrics on a configurable listener (defaults to loopback only).

## Quick start
1. Create or seed the DB (defaults to `sqlite://./db.sqlite`):
   - Run `cargo run -- migrate` to apply migrations, or
   - Apply `migrations/0001_create_tables` manually with `sqlite3`.
2. Insert network config rows (example devnet):
   - `network_name`: `devnet`
   - `chain_name`: `casper-devnet`
   - `rest`: `http://127.0.0.1:14101`
   - `sse`: `http://127.0.0.1:18101/events`
   - `rpc`: `http://127.0.0.1:11101/rpc`
   - `binary`: `127.0.0.1:28101`
   - `gossip`: `127.0.0.1:22101`
3. Run: `cargo run -- start` (or `cargo run`)

## CLI
Subcommands:
- `start` (default): run the HTTP proxy service.
- `migrate`: apply database migrations and exit.

## Environment variables
- `DATABASE_URL` (default: `sqlite://./db.sqlite`) - database connection string (SQLite or Postgres).
- `BIND_ADDR` (default: `0.0.0.0:8080`) - HTTP listener address for the proxy API.
- `RATE_LIMIT_PER_MIN` (default: `60`) - HTTP-level requests per minute per client IP.
- `RATE_LIMIT_BURST` (default: `20`) - extra burst capacity for HTTP-level rate limiting.
- `METRICS_BIND_ADDR` (default: `127.0.0.1:9090`) - metrics listener address (Prometheus `/metrics`).
- `SSE_BROADCAST_CAPACITY` (default: `256`) - capacity for per-network SSE broadcast channel.
- `SSE_BACKLOG_LIMIT` (default: `16384`) - max SSE events retained in memory for replay.
- `BINARY_POOL_SIZE` (default: `4`) - number of pooled upstream binary-port connections per network.
- `BINARY_RATE_LIMIT_PER_MIN` (default: `RATE_LIMIT_PER_MIN`) - per-connection binary-port requests per minute.
- `BINARY_RATE_LIMIT_BURST` (default: `RATE_LIMIT_BURST`) - per-connection binary-port burst capacity.
- `REDIS_URL` (default: `redis://127.0.0.1/`) - Redis endpoint for persisting SSE resume state (optional).

## Database schema
Tables (Diesel):
- `network`:
  - `network_name` TEXT PK
  - `chain_name` TEXT
- `config`:
  - `network_name` TEXT PK/FK -> network
  - `rest` TEXT
  - `sse` TEXT
  - `rpc` TEXT
  - `binary` TEXT
  - `gossip` TEXT

## Endpoints

### `/{network_name}/`
Returns a HATEOAS-style JSON payload with discoverable endpoints.

Example response:
```json
{
  "network_name": "devnet",
  "chain_name": "casper-devnet",
  "rpc": "/devnet/rpc",
  "events": "/devnet/events",
  "binary": "/devnet/binary"
}
```

### `/{network_name}/rpc` (JSON-RPC)
Proxies JSON-RPC to the configured `rpc` URL.

Behavior:
- Accepts single JSON-RPC payloads; batch arrays return HTTP 400.
- Metrics count each JSON-RPC method.

### `/{network_name}/events` (WebSocket or SSE)
Event stream of Casper SSE events with proxy-assigned IDs.

Source:
- The proxy connects to the configured `sse` URL using `veles_casper_rust_sdk`.
- Each incoming event gets an incrementing `id` and is stored in an in-memory backlog.
- Backlog size is capped by `SSE_BACKLOG_LIMIT` (oldest items drop).

Query params:
- `start_from=<id>` (inclusive): returns backlog items with `id >= start_from` before live stream.

#### WebSocket
If the client requests a WebSocket upgrade:
- Each message is a JSON text frame:
  ```json
  { "id": 42, "data": { ...SseEvent... } }
  ```

#### SSE
If not upgrading to WebSocket, the response is `text/event-stream`:
- `id`: proxy-assigned ID
- `data`: JSON encoded `SseEvent` (not wrapped)

Example SSE frame:
```
id: 42
data: {"ApiVersion":{"major":1,"minor":5,"patch":0}}

```

### `/{network_name}/binary` (WebSocket)
Binary port bridge to the configured `binary` address.

Behavior:
- Client sends binary WebSocket messages containing a Casper binary port request.
- Proxy forwards each request to the upstream and returns the raw binary response as a binary WS message.
- Upstream connections are pooled; requests are serialized per connection.
- Binary port `request_id` is client-scoped; the proxy forwards IDs unchanged and does not require global uniqueness.
- Per-connection rate limiting applies to binary requests; over-limit messages receive a JSON error over the websocket.

## Rate limiting
HTTP-level limiter (tower-governor):
- Applies to all endpoints.
- Adds `x-ratelimit-*` and `retry-after` headers.

Client handling guidance:
- Treat HTTP `429` responses as rate-limited; honor `retry-after` and `x-ratelimit-*` headers.

## Metrics
Prometheus metrics are exposed on `METRICS_BIND_ADDR` at `/metrics` (defaults to `127.0.0.1:9090`).

## SSE retry state
Each network stores the last seen SSE event id in Redis at `sse_last_id:<network_name>`.
If the listener reconnects, it uses `start_from` to resume.
If Redis is unavailable, resume is disabled and the listener starts without `start_from`.

## Testing
Integration tests:
- `tests/sidecar_batch.rs`: batch requests rejected
- `tests/events_ws.rs`: `/events` WebSocket stream
- `tests/events_sse_listener.rs`: `/events` SSE stream using `veles_casper_rust_sdk::sse::listener`
- `tests/binary_ws.rs`: `/binary` WebSocket to binary port
- `tests/rate_limit.rs`: HTTP rate limiting

Some tests require local Casper services:
- Sidecar RPC default: `http://127.0.0.1:11101/rpc` (`SIDECAR_RPC_URL` override)
- Binary port default: `127.0.0.1:28101` (`BINARY_PORT_ADDR` override)

## Rust toolchain
Pinned via `rust-toolchain.toml`:
- `channel = "1.92.0"`
