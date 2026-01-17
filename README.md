# Casper Node Proxy

HTTP proxy for Casper node/sidecar with per-network routing, SSE fanout, JSON-RPC batching, and binary port bridging.

## Features
- SQLite database (Diesel) with network and config tables, override via `DATABASE_URL`.
- Per-network SSE ingestion using `veles_casper_rust_sdk` with retry state in `/tmp/<network>_last_id.txt`.
- `/events` supports both WebSocket and SSE.
- JSON-RPC proxy with batch support (splits batches for upstream sidecar) and per-method metrics.
- Binary port WebSocket proxy with upstream connection pooling.
- Per-IP rate limiting (HTTP-level and per-JSON-RPC-item).

## Quick start
1. Create or seed the DB (defaults to `sqlite://./db.sqlite`):
   - Run the app once to apply migrations, or
   - Apply `migrations/0001_create_tables` manually with `sqlite3`.
2. Insert network config rows (example devnet):
   - `network_name`: `devnet`
   - `chain_name`: `casper-devnet`
   - `rest`: `http://127.0.0.1:14101`
   - `sse`: `http://127.0.0.1:18101/events`
   - `rpc`: `http://127.0.0.1:11101/rpc`
   - `binary`: `127.0.0.1:28101`
   - `gossip`: `127.0.0.1:22101`
3. Run: `cargo run`

## Environment variables
- `DATABASE_URL` (default: `sqlite://./db.sqlite`)
- `BIND_ADDR` (default: `0.0.0.0:8080`)
- `RATE_LIMIT_PER_MIN` (default: `60`) - HTTP-level limiter
- `RATE_LIMIT_BURST` (default: `20`) - HTTP-level burst
- `RPC_RATE_LIMIT_PER_MIN` (default: `RATE_LIMIT_PER_MIN`)
- `RPC_RATE_LIMIT_BURST` (default: `RATE_LIMIT_BURST`)
- `SSE_BROADCAST_CAPACITY` (default: `256`)
- `SSE_BACKLOG_LIMIT` (default: `16384`)
- `BINARY_POOL_SIZE` (default: `4`)

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
- Accepts single and batch JSON-RPC payloads.
- Batch payloads are split and forwarded one-by-one to the upstream.
- Metrics count each JSON-RPC method item.
- Per-item rate limiting:
  - If over limit, returns a JSON-RPC error object for that item:
    ```json
    {
      "jsonrpc": "2.0",
      "id": 1,
      "error": {
        "code": -32000,
        "message": "rate limit exceeded",
        "data": { "retry_after_secs": 3 }
      }
    }
    ```
  - Response includes `ratelimit-remaining` HTTP header for per-item limiter.

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

## Rate limiting
Two layers:
1. HTTP-level limiter (tower-governor):
   - Applies to all endpoints.
   - Adds `x-ratelimit-*` and `retry-after` headers.
2. JSON-RPC per-item limiter:
   - Applied inside `/rpc`.
   - Adds `ratelimit-remaining` HTTP header.
   - Over-limit items return JSON-RPC error objects (no HTTP 429 for the whole batch).

Client handling guidance:
- Treat HTTP `429` responses as rate-limited; honor `retry-after` and `x-ratelimit-*` headers.
- For `/rpc`, parse the JSON-RPC error `data.retry_after_secs` and use the `ratelimit-remaining` header.

## SSE retry state
Each network uses `/tmp/<network_name>_last_id.txt` to persist the last seen SSE event id.
If the listener reconnects, it uses `start_from` to resume.

## Testing
Integration tests:
- `tests/sidecar_batch.rs`: batch proxying to sidecar
- `tests/events_ws.rs`: `/events` WebSocket stream
- `tests/events_sse_listener.rs`: `/events` SSE stream using `veles_casper_rust_sdk::sse::listener`
- `tests/binary_ws.rs`: `/binary` WebSocket to binary port
- `tests/rate_limit.rs`: HTTP rate limiting
- `tests/rpc_item_rate_limit.rs`: per-item JSON-RPC limiting

Some tests require local Casper services:
- Sidecar RPC default: `http://127.0.0.1:11101/rpc` (`SIDECAR_RPC_URL` override)
- Binary port default: `127.0.0.1:28101` (`BINARY_PORT_ADDR` override)

## Rust toolchain
Pinned via `rust-toolchain.toml`:
- `channel = "1.92.0"`
