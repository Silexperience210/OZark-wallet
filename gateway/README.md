# OZark Gateway

A small, standalone service that lets many OZark wallets safely share **one** tapd
(Taproot Assets) node without any of them holding the node's macaroon — and without
being able to touch each other's assets.

## Why it exists

tapd has **no concept of per-user ownership**: anyone with the macaroon can mint,
send, or burn *any* asset on the node. Early OZark builds shipped that macaroon
inside the APK, so any user could act on the operator's node. The gateway fixes this
at the root:

- The **macaroon stays on the node**, read from a local file. It is never in the
  binary and never in the APK.
- The wallet talks to the gateway over the node's **Tor onion service** (the onion
  address is not a secret and can be shipped in the app).
- Every request is authenticated with a **NIP-98** event signed by the wallet's
  Nostr key (the same key it derives from its seed via NIP-06).
- A **SQLite ownership registry** maps `asset_id → owner_pubkey`. Reads are scoped
  to the caller; mutating actions (later phases) require `owner == caller`.

So the node becomes a shared tool: everyone can read/mint, but only the owner of an
asset can move or destroy it.

## Status — Phase 1 (this crate)

Skeleton + auth + read + registry. Endpoints:

| Method | Path                  | Auth | Notes |
|--------|-----------------------|------|-------|
| GET    | `/health`             | no   | liveness |
| GET    | `/v1/assets`          | yes  | tapd assets **filtered to the caller's owned asset ids** |
| GET    | `/v1/universe/stats`  | yes  | global aggregate (not owner-scoped) |
| GET    | `/v1/universe/roots`  | yes  | global aggregate (not owner-scoped) |
| GET    | `/v1/decode?addr=…`   | yes  | decode a Taproot Asset address |

Mint (records ownership), send/burn (ownership-checked), and pay-to-mint arrive in
later phases. See `../.claude` memory `ozark-marketplace` for the full roadmap.

## Authentication (NIP-98)

Send `Authorization: Nostr <base64(event-json)>` where the event is:

- `kind` **27235**, signed by the caller's key;
- tag `u` = the full request URL, tag `method` = the HTTP method;
- `created_at` within `OZARK_GATEWAY_MAX_SKEW_SECS` of now (default 60s) — a captured
  header cannot be replayed later;
- optional `payload` tag = hex sha256 of the request body (bound on write endpoints).

The verified pubkey is the identity every ownership check keys off.

### URL binding

Set `OZARK_GATEWAY_PUBLIC_URL` to the canonical onion base URL (e.g.
`http://<onion>.onion`) so the full signed URL must match. If unset, only the
request path+query is matched (host-agnostic — tolerant of reverse proxies, still
binds the token to a single endpoint).

## Configuration (environment)

| Variable | Required | Default | Meaning |
|----------|----------|---------|---------|
| `OZARK_GATEWAY_LISTEN`         | no  | `127.0.0.1:8787` | TCP bind address (Tor fronts this) |
| `OZARK_GATEWAY_TAPD_HOST`      | yes | — | tapd gRPC `host:port` |
| `OZARK_GATEWAY_TAPD_CERT`      | yes | — | path to tapd `tls.cert` (PEM) |
| `OZARK_GATEWAY_TAPD_MACAROON`  | yes | — | path to the tapd macaroon (raw bytes) |
| `OZARK_GATEWAY_DB`             | no  | `ozark-gateway.sqlite` | ownership registry path |
| `OZARK_GATEWAY_PUBLIC_URL`     | no  | — | canonical base URL for strict `u`-tag matching |
| `OZARK_GATEWAY_MAX_SKEW_SECS`  | no  | `60` | NIP-98 timestamp tolerance |

## Deployment (Umbrel)

The gateway is a plain HTTP server on a local TCP port. It does **not** embed a Tor
onion server — Umbrel's system Tor publishes the onion, the standard pattern for
Umbrel apps. In the app's Tor config, add a hidden service pointing at
`OZARK_GATEWAY_LISTEN`, then bake that onion address into the wallet.

```
# torrc (managed by Umbrel's tor proxy)
HiddenServiceDir /var/lib/tor/ozark-gateway
HiddenServicePort 80 <gateway-container>:8787
```

Point `OZARK_GATEWAY_TAPD_*` at the litd/tapd running on the same node.

## Build & test

Requires `protoc` on PATH (compiles the tapd proto subset). CI installs it; see
`.github/workflows/gateway-ci.yml`.

```sh
cargo test          # auth + registry unit tests
cargo run           # needs the OZARK_GATEWAY_TAPD_* env vars set
```
