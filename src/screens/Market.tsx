import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { motion } from "framer-motion";
import {
  ArrowLeft,
  Plus,
  RefreshCw,
  Store,
  ChevronRight,
  TrendingUp,
  TrendingDown,
  Rocket,
  Pause,
  Play,
  Download,
  Globe,
} from "lucide-react";
import { useNotification } from "../contexts/NotificationContext";

interface MarketProps {
  onBack: () => void;
}

type Visibility = "public" | "private";
type Status = "trading" | "paused" | "migrated";
type Side = "buy" | "sell";

interface MarketView {
  token_id: string;
  ticker: string;
  name: string;
  creator: string;
  visibility: Visibility;
  status: Status;
  supply: number;
  reserve_sats: number;
  withdrawn: number;
  spot_price_msat: number;
  progress_bp: number;
  creator_fee_bp: number;
  creator_fees_sats: number;
  trade_count: number;
  created_at: number;
}

interface PricePoint {
  ts: number;
  price_msat: number;
  side: Side;
  tokens: number;
  supply_after: number;
}

interface BuyPreview {
  tokens: number;
  cost_sats: number;
  fee_sats: number;
  total_sats: number;
  new_supply: number;
  avg_price_msat: number;
}

interface SellPreview {
  tokens: number;
  refund_sats: number;
  fee_sats: number;
  payout_sats: number;
  new_supply: number;
  avg_price_msat: number;
}

// A real Taproot asset minted on the connected tapd node (from list_taproot_assets).
interface TapdAsset {
  asset_id: string;
  name: string;
  amount: number;
  asset_type: string;
  decimal_display: number;
}

// A token announcement published on Nostr by a desk (from market_discover).
interface TokenAnnouncement {
  asset_id: string;
  ticker: string;
  name: string;
  supply: number;
  reserve_sats: number;
  spot_price_msat: number;
  migration_sats: number;
  status: Status;
}
interface DiscoveredToken {
  desk_pubkey: string;
  ann: TokenAnnouncement;
}

// Nano-sat price resolution: keeps the integer curve params precise across a
// wide range of start/end prices and supply caps.
const DENOM = 1_000_000_000;

/** msat/token -> a compact sats string. */
function priceSat(msat: number): string {
  const s = msat / 1000;
  if (s >= 100) return s.toFixed(0);
  if (s >= 1) return s.toFixed(2);
  if (s > 0) return s.toFixed(3);
  return "0";
}

function statusMeta(s: Status): { label: string; color: string } {
  switch (s) {
    case "trading":
      return { label: "en cours", color: "#00f0ff" };
    case "paused":
      return { label: "en pause", color: "#f59e0b" };
    case "migrated":
      return { label: "migré", color: "#a855f7" };
  }
}

/** Lightweight inline SVG line chart of the price history (no chart lib). */
function PriceChart({ points }: { points: PricePoint[] }) {
  if (points.length < 2) {
    return (
      <div className="text-muted" style={{ fontSize: 12, textAlign: "center", padding: "24px 0" }}>
        Pas encore assez de trades pour tracer le graphique
      </div>
    );
  }
  const W = 320;
  const H = 110;
  const pad = 6;
  const prices = points.map((p) => p.price_msat);
  const min = Math.min(...prices);
  const max = Math.max(...prices);
  const range = max - min || 1;
  const n = points.length;
  const coords = points
    .map((p, i) => {
      const x = pad + (i / (n - 1)) * (W - 2 * pad);
      const y = pad + (1 - (p.price_msat - min) / range) * (H - 2 * pad);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  const up = prices[n - 1] >= prices[0];
  const stroke = up ? "#00f0ff" : "#ff4466";
  return (
    <svg viewBox={`0 0 ${W} ${H}`} width="100%" height="120" preserveAspectRatio="none" style={{ display: "block" }}>
      <polyline points={coords} fill="none" stroke={stroke} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  );
}

export function Market({ onBack }: MarketProps) {
  const { notify } = useNotification();
  const [view, setView] = useState<"list" | "detail" | "create">("list");
  const [markets, setMarkets] = useState<MarketView[]>([]);
  const [remote, setRemote] = useState<DiscoveredToken[]>([]);
  const [loading, setLoading] = useState(false);
  const [discovering, setDiscovering] = useState(false);

  // detail state
  const [detail, setDetail] = useState<MarketView | null>(null);
  const [history, setHistory] = useState<PricePoint[]>([]);
  const [position, setPosition] = useState(0);
  const [buyBudget, setBuyBudget] = useState("");
  const [buyPreview, setBuyPreview] = useState<BuyPreview | null>(null);
  const [sellAmount, setSellAmount] = useState("");
  const [sellPreview, setSellPreview] = useState<SellPreview | null>(null);
  const [busy, setBusy] = useState(false);
  const [tapdAssets, setTapdAssets] = useState<TapdAsset[]>([]);
  const [withdrawAddr, setWithdrawAddr] = useState("");
  const [withdrawAmount, setWithdrawAmount] = useState("");
  const [withdrawFee, setWithdrawFee] = useState("1");
  // The trader's ledger account id = their Nostr pubkey (derived from the seed).
  // Falls back to "me" if the wallet is locked / identity not derived yet.
  const [me, setMe] = useState("me");
  const [npub, setNpub] = useState("");

  // create form
  const [form, setForm] = useState({
    token_id: "",
    ticker: "",
    name: "",
    initialPrice: "1",
    finalPrice: "100",
    supplyCap: "1000000",
    feePercent: "1",
    seedSats: "0",
    migrationSats: "0",
    visibility: "public" as Visibility,
  });

  useEffect(() => {
    loadMarkets();
    discover();
  }, []);

  // Resolve the local Nostr identity so trades are keyed by the real pubkey.
  useEffect(() => {
    invoke<{ pubkey_hex: string; npub: string }>("get_nostr_identity")
      .then((id) => {
        setMe(id.pubkey_hex);
        setNpub(id.npub);
      })
      .catch(() => {});
  }, []);

  // When opening the create form, pull the node's real minted Taproot assets so
  // the user can launch a market for one (minting itself stays in the Assets
  // screen — it's an async on-chain batch). Silent on failure (tapd offline).
  useEffect(() => {
    if (view !== "create") return;
    let cancelled = false;
    invoke<TapdAsset[]>("list_taproot_assets")
      .then((a) => !cancelled && setTapdAssets(a))
      .catch(() => !cancelled && setTapdAssets([]));
    return () => {
      cancelled = true;
    };
  }, [view]);

  async function loadMarkets() {
    setLoading(true);
    try {
      setMarkets(await invoke<MarketView[]>("market_list"));
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setLoading(false);
    }
  }

  // Pull the public catalogue announced by other desks on Nostr. Best-effort:
  // relays can be slow/unreachable, so failures are silent (no nagging).
  async function discover() {
    setDiscovering(true);
    try {
      setRemote(await invoke<DiscoveredToken[]>("market_discover"));
    } catch (e) {
      console.error("discover error:", e);
    } finally {
      setDiscovering(false);
    }
  }

  async function openDetail(tokenId: string) {
    setBuyBudget("");
    setBuyPreview(null);
    setSellAmount("");
    setSellPreview(null);
    setView("detail");
    await refreshDetail(tokenId);
  }

  async function refreshDetail(tokenId: string) {
    try {
      const [m, h, pos] = await Promise.all([
        invoke<MarketView>("market_get", { tokenId }),
        invoke<PricePoint[]>("market_price_history", { tokenId }),
        invoke<number>("market_position", { tokenId, user: me }),
      ]);
      setDetail(m);
      setHistory(h);
      setPosition(pos);
    } catch (e) {
      notify(String(e), "error");
    }
  }

  // live buy quote
  useEffect(() => {
    if (view !== "detail" || !detail) return;
    const budget = Number(buyBudget);
    if (!Number.isFinite(budget) || budget <= 0) {
      setBuyPreview(null);
      return;
    }
    let cancelled = false;
    invoke<BuyPreview>("market_quote_buy", { tokenId: detail.token_id, budgetSats: Math.floor(budget) })
      .then((p) => !cancelled && setBuyPreview(p))
      .catch(() => !cancelled && setBuyPreview(null));
    return () => {
      cancelled = true;
    };
  }, [buyBudget, detail, view]);

  // live sell quote
  useEffect(() => {
    if (view !== "detail" || !detail) return;
    const amount = Number(sellAmount);
    if (!Number.isFinite(amount) || amount <= 0) {
      setSellPreview(null);
      return;
    }
    let cancelled = false;
    invoke<SellPreview>("market_quote_sell", { tokenId: detail.token_id, user: me, amount: Math.floor(amount) })
      .then((p) => !cancelled && setSellPreview(p))
      .catch(() => !cancelled && setSellPreview(null));
    return () => {
      cancelled = true;
    };
  }, [sellAmount, detail, view, me]);

  async function doBuy() {
    if (!detail || !buyPreview) return;
    setBusy(true);
    try {
      await invoke("market_buy", { tokenId: detail.token_id, user: me, budgetSats: Math.floor(Number(buyBudget)) });
      notify(`Acheté ${buyPreview.tokens.toLocaleString()} ${detail.ticker}`, "success");
      setBuyBudget("");
      setBuyPreview(null);
      await refreshDetail(detail.token_id);
      await loadMarkets();
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  }

  async function doSell() {
    if (!detail || !sellPreview) return;
    setBusy(true);
    try {
      await invoke("market_sell", { tokenId: detail.token_id, user: me, amount: Math.floor(Number(sellAmount)) });
      notify(`Vendu ${sellPreview.tokens.toLocaleString()} ${detail.ticker} → ${sellPreview.payout_sats.toLocaleString()} sats`, "success");
      setSellAmount("");
      setSellPreview(null);
      await refreshDetail(detail.token_id);
      await loadMarkets();
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  }

  async function doWithdraw() {
    if (!detail) return;
    const amt = Math.floor(Number(withdrawAmount));
    if (!withdrawAddr.trim() || !Number.isFinite(amt) || amt <= 0) {
      notify("Adresse Taproot et quantité requises", "error");
      return;
    }
    setBusy(true);
    try {
      const txid = await invoke<string>("market_withdraw_asset", {
        tokenId: detail.token_id,
        user: me,
        amount: amt,
        address: withdrawAddr.trim(),
        feeRateSatVb: Math.max(1, Math.floor(Number(withdrawFee) || 1)),
      });
      notify(`Retrait envoyé · tx ${txid.slice(0, 12)}…`, "success");
      setWithdrawAddr("");
      setWithdrawAmount("");
      await refreshDetail(detail.token_id);
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  }

  async function togglePause() {
    if (!detail) return;
    const paused = detail.status !== "paused";
    try {
      await invoke("market_set_paused", { tokenId: detail.token_id, paused });
      await refreshDetail(detail.token_id);
      notify(paused ? "Trading en pause" : "Trading repris", "success");
    } catch (e) {
      notify(String(e), "error");
    }
  }

  async function createToken() {
    if (!form.token_id.trim() || !form.ticker.trim() || !form.name.trim()) {
      notify("Ticker, nom et asset ID requis", "error");
      return;
    }
    const cap = parseInt(form.supplyCap, 10);
    if (!Number.isFinite(cap) || cap <= 0) {
      notify("Supply max invalide", "error");
      return;
    }
    const p0 = Math.round(parseFloat(form.initialPrice || "0") * DENOM);
    const finalP = Math.round(parseFloat(form.finalPrice || "0") * DENOM);
    const k = Math.max(0, Math.round((finalP - p0) / cap));
    const spec = {
      token_id: form.token_id.trim(),
      ticker: form.ticker.trim().toUpperCase(),
      name: form.name.trim(),
      creator: me,
      params: {
        p0_num: p0,
        k_num: k,
        denom: DENOM,
        supply_cap: cap,
        migration_sats: parseInt(form.migrationSats, 10) || 0,
      },
      visibility: form.visibility,
      creator_fee_bp: Math.round(parseFloat(form.feePercent || "0") * 100),
      seed_sats: parseInt(form.seedSats, 10) || 0,
    };
    setBusy(true);
    try {
      await invoke("market_create", { spec });
      notify(`Marché ${spec.ticker} créé`, "success");
      // Public tokens are auto-announced on Nostr so everyone can discover them.
      if (spec.visibility === "public") {
        invoke("market_publish", { tokenId: spec.token_id })
          .then(() => notify("Annoncé sur Nostr — découvrable par tous", "success"))
          .catch((e) => notify(`Annonce Nostr différée : ${e}`, "error"));
      }
      setForm((f) => ({ ...f, token_id: "", ticker: "", name: "", seedSats: "0" }));
      setView("list");
      await loadMarkets();
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  }

  const isCreator = detail?.creator === me;
  // Remote tokens = discovered on Nostr, not run by this desk and not already
  // in the local list.
  const remoteOnly = remote.filter(
    (d) => d.desk_pubkey !== me && !markets.some((m) => m.token_id === d.ann.asset_id)
  );

  return (
    <div style={{ width: "100%", height: "100%", display: "flex", flexDirection: "column", padding: "24px", overflow: "auto" }}>
      <motion.div
        initial={{ opacity: 0, y: -10 }}
        animate={{ opacity: 1, y: 0 }}
        style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 24 }}
      >
        <div>
          <h1 className="title-lg">Marché</h1>
          <p className="text-muted">
            Tokens Taproot · bonding curve{npub && ` · ${npub.slice(0, 12)}…`}
          </p>
        </div>
        <button className="btn btn-ghost" onClick={view === "list" ? onBack : () => setView("list")}>
          <ArrowLeft size={18} /> {view === "list" ? "Retour" : "Marché"}
        </button>
      </motion.div>

      {/* ---------------- LIST ---------------- */}
      {view === "list" && (
        <>
          <div style={{ display: "flex", gap: 10, marginBottom: 20 }}>
            <button className="btn btn-primary" onClick={() => setView("create")}>
              <Plus size={16} /> Créer un token
            </button>
            <button
              className="btn btn-ghost"
              onClick={() => {
                loadMarkets();
                discover();
              }}
              disabled={loading || discovering}
            >
              {loading || discovering ? <span className="spinner" /> : <RefreshCw size={16} />} Actualiser
            </button>
          </div>
          {markets.length === 0 && remoteOnly.length === 0 ? (
            <div className="glass-card" style={{ padding: 32, textAlign: "center" }}>
              <Store size={28} style={{ opacity: 0.5, marginBottom: 8 }} />
              <div className="text-muted">
                {discovering ? "Recherche sur Nostr…" : "Aucun token pour l'instant."}
              </div>
              <div className="text-muted" style={{ fontSize: 12, marginTop: 4 }}>
                Crée le premier avec « Créer un token » — il sera annoncé sur Nostr.
              </div>
            </div>
          ) : (
            <>
              {markets.length > 0 && (
                <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
                  {markets.map((m) => (
                    <MarketRow key={m.token_id} m={m} onClick={() => openDetail(m.token_id)} />
                  ))}
                </div>
              )}
              {remoteOnly.length > 0 && (
                <>
                  <div className="text-muted" style={{ fontSize: 12, margin: "20px 0 10px" }}>
                    <Globe size={13} style={{ verticalAlign: "middle", marginRight: 6 }} />
                    Tokens distants (Nostr){discovering ? " · …" : ""}
                  </div>
                  <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
                    {remoteOnly.map((d) => (
                      <RemoteRow
                        key={`${d.desk_pubkey}:${d.ann.asset_id}`}
                        d={d}
                        onClick={() =>
                          notify("Trading des tokens distants : bientôt (règlement Lightning, Phase D)", "info")
                        }
                      />
                    ))}
                  </div>
                </>
              )}
            </>
          )}
        </>
      )}

      {/* ---------------- DETAIL ---------------- */}
      {view === "detail" && detail && (
        <>
          <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} className="glass-card" style={{ padding: 24, marginBottom: 20 }}>
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "flex-start" }}>
              <div>
                <div style={{ fontSize: 24, fontWeight: 700 }}>{detail.ticker}</div>
                <div className="text-muted" style={{ fontSize: 13 }}>{detail.name}</div>
              </div>
              <StatusBadge status={detail.status} />
            </div>
            <div
              style={{
                fontSize: 34,
                fontWeight: 700,
                marginTop: 12,
                background: "linear-gradient(135deg, #fff, #00f0ff)",
                WebkitBackgroundClip: "text",
                WebkitTextFillColor: "transparent",
              }}
            >
              {priceSat(detail.spot_price_msat)} sat
            </div>
            <div className="text-secondary" style={{ fontSize: 13 }}>
              Cap {detail.reserve_sats.toLocaleString()} sat · {detail.supply.toLocaleString()} en circulation · {detail.trade_count} trades
              {detail.withdrawn > 0 && ` · ${detail.withdrawn.toLocaleString()} hors custody`}
            </div>
            <ProgressBar bp={detail.progress_bp} />
            {isCreator && (
              <div style={{ display: "flex", gap: 10, marginTop: 14, flexWrap: "wrap", alignItems: "center" }}>
                {detail.status !== "migrated" && (
                  <button className="btn btn-ghost" onClick={togglePause}>
                    {detail.status === "paused" ? <Play size={16} /> : <Pause size={16} />}
                    {detail.status === "paused" ? "Reprendre" : "Pause"}
                  </button>
                )}
                <span className="text-muted" style={{ fontSize: 12 }}>
                  Créateur · frais {(detail.creator_fee_bp / 100).toFixed(2)}% · gagnés {detail.creator_fees_sats.toLocaleString()} sat
                </span>
              </div>
            )}
          </motion.div>

          <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ delay: 0.05 }} className="glass-card" style={{ padding: 20, marginBottom: 20 }}>
            <div className="text-secondary" style={{ marginBottom: 8, display: "flex", justifyContent: "space-between", alignItems: "center" }}>
              <span>Prix</span>
              <button className="btn btn-ghost" style={{ fontSize: 13 }} onClick={() => refreshDetail(detail.token_id)}>
                <RefreshCw size={14} /> Actualiser
              </button>
            </div>
            <PriceChart points={history} />
          </motion.div>

          {/* buy */}
          {detail.status === "trading" && (
            <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ delay: 0.1 }} className="glass-card" style={{ padding: 20, marginBottom: 20 }}>
              <div className="text-secondary" style={{ marginBottom: 12 }}>
                <TrendingUp size={16} style={{ marginRight: 8, verticalAlign: "middle", color: "#00f0ff" }} />
                Acheter
              </div>
              <input
                className="input"
                type="number"
                placeholder="Budget (sats)"
                value={buyBudget}
                onChange={(e) => setBuyBudget(e.target.value)}
                style={{ marginBottom: 10 }}
              />
              {buyPreview && (
                <div className="text-muted" style={{ fontSize: 13, marginBottom: 12 }}>
                  ≈ <b>{buyPreview.tokens.toLocaleString()} {detail.ticker}</b> · coût {buyPreview.cost_sats.toLocaleString()} sat
                  {buyPreview.fee_sats > 0 && ` + ${buyPreview.fee_sats.toLocaleString()} frais`} · prix moyen {priceSat(buyPreview.avg_price_msat)} sat
                </div>
              )}
              <button className="btn btn-primary" onClick={doBuy} disabled={busy || !buyPreview || buyPreview.tokens === 0}>
                <TrendingUp size={16} /> Acheter
              </button>
            </motion.div>
          )}

          {/* sell */}
          {detail.status !== "paused" && (
            <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ delay: 0.12 }} className="glass-card" style={{ padding: 20, marginBottom: 20 }}>
              <div className="text-secondary" style={{ marginBottom: 12, display: "flex", justifyContent: "space-between" }}>
                <span>
                  <TrendingDown size={16} style={{ marginRight: 8, verticalAlign: "middle", color: "#ff4466" }} />
                  Vendre
                </span>
                <span className="text-muted" style={{ fontSize: 12 }}>
                  Position : {position.toLocaleString()} {detail.ticker}
                </span>
              </div>
              <div style={{ display: "flex", gap: 8, marginBottom: 10 }}>
                <input
                  className="input"
                  type="number"
                  placeholder={`Quantité (${detail.ticker})`}
                  value={sellAmount}
                  onChange={(e) => setSellAmount(e.target.value)}
                  style={{ flex: 1 }}
                />
                <button className="btn btn-ghost" style={{ flex: "none" }} onClick={() => setSellAmount(String(position))} disabled={position === 0}>
                  Max
                </button>
              </div>
              {sellPreview && (
                <div className="text-muted" style={{ fontSize: 13, marginBottom: 12 }}>
                  ≈ <b>{sellPreview.payout_sats.toLocaleString()} sat</b>
                  {sellPreview.fee_sats > 0 && ` (dont ${sellPreview.fee_sats.toLocaleString()} frais)`} · prix moyen {priceSat(sellPreview.avg_price_msat)} sat
                </div>
              )}
              <button className="btn btn-secondary" onClick={doSell} disabled={busy || !sellPreview || position === 0}>
                <TrendingDown size={16} /> Vendre
              </button>
            </motion.div>
          )}

          {/* withdraw the custodial token balance on-chain */}
          {position > 0 && (
            <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ delay: 0.14 }} className="glass-card" style={{ padding: 20, marginBottom: 20 }}>
              <div className="text-secondary" style={{ marginBottom: 12 }}>
                <Download size={16} style={{ marginRight: 8, verticalAlign: "middle" }} />
                Retirer on-chain
              </div>
              <input
                className="input"
                placeholder="Adresse Taproot du destinataire (encode ce montant)"
                value={withdrawAddr}
                onChange={(e) => setWithdrawAddr(e.target.value)}
                style={{ marginBottom: 10, fontFamily: "var(--font-mono)", fontSize: 12 }}
              />
              <div style={{ display: "grid", gridTemplateColumns: "2fr 1fr", gap: 8, marginBottom: 10 }}>
                <input
                  className="input"
                  type="number"
                  placeholder={`Quantité (${detail.ticker})`}
                  value={withdrawAmount}
                  onChange={(e) => setWithdrawAmount(e.target.value)}
                />
                <input
                  className="input"
                  type="number"
                  placeholder="Frais sat/vB"
                  value={withdrawFee}
                  onChange={(e) => setWithdrawFee(e.target.value)}
                />
              </div>
              <div className="text-muted" style={{ fontSize: 11, marginBottom: 12 }}>
                Sort le token de la custody du desk vers ta propre adresse tap. Il reste en circulation (adossé à la réserve) — génère l'adresse pour ce montant exact de cet asset.
              </div>
              <button className="btn btn-secondary" onClick={doWithdraw} disabled={busy || position === 0}>
                <Download size={16} /> Retirer
              </button>
            </motion.div>
          )}

          {detail.status === "migrated" && (
            <div className="glass-card" style={{ padding: 16, marginBottom: 20, fontSize: 13, display: "flex", gap: 10, alignItems: "center" }}>
              <Rocket size={18} style={{ color: "#a855f7", flexShrink: 0 }} />
              <span className="text-muted">
                Courbe pleine — ce token a <b>migré</b>. Les achats sur la courbe sont fermés ; les ventes restent ouvertes le temps de la sortie. Le carnet P2P (échange libre) arrive en V3.
              </span>
            </div>
          )}
        </>
      )}

      {/* ---------------- CREATE ---------------- */}
      {view === "create" && (
        <motion.div initial={{ opacity: 0 }} animate={{ opacity: 1 }} className="glass-card" style={{ padding: 20, marginBottom: 20 }}>
          <div className="text-secondary" style={{ marginBottom: 14 }}>
            <Plus size={16} style={{ marginRight: 8, verticalAlign: "middle" }} />
            Créer un token
          </div>

          {tapdAssets.length > 0 ? (
            <div style={{ marginBottom: 14 }}>
              <label className="text-muted" style={{ fontSize: 12 }}>Depuis un asset tapd minté</label>
              <div style={{ display: "flex", flexWrap: "wrap", gap: 8, marginTop: 6 }}>
                {tapdAssets.map((a) => (
                  <button
                    key={a.asset_id}
                    className={form.token_id === a.asset_id ? "btn btn-primary" : "btn btn-ghost"}
                    style={{ fontSize: 12 }}
                    title={a.asset_id}
                    onClick={() =>
                      setForm((f) => ({
                        ...f,
                        token_id: a.asset_id,
                        name: f.name || a.name,
                        ticker: f.ticker || a.name.slice(0, 6).toUpperCase(),
                        supplyCap: String(a.amount),
                      }))
                    }
                  >
                    {a.name} · {a.amount.toLocaleString()}
                  </button>
                ))}
              </div>
            </div>
          ) : (
            <div className="text-muted" style={{ fontSize: 11, marginBottom: 12 }}>
              Aucun asset tapd détecté (nœud non connecté ou aucun mint). Minte d'abord dans l'onglet « Assets », puis reviens lancer son marché — ou saisis un identifiant pour tester.
            </div>
          )}

          <label className="text-muted" style={{ fontSize: 12 }}>Ticker</label>
          <input className="input" placeholder="OZ" value={form.ticker} onChange={(e) => setForm({ ...form, ticker: e.target.value })} style={{ margin: "4px 0 12px" }} />

          <label className="text-muted" style={{ fontSize: 12 }}>Nom</label>
          <input className="input" placeholder="OZark Token" value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} style={{ margin: "4px 0 12px" }} />

          <label className="text-muted" style={{ fontSize: 12 }}>Asset ID Taproot (déjà minté — ou un identifiant pour tester)</label>
          <input className="input" placeholder="asset id / id de test" value={form.token_id} onChange={(e) => setForm({ ...form, token_id: e.target.value })} style={{ margin: "4px 0 12px", fontFamily: "var(--font-mono)", fontSize: 12 }} />

          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 10 }}>
            <div>
              <label className="text-muted" style={{ fontSize: 12 }}>Prix initial (sat)</label>
              <input className="input" type="number" value={form.initialPrice} onChange={(e) => setForm({ ...form, initialPrice: e.target.value })} style={{ marginTop: 4 }} />
            </div>
            <div>
              <label className="text-muted" style={{ fontSize: 12 }}>Prix à saturation (sat)</label>
              <input className="input" type="number" value={form.finalPrice} onChange={(e) => setForm({ ...form, finalPrice: e.target.value })} style={{ marginTop: 4 }} />
            </div>
            <div>
              <label className="text-muted" style={{ fontSize: 12 }}>Supply max (tokens)</label>
              <input className="input" type="number" value={form.supplyCap} onChange={(e) => setForm({ ...form, supplyCap: e.target.value })} style={{ marginTop: 4 }} />
            </div>
            <div>
              <label className="text-muted" style={{ fontSize: 12 }}>Frais créateur (%)</label>
              <input className="input" type="number" value={form.feePercent} onChange={(e) => setForm({ ...form, feePercent: e.target.value })} style={{ marginTop: 4 }} />
            </div>
            <div>
              <label className="text-muted" style={{ fontSize: 12 }}>Seed créateur (sat, 0 = fair launch)</label>
              <input className="input" type="number" value={form.seedSats} onChange={(e) => setForm({ ...form, seedSats: e.target.value })} style={{ marginTop: 4 }} />
            </div>
            <div>
              <label className="text-muted" style={{ fontSize: 12 }}>Objectif migration (sat, 0 = off)</label>
              <input className="input" type="number" value={form.migrationSats} onChange={(e) => setForm({ ...form, migrationSats: e.target.value })} style={{ marginTop: 4 }} />
            </div>
          </div>

          <div style={{ display: "flex", gap: 8, margin: "14px 0" }}>
            {(["public", "private"] as Visibility[]).map((v) => (
              <button
                key={v}
                className={form.visibility === v ? "btn btn-primary" : "btn btn-ghost"}
                style={{ flex: 1 }}
                onClick={() => setForm({ ...form, visibility: v })}
              >
                {v === "public" ? "Marketplace (public)" : "Privé"}
              </button>
            ))}
          </div>
          <div className="text-muted" style={{ fontSize: 11, marginBottom: 14 }}>
            Public = listé + courbe active. Privé = token créé mais hors marché. La réserve démarre à zéro et se remplit avec les acheteurs — pas de premine.
          </div>

          <div style={{ display: "flex", gap: 10 }}>
            <button className="btn btn-primary" onClick={createToken} disabled={busy}>
              {busy ? <span className="spinner" /> : <Rocket size={16} />} Créer
            </button>
            <button className="btn btn-ghost" onClick={() => setView("list")}>Annuler</button>
          </div>
        </motion.div>
      )}
    </div>
  );
}

function StatusBadge({ status }: { status: Status }) {
  const { label, color } = statusMeta(status);
  return (
    <span
      style={{
        fontSize: 11,
        padding: "3px 10px",
        borderRadius: 999,
        border: `1px solid ${color}`,
        color,
        background: `${color}1a`,
        whiteSpace: "nowrap",
      }}
    >
      {label}
    </span>
  );
}

function ProgressBar({ bp }: { bp: number }) {
  const pct = Math.min(100, bp / 100);
  return (
    <div style={{ marginTop: 12 }}>
      <div style={{ height: 6, borderRadius: 999, background: "rgba(255,255,255,0.08)", overflow: "hidden" }}>
        <div style={{ width: `${pct}%`, height: "100%", background: "linear-gradient(90deg,#00f0ff,#a855f7)" }} />
      </div>
      <div className="text-muted" style={{ fontSize: 11, marginTop: 4 }}>
        Courbe {pct.toFixed(0)}%{bp >= 10000 ? " · prête à migrer ⚡" : ""}
      </div>
    </div>
  );
}

function MarketRow({ m, onClick }: { m: MarketView; onClick: () => void }) {
  return (
    <div
      onClick={onClick}
      role="button"
      className="glass-card"
      style={{ padding: 16, cursor: "pointer", display: "flex", alignItems: "center", gap: 14 }}
    >
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ fontWeight: 700 }}>{m.ticker}</span>
          <StatusBadge status={m.status} />
        </div>
        <div className="text-muted" style={{ fontSize: 12, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
          {m.name}
        </div>
        <div style={{ height: 4, borderRadius: 999, background: "rgba(255,255,255,0.08)", overflow: "hidden", marginTop: 8 }}>
          <div style={{ width: `${Math.min(100, m.progress_bp / 100)}%`, height: "100%", background: "linear-gradient(90deg,#00f0ff,#a855f7)" }} />
        </div>
      </div>
      <div style={{ textAlign: "right", flexShrink: 0 }}>
        <div style={{ fontWeight: 600 }}>{priceSat(m.spot_price_msat)} sat</div>
        <div className="text-muted" style={{ fontSize: 11 }}>{m.reserve_sats.toLocaleString()} sat cap</div>
      </div>
      <ChevronRight size={18} className="text-muted" style={{ flexShrink: 0 }} />
    </div>
  );
}

function RemoteRow({ d, onClick }: { d: DiscoveredToken; onClick: () => void }) {
  const a = d.ann;
  const progress = a.migration_sats > 0 ? Math.min(100, (a.reserve_sats * 100) / a.migration_sats) : 0;
  return (
    <div
      onClick={onClick}
      role="button"
      className="glass-card"
      style={{ padding: 16, cursor: "pointer", display: "flex", alignItems: "center", gap: 14, opacity: 0.92 }}
    >
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <span style={{ fontWeight: 700 }}>{a.ticker}</span>
          <span
            style={{
              fontSize: 11,
              padding: "3px 10px",
              borderRadius: 999,
              border: "1px solid #a855f7",
              color: "#a855f7",
              background: "#a855f71a",
              whiteSpace: "nowrap",
            }}
          >
            distant
          </span>
        </div>
        <div className="text-muted" style={{ fontSize: 12, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
          {a.name}
        </div>
        <div style={{ height: 4, borderRadius: 999, background: "rgba(255,255,255,0.08)", overflow: "hidden", marginTop: 8 }}>
          <div style={{ width: `${progress}%`, height: "100%", background: "linear-gradient(90deg,#00f0ff,#a855f7)" }} />
        </div>
      </div>
      <div style={{ textAlign: "right", flexShrink: 0 }}>
        <div style={{ fontWeight: 600 }}>{priceSat(a.spot_price_msat)} sat</div>
        <div className="text-muted" style={{ fontSize: 11 }}>{a.reserve_sats.toLocaleString()} sat cap</div>
      </div>
      <ChevronRight size={18} className="text-muted" style={{ flexShrink: 0 }} />
    </div>
  );
}
