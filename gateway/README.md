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

## Status — Phases 1–4 (this crate)

Skeleton + auth + **custodial balance ledger** + read + mint + receive + send/burn +
instant internal transfers. Endpoints:

| Method | Path                  | Auth | Notes |
|--------|-----------------------|------|-------|
| GET    | `/health`             | no   | liveness |
| GET    | `/v1/assets`          | yes  | the caller's **balances**, enriched with tapd metadata |
| GET    | `/v1/balance?asset_id=…` | yes | the caller's balance of one asset |
| GET    | `/v1/history?limit=…` | yes  | the caller's tx history (mint/receive/send/burn/transfers), owner-scoped |
| GET    | `/v1/universe/stats`  | yes  | global aggregate (not owner-scoped) |
| GET    | `/v1/universe/roots`  | yes  | global aggregate (not owner-scoped) |
| GET    | `/v1/asset/meta?asset_id=…` | yes | public asset metadata (name/ticker blob, decimals) |
| GET    | `/v1/info`            | yes  | node version + network (non-sensitive) |
| GET    | `/v1/ln/decode?pay_req=…&asset_id=…` | yes | decode a Lightning asset invoice (read-only) |
| GET    | `/v1/ln/rfq-quotes`   | yes  | accepted RFQ quote counts (LN-asset routing health) |
| POST   | `/v1/ln/pay`          | yes  | pay a Lightning asset invoice; **debits the caller** (refund on failure) |
| POST   | `/v1/ln/receive`      | yes  | create a Lightning asset invoice; **credits the caller** when it settles (needs lnd macaroon for auto-credit) |
| GET    | `/v1/decode?addr=…`   | yes  | decode a Taproot Asset address |
| POST   | `/v1/mint`            | yes  | mint an asset; credits the caller on confirmation (async) |
| GET    | `/v1/mint/status?batch_key=…` | yes | mint progress; owner-gated |
| POST   | `/v1/receive`         | yes  | get a receive address; caller is credited when it confirms |
| POST   | `/v1/send`            | yes  | send an asset out; **debits the caller** (403 if insufficient) |
| POST   | `/v1/burn`            | yes  | burn an asset; **debits the caller** |
| POST   | `/v1/transfer`        | yes  | **instant, free** ledger transfer to another gateway user |
| GET    | `/v1/sats/balance`    | yes  | the caller's custodial sats balance (funds on-chain fees) |
| POST   | `/v1/sats/deposit`    | yes  | LN invoice to top up sats; credited on settlement (needs lnd `invoices:write`) |
| GET    | `/v1/fee/quote?op=…&fee_rate_sat_vb=…` | yes | sats fee estimate for a chargeable op (`mint`/`send`); `charged` flags whether it's actually debited |
| POST   | `/v1/admin/claim`     | authed | trust-on-first-use: become operator (needs `OZARK_GATEWAY_ALLOW_ADMIN_CLAIM`, no admin yet) |
| GET    | `/v1/admin/channels`  | operator | list the node's channels (asset channels included) |
| POST   | `/v1/admin/channel/open` | operator | open an asset channel to a connected peer (funds LN routing) |
| POST   | `/v1/admin/peer/connect` | operator | connect to a Lightning peer |

**Balance ledger:** ownership is `(asset_id, pubkey) → amount`. tapd holds the real
assets; the ledger tracks each user's share. Every mutating action checks and moves
the caller's balance, so no user can touch another's holdings. `send`/`burn` debit
first and refund if tapd rejects; an insufficient balance is a **403**.

**Instant transfers:** `/v1/transfer` between two gateway users is a pure ledger
move — atomic, no on-chain transaction, no fee.

**Fees & sats balance (optional):** each user has a custodial **sats balance**
(separate from asset balances) that funds on-chain operation fees. Off by default
(`OZARK_GATEWAY_CHARGE_FEES=0`) — ops are free and the operator eats the on-chain
cost. When on, `mint`/`send` debit `fee = max(floor, rate × vsize) + margin` from
the caller's sats and credit it to the operator, atomically, before the node call;
the fee is refunded if the op fails (insufficient sats is a **403**). The margin is
a configurable markup (`OZARK_GATEWAY_FEE_MARGIN_BPS`, default 10%). Users top up
via `/v1/sats/deposit` (an lnd invoice, credited on settlement) and preview the cost
via `/v1/fee/quote`. Charging fees needs an operator (recipient) **and** an lnd
macaroon with `invoices:write` (deposits issue invoices via lnd `AddInvoice`).

**Operator routes:** `/v1/admin/*` spend the node's OWN liquidity (open asset
channels, connect peers) so they are gated to a single operator: the routes require
a NIP-98 signature by exactly the operator pubkey (403 otherwise). The operator is
either `OZARK_GATEWAY_ADMIN_PUBKEY` (explicit) **or** whoever claimed it. Opening an
asset channel to a quoting peer is the prerequisite for LN-asset pay/receive to
route at all.

**Operator claim (no hex to copy):** to make setup one-tap, set
`OZARK_GATEWAY_ALLOW_ADMIN_CLAIM=1` and `POST /v1/admin/claim` from the wallet: the
first authenticated caller is persisted as the operator (ignored once an operator
exists). The wallet's "Devenir opérateur" button does this. Turn the flag off after
claiming. Anyone who can reach the onion could claim while it's on, so only enable
it during setup.

**Async mint:** tapd broadcasts a batch immediately but the asset id only exists
once the genesis confirms. The gateway holds a pending claim keyed by batch and
resolves it (crediting the minter) by matching an asset's anchor txid to the batch
txid. Reconciliation (mints + receives + LN receives) runs opportunistically on
`/v1/assets` and `/v1/mint/status`, **and** on a background interval
(`OZARK_GATEWAY_RECONCILE_INTERVAL_SECS`, default 60s) so settlements are credited
even with no request traffic. The background loop also purges canceled/expired LN
invoices and runs a **solvency audit**: per asset, the sum of ledger balances is
compared to tapd's actual holding, logging an `error!` on any drift (the invariant
that liabilities never exceed holdings).

**Crash recovery:** `send`/`ln_pay` reserve the balance **and** write a durable
`in_flight` row in one transaction *before* the node call, so a crash mid-payment
leaves a recoverable record instead of a silent debit. On the terminal outcome the
row is settled (history recorded) or refunded. On boot + each maintenance pass,
`recover_in_flight` resolves any leftovers: **LN** payments are tracked via lnd's
`TrackPaymentV2` (SUCCEEDED keeps the debit, FAILED refunds); **on-chain** sends
are never auto-refunded (the tx may have broadcast — that would double-spend
custody) and are surfaced for manual review.

**Backups:** the SQLite ledger IS the custody record — losing it means tapd still
holds the assets but attribution is gone. Set `OZARK_GATEWAY_BACKUP_DIR` (+ a
32-byte hex `OZARK_GATEWAY_BACKUP_KEY`) to take periodic consistent snapshots
(`VACUUM INTO`), encrypted at rest with XChaCha20-Poly1305, retained
`OZARK_GATEWAY_BACKUP_RETENTION` deep. Ship them off-box for real disaster
recovery.

Not yet: Taproot-assets-over-Lightning, pay-to-mint, and rewiring the app to the
gateway. See `../.claude` memory `ozark-marketplace` for the full roadmap.

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
| `OZARK_GATEWAY_LND_MACAROON`   | no  | — | lnd macaroon path: `invoices:read` (LN-receive credit) + `invoices:write` (sats deposit) |
| `OZARK_GATEWAY_CHARGE_FEES`    | no  | `0` | charge sats fees on mint/send (needs operator + lnd `invoices:write`) |
| `OZARK_GATEWAY_FEE_MARGIN_BPS` | no  | `300` | operator markup on the network estimate, bps (300 = 3%) |
| `OZARK_GATEWAY_FEE_FLOOR_SATS` | no  | `100` | minimum sats per chargeable op |
| `OZARK_GATEWAY_MINT_VSIZE` / `_SEND_VSIZE` | no | `250` / `200` | assumed tx vsize for the estimate |
| `OZARK_GATEWAY_DEFAULT_FEE_RATE` | no | `5` | sat/vB assumed when the request omits a rate |

(Other optional vars — admin/claim, reconcile interval, backups — are documented in
`deploy/env.example`.)

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
