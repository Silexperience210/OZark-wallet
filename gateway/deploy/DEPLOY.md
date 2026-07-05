# Deploying the OZark gateway (Umbrel / self-hosted)

This runs the gateway next to your `litd`/`tapd` and publishes it as a **Tor onion
service**. The wallet's **Vault** screen then talks to that onion. The tapd macaroon
stays here, on the node — it is never in the app.

```
wallet (phone) ──Tor──▶ onion ──▶ [tor sidecar] ──▶ gateway:8787 ──gRPC──▶ litd/tapd
                                                        holds the macaroon
```

## 0. Prerequisites

- Docker + Docker Compose on the node (Umbrel has both).
- A running `litd`/`tapd` you can reach from Docker, and its **`tls.cert`** + a
  **tapd macaroon** with mint/send/burn/list/addr permissions (the tapd
  `admin.macaroon`).

## 1. Get the files onto the node

Copy the `gateway/` folder to the node (or `git clone` the repo and `cd gateway`).
Everything below runs from `gateway/deploy/`.

```sh
cd gateway/deploy
cp env.example .env
```

## 2. Provide the tapd credentials

Create `secrets/` and put your node's cert and macaroon there (the compose mounts
them read-only; they are gitignored and never leave the box):

```sh
mkdir -p secrets
cp /path/to/litd/tls.cert          secrets/tls.cert
cp /path/to/tapd/admin.macaroon    secrets/tapd.macaroon
```

On Umbrel the lightning app data lives under `~/umbrel/app-data/<lightning-app>/`.
Look for `tls.cert` and the tapd `admin.macaroon` (often under a `.lit`/`.tapd`
data dir). If unsure, `find ~/umbrel/app-data -name tls.cert` and
`find ~/umbrel/app-data -name '*.macaroon' | grep -i tap`.

## 3. Point the gateway at tapd

Edit `.env` and set `OZARK_GATEWAY_TAPD_HOST` to your litd/tapd gRPC `host:port`
(litd's default is `8443`). Two common ways to make it reachable:

- **Via LAN IP** (simplest): `OZARK_GATEWAY_TAPD_HOST=<node-ip>:8443`.
- **Via the node's docker network**: uncomment the `networks:` blocks in
  `docker-compose.yml` (set the external network to your node's, e.g.
  `umbrel_main_network`) and use the service name, e.g. `litd:8443`.

## 4. Build and start

```sh
docker compose up -d --build
```

First build compiles Rust (a few minutes on a Pi). Check it came up:

```sh
docker compose logs -f gateway     # look for "ozark-gateway listening on 0.0.0.0:8787"
```

If it exits complaining about tapd, re-check step 2/3 (cert path, macaroon, host).

## 5. Read the onion address

```sh
docker compose exec tor cat /var/lib/tor/ozark-gateway/hostname
```

That prints `something.onion` — this is your gateway URL: **`http://<that>.onion`**.
It is not a secret.

## 6. (Optional) strict URL binding

For the tightest auth, set the onion in `.env` and restart:

```sh
echo "OZARK_GATEWAY_PUBLIC_URL=http://<that>.onion" >> .env   # edit, don't duplicate
docker compose up -d
```

Skipping this is fine — auth still binds each request to its endpoint path.

## 7. Configure the wallet

Install the wallet build that has the **Vault** screen, open **Vault**, and paste
`http://<that>.onion` into the gateway URL field, then **Enregistrer**. Tap
**Actualiser** — an empty list means it connected (you just hold nothing yet). Mint
or receive to try it.

The wallet reaches the onion through its own embedded Tor, so it works from anywhere
— no port forwarding, no clearnet exposure.

## Operating notes

- **Data**: balances/mints/receives live in the `gateway-data` volume
  (`/data/ozark-gateway.sqlite`). Back it up if you care about the ledger.
- **Updates**: `git pull` then `docker compose up -d --build`.
- **Logs**: `docker compose logs -f gateway`.
- **Security**: the macaroon never leaves this box; the onion is the only ingress
  and every request must carry a valid NIP-98 signature. The custodial ledger means
  you (the operator) are trusted to honor balances — the exit door is on-chain
  send/withdraw.
