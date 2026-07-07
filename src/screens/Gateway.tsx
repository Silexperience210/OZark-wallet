import { useEffect, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  ArrowLeft,
  RefreshCw,
  Server,
  Coins,
  Download,
  Send,
  Flame,
  ArrowLeftRight,
  Copy,
  Check,
  Info,
  Clock,
  ScanLine,
} from "lucide-react";
import { useNotification } from "../contexts/NotificationContext";
import { QRImage } from "../components/QRImage";
import { scanQrCode } from "../lib/scan";

interface GatewayProps {
  onBack: () => void;
}

interface HeldAsset {
  asset_id: string;
  name: string;
  amount: number;
  asset_type: string;
  decimal_display: number;
}

interface MintResp {
  batch_key: string;
  batch_txid: string;
  status: string;
}

interface MintStatus {
  status: string;
  batch_txid: string;
  asset_id: string | null;
}

interface ReceiveResp {
  addr: string;
}

interface TxResp {
  txid: string;
}

interface GatewayConfig {
  base_url: string;
}

interface AssetMeta {
  data: string;
  meta_type: string;
  meta_hash: string;
  decimal_display: number;
}

interface NodeInfo {
  version: string;
  lnd_version: string;
  network: string;
}

interface LedgerEvent {
  id: number;
  asset_id: string;
  kind: string;
  amount: number;
  counterparty: string | null;
  reference: string | null;
  created_at: number;
}

interface RfqQuotes {
  buy_quotes: number;
  sell_quotes: number;
}

interface DecodedAssetInvoice {
  asset_amount: number;
  sat_amount: number;
  description: string;
  destination: string;
}

interface RfqQuote {
  peer: string;
  rate_coefficient: string;
  rate_scale: number;
  expiry: number;
}

/// Human rate from an RFQ quote: coefficient/10^scale = asset units per BTC.
function formatRfqRate(q: RfqQuote): string {
  try {
    const coeff = BigInt(q.rate_coefficient || "0");
    if (coeff === 0n) return "taux indisponible";
    const satsPerBtcScaled = 100_000_000 * 10 ** q.rate_scale; // 1e8 * 10^scale
    const satsPerUnit = satsPerBtcScaled / Number(coeff);
    return satsPerUnit >= 1
      ? `1 unité ≈ ${satsPerUnit.toFixed(2)} sats`
      : `1 sat ≈ ${(Number(coeff) / satsPerBtcScaled).toFixed(2)} unités`;
  } catch {
    return "taux indisponible";
  }
}

interface ChannelInfo {
  active: boolean;
  peer: string;
  chan_id: string;
  capacity: number;
  local_balance: number;
  remote_balance: number;
}

interface FeeQuote {
  network_sats: number;
  margin_sats: number;
  total_sats: number;
  charged: boolean;
}

const card: CSSProperties = { padding: 18, marginBottom: 14 };
const label: CSSProperties = { fontSize: 12, fontWeight: 600, marginBottom: 6, display: "block" };
const row: CSSProperties = { display: "flex", gap: 8, marginBottom: 8 };

function num(v: string): number {
  return Math.floor(Number(v) || 0);
}

function short(s: string, head = 10, tail = 6): string {
  return s.length > head + tail + 1 ? `${s.slice(0, head)}…${s.slice(-tail)}` : s;
}

const CREDIT_KINDS = new Set(["mint", "receive", "ln_receive", "transfer_in"]);

function kindLabel(kind: string): string {
  switch (kind) {
    case "mint":
      return "Mint";
    case "receive":
      return "Reçu";
    case "send":
      return "Envoyé";
    case "ln_send":
      return "Envoyé (LN)";
    case "ln_receive":
      return "Reçu (LN)";
    case "burn":
      return "Brûlé";
    case "transfer_in":
      return "Transfert reçu";
    case "transfer_out":
      return "Transfert envoyé";
    default:
      return kind;
  }
}

export function Gateway({ onBack }: GatewayProps) {
  const { notify } = useNotification();

  const [url, setUrl] = useState("");
  const [savedUrl, setSavedUrl] = useState("");

  const [assets, setAssets] = useState<HeldAsset[]>([]);
  const [loading, setLoading] = useState(false);

  // Node info + per-asset metadata (read-only)
  const [nodeInfo, setNodeInfo] = useState<NodeInfo | null>(null);
  // The wallet's own Nostr pubkey (identity signing every request; = operator key).
  const [myPubkey, setMyPubkey] = useState<{ hex: string; npub: string } | null>(null);
  const [pubkeyShown, setPubkeyShown] = useState(false);
  const [pubkeyCopied, setPubkeyCopied] = useState(false);
  const [metaFor, setMetaFor] = useState<string | null>(null);
  const [meta, setMeta] = useState<AssetMeta | null>(null);
  const [metaLoading, setMetaLoading] = useState(false);

  // Transaction history (per-user ledger)
  const [history, setHistory] = useState<LedgerEvent[]>([]);

  // Custodial sats balance (funds on-chain operation fees) + LN top-up
  const [satsBalance, setSatsBalance] = useState<number | null>(null);
  const [depositAmount, setDepositAmount] = useState("");
  const [depositInvoice, setDepositInvoice] = useState("");
  const [depositCopied, setDepositCopied] = useState(false);
  // Fee estimates for mint/send (default-rate quotes, refreshed on demand)
  const [mintQuote, setMintQuote] = useState<FeeQuote | null>(null);
  const [sendQuote, setSendQuote] = useState<FeeQuote | null>(null);

  // Lightning assets (read-only: RFQ health + invoice decode)
  const [rfq, setRfq] = useState<RfqQuotes | null>(null);
  const [lnPayReq, setLnPayReq] = useState("");
  const [lnAssetId, setLnAssetId] = useState("");
  // Optional fungible group key: decode/preview an invoice priced against a whole
  // asset group instead of one asset id (paying still needs a concrete asset id).
  const [lnGroupKey, setLnGroupKey] = useState("");
  const [lnDecoded, setLnDecoded] = useState<DecodedAssetInvoice | null>(null);
  // Lightning receive (create asset invoice)
  const [lnRcvAssetId, setLnRcvAssetId] = useState("");
  const [lnRcvAmount, setLnRcvAmount] = useState("");
  const [lnRcvInvoice, setLnRcvInvoice] = useState("");
  const [lnRcvQuote, setLnRcvQuote] = useState<RfqQuote | null>(null);
  const [lnRcvCopied, setLnRcvCopied] = useState(false);

  // Operator (admin) — asset channel management (only works if this wallet's
  // pubkey is the gateway's OZARK_GATEWAY_ADMIN_PUBKEY).
  const [opShown, setOpShown] = useState(false);
  const [channels, setChannels] = useState<ChannelInfo[] | null>(null);
  const [peerPubkey, setPeerPubkey] = useState("");
  const [peerHost, setPeerHost] = useState("");
  const [chAssetId, setChAssetId] = useState("");
  const [chAmount, setChAmount] = useState("");
  const [chPeer, setChPeer] = useState("");
  const [chFee, setChFee] = useState("");

  // Mint
  const [mintName, setMintName] = useState("");
  const [mintAmount, setMintAmount] = useState("");
  const [mintMeta, setMintMeta] = useState("");
  const [mintCollectible, setMintCollectible] = useState(false);
  const [mintGrouped, setMintGrouped] = useState(false);
  const [mintFee, setMintFee] = useState("");
  const [lastBatch, setLastBatch] = useState("");
  const [mintStatus, setMintStatus] = useState<string>("");

  // Receive
  const [rcvAssetId, setRcvAssetId] = useState("");
  const [rcvAmount, setRcvAmount] = useState("");
  const [rcvAddr, setRcvAddr] = useState("");
  const [copied, setCopied] = useState(false);

  // Send
  const [sendAddr, setSendAddr] = useState("");
  const [sendFee, setSendFee] = useState("");

  // Burn
  const [burnAssetId, setBurnAssetId] = useState("");
  const [burnAmount, setBurnAmount] = useState("");

  // Transfer
  const [xferAssetId, setXferAssetId] = useState("");
  const [xferTo, setXferTo] = useState("");
  const [xferAmount, setXferAmount] = useState("");

  const [busy, setBusy] = useState(false);

  useEffect(() => {
    invoke<GatewayConfig | null>("load_gateway_config")
      .then((cfg) => {
        if (cfg?.base_url) {
          setUrl(cfg.base_url);
          setSavedUrl(cfg.base_url);
        }
      })
      .catch(() => {});
  }, []);

  const saveUrl = async () => {
    try {
      await invoke("save_gateway_config", { baseUrl: url.trim() });
      setSavedUrl(url.trim());
      notify("Gateway enregistré", "success");
    } catch (e) {
      notify(String(e), "error");
    }
  };

  const refresh = async () => {
    setLoading(true);
    try {
      const a = await invoke<HeldAsset[]>("gateway_list_assets");
      setAssets(a);
      // Node info + history are best-effort: a failure must not hide balances.
      try {
        setNodeInfo(await invoke<NodeInfo>("gateway_info"));
      } catch {
        setNodeInfo(null);
      }
      // My Nostr pubkey is derived locally (no network) — never fatal.
      try {
        setMyPubkey(await invoke<{ hex: string; npub: string }>("gateway_pubkey"));
      } catch {
        setMyPubkey(null);
      }
      try {
        setHistory(await invoke<LedgerEvent[]>("gateway_history", { limit: 50 }));
      } catch {
        setHistory([]);
      }
      try {
        setRfq(await invoke<RfqQuotes>("gateway_ln_rfq_quotes"));
      } catch {
        setRfq(null);
      }
      // Sats balance + default fee estimates (best-effort; node may not charge).
      try {
        const b = await invoke<{ amount: number }>("gateway_sats_balance");
        setSatsBalance(b.amount);
      } catch {
        setSatsBalance(null);
      }
      setMintQuote(await quoteFee("mint", ""));
      setSendQuote(await quoteFee("send", ""));
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setLoading(false);
    }
  };

  // Fetch a fee quote for `op` at an optional custom rate. Best-effort: returns
  // null on failure so a fee-estimate failure never blocks the screen.
  const quoteFee = async (op: "mint" | "send", rate: string): Promise<FeeQuote | null> => {
    try {
      return await invoke<FeeQuote>("gateway_fee_quote", {
        op,
        feeRateSatVb: rate.trim() ? num(rate) : null,
      });
    } catch {
      return null;
    }
  };

  const doDeposit = async () => {
    const amt = Number(depositAmount);
    if (!Number.isFinite(amt) || amt <= 0) return notify("Montant en sats requis", "error");
    setBusy(true);
    setDepositInvoice("");
    try {
      const r = await invoke<{ payment_request: string; r_hash: string }>("gateway_sats_deposit", {
        amountSats: Math.floor(amt),
      });
      setDepositInvoice(r.payment_request);
      notify("Facture créée — ton solde sats est crédité au règlement", "success");
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const copyDeposit = async () => {
    try {
      await navigator.clipboard.writeText(depositInvoice);
      setDepositCopied(true);
      setTimeout(() => setDepositCopied(false), 1500);
    } catch {
      /* ignore */
    }
  };

  // Toggle the metadata panel for one asset, fetching it on first open.
  const toggleMeta = async (assetId: string) => {
    if (metaFor === assetId) {
      setMetaFor(null);
      setMeta(null);
      return;
    }
    setMetaFor(assetId);
    setMeta(null);
    setMetaLoading(true);
    try {
      setMeta(await invoke<AssetMeta>("gateway_asset_meta", { assetId }));
    } catch (e) {
      notify(String(e), "error");
      setMetaFor(null);
    } finally {
      setMetaLoading(false);
    }
  };

  const doLnDecode = async () => {
    if (!lnPayReq.trim() || (!lnAssetId.trim() && !lnGroupKey.trim()))
      return notify("Facture + asset id (ou group key) requis", "error");
    setBusy(true);
    setLnDecoded(null);
    try {
      setLnDecoded(
        await invoke<DecodedAssetInvoice>("gateway_ln_decode", {
          payReq: lnPayReq.trim(),
          assetId: lnAssetId.trim() || null,
          groupKey: lnGroupKey.trim() || null,
        }),
      );
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doLnPay = async () => {
    if (!lnPayReq.trim() || !lnAssetId.trim())
      return notify("Facture et asset id requis", "error");
    setBusy(true);
    try {
      const r = await invoke<{ status: string; asset_amount: number }>("gateway_ln_pay", {
        payReq: lnPayReq.trim(),
        assetId: lnAssetId.trim(),
        peerPubkey: null,
      });
      notify(`Paiement ${r.status} — ${r.asset_amount.toLocaleString()} unités`, "success");
      setLnPayReq("");
      setLnDecoded(null);
      refresh();
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doLnReceive = async () => {
    const amt = Number(lnRcvAmount);
    if (!lnRcvAssetId.trim() || !Number.isFinite(amt) || amt <= 0)
      return notify("Asset id et montant requis", "error");
    setBusy(true);
    setLnRcvInvoice("");
    setLnRcvQuote(null);
    try {
      const r = await invoke<{ payment_request: string; r_hash: string; quote?: RfqQuote }>(
        "gateway_ln_receive",
        {
          assetId: lnRcvAssetId.trim(),
          assetAmount: amt,
          peerPubkey: null,
          memo: null,
        },
      );
      setLnRcvInvoice(r.payment_request);
      setLnRcvQuote(r.quote ?? null);
      notify("Facture Lightning créée — crédit à réception", "success");
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doClaimOperator = async () => {
    setBusy(true);
    try {
      await invoke("gateway_admin_claim");
      notify("Tu es désormais l'opérateur de ce nœud ✅", "success");
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doListChannels = async () => {
    setBusy(true);
    try {
      setChannels(await invoke<ChannelInfo[]>("gateway_admin_channels"));
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doConnectPeer = async () => {
    if (!peerPubkey.trim() || !peerHost.trim()) return notify("Pubkey et host requis", "error");
    setBusy(true);
    try {
      await invoke("gateway_admin_peer_connect", {
        pubkey: peerPubkey.trim(),
        host: peerHost.trim(),
      });
      notify("Pair connecté", "success");
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doOpenChannel = async () => {
    const amt = Number(chAmount);
    if (!chAssetId.trim() || !chPeer.trim() || !Number.isFinite(amt) || amt <= 0)
      return notify("Asset id, montant et pair requis", "error");
    setBusy(true);
    try {
      const r = await invoke<{ txid: string }>("gateway_admin_channel_open", {
        assetId: chAssetId.trim(),
        assetAmount: amt,
        peerPubkey: chPeer.trim(),
        feeRateSatVb: chFee.trim() ? Number(chFee) : null,
      });
      notify(`Canal ouvert — txid ${r.txid.slice(0, 12)}…`, "success");
      doListChannels();
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doMint = async () => {
    if (!mintName.trim()) return notify("Nom requis", "error");
    setBusy(true);
    setMintStatus("");
    try {
      const r = await invoke<MintResp>("gateway_mint", {
        name: mintName.trim(),
        amount: num(mintAmount),
        meta: mintMeta.trim() || null,
        collectible: mintCollectible,
        grouped: mintGrouped,
        feeRateSatVb: mintFee ? num(mintFee) : null,
      });
      setLastBatch(r.batch_key);
      notify("Mint diffusé — en attente de confirmation", "info");
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const checkMint = async () => {
    if (!lastBatch) return;
    try {
      const s = await invoke<MintStatus>("gateway_mint_status", { batchKey: lastBatch });
      if (s.status === "minted" && s.asset_id) {
        setMintStatus(`Minté ✅ — asset ${short(s.asset_id)}`);
        notify("Asset minté et crédité", "success");
        refresh();
      } else {
        setMintStatus("En attente de confirmation on-chain…");
      }
    } catch (e) {
      notify(String(e), "error");
    }
  };

  const doReceive = async () => {
    if (!rcvAssetId.trim() || num(rcvAmount) <= 0) return notify("Asset et montant requis", "error");
    setBusy(true);
    try {
      const r = await invoke<ReceiveResp>("gateway_receive", {
        assetId: rcvAssetId.trim(),
        amount: num(rcvAmount),
      });
      setRcvAddr(r.addr);
      notify("Adresse générée", "success");
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const copyAddr = async () => {
    try {
      await navigator.clipboard.writeText(rcvAddr);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      /* ignore */
    }
  };

  const copyLnInvoice = async () => {
    try {
      await navigator.clipboard.writeText(lnRcvInvoice);
      setLnRcvCopied(true);
      setTimeout(() => setLnRcvCopied(false), 1500);
    } catch {
      /* ignore */
    }
  };

  const copyPubkey = async () => {
    if (!myPubkey) return;
    try {
      await navigator.clipboard.writeText(myPubkey.hex);
      setPubkeyCopied(true);
      setTimeout(() => setPubkeyCopied(false), 1500);
    } catch {
      /* ignore */
    }
  };

  const doSend = async () => {
    if (!sendAddr.trim()) return notify("Adresse requise", "error");
    setBusy(true);
    try {
      const r = await invoke<TxResp>("gateway_send", {
        addr: sendAddr.trim(),
        feeRateSatVb: sendFee ? num(sendFee) : null,
      });
      notify(`Envoyé — tx ${short(r.txid)}`, "success");
      setSendAddr("");
      refresh();
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doBurn = async () => {
    if (!burnAssetId.trim() || num(burnAmount) <= 0) return notify("Asset et montant requis", "error");
    setBusy(true);
    try {
      const r = await invoke<TxResp>("gateway_burn", {
        assetId: burnAssetId.trim(),
        amount: num(burnAmount),
      });
      notify(`Brûlé — tx ${short(r.txid)}`, "success");
      refresh();
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const doTransfer = async () => {
    if (!xferAssetId.trim() || !xferTo.trim() || num(xferAmount) <= 0)
      return notify("Asset, destinataire et montant requis", "error");
    setBusy(true);
    try {
      await invoke("gateway_transfer", {
        assetId: xferAssetId.trim(),
        toPubkey: xferTo.trim(),
        amount: num(xferAmount),
      });
      notify("Transfert instantané effectué ⚡", "success");
      setXferAmount("");
      refresh();
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setBusy(false);
    }
  };

  const configured = savedUrl.trim().length > 0;

  // Compact fee line under mint/send. When the node doesn't charge, says so; when
  // it does, breaks down network + margin and flags an insufficient sats balance.
  const feeEstimate = (quote: FeeQuote | null) => {
    if (!quote) return null;
    if (!quote.charged) {
      return (
        <p className="text-muted" style={{ fontSize: 10, marginBottom: 8 }}>
          Frais désactivés sur ce nœud — aucun sat débité.
        </p>
      );
    }
    const insufficient = satsBalance != null && satsBalance < quote.total_sats;
    return (
      <p
        className={insufficient ? undefined : "text-muted"}
        style={{ fontSize: 10, marginBottom: 8, color: insufficient ? "#f87171" : undefined }}
      >
        Frais ~{quote.total_sats.toLocaleString()} sats (réseau{" "}
        {quote.network_sats.toLocaleString()} + marge {quote.margin_sats.toLocaleString()})
        {insufficient ? " · solde sats insuffisant, recharge d'abord" : ""}
      </p>
    );
  };

  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        flexDirection: "column",
        padding: 24,
        overflow: "auto",
      }}
    >
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "flex-start", marginBottom: 20 }}>
        <div>
          <h1 className="title-lg">Vault partagé</h1>
          <p className="text-muted" style={{ fontSize: 13 }}>
            Assets Taproot via le gateway — le nœud est partagé, tes avoirs sont isolés.
          </p>
        </div>
        <button className="btn btn-ghost" onClick={onBack}>
          <ArrowLeft size={18} /> Retour
        </button>
      </div>

      {/* Settings */}
      <div className="glass-card" style={card}>
        <label style={label}>
          <Server size={14} style={{ verticalAlign: "-2px", marginRight: 6 }} />
          Adresse onion du gateway
        </label>
        <div style={row}>
          <input
            className="input"
            style={{ flex: 1 }}
            placeholder="http://xxxxxxxx.onion"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
          />
          <button className="btn btn-primary" onClick={saveUrl} disabled={!url.trim()}>
            Enregistrer
          </button>
        </div>
        <p className="text-muted" style={{ fontSize: 11 }}>
          Le macaroon reste sur le nœud. L'app signe chaque requête avec ta clé Nostr (NIP-98).
        </p>
      </div>

      {!configured ? (
        <div className="glass-card" style={{ ...card, textAlign: "center" }}>
          <p className="text-muted">Configure l'adresse onion du gateway pour commencer.</p>
        </div>
      ) : (
        <>
          {/* Portfolio */}
          <div className="glass-card" style={card}>
            <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 10 }}>
              <strong>
                <Coins size={16} style={{ verticalAlign: "-3px", marginRight: 6 }} />
                Mes soldes
              </strong>
              <button className="btn btn-ghost" onClick={refresh} disabled={loading}>
                {loading ? <span className="spinner" /> : <RefreshCw size={16} />} Actualiser
              </button>
            </div>
            {nodeInfo && (
              <p className="text-muted" style={{ fontSize: 11, marginBottom: 10 }}>
                <Server size={11} style={{ verticalAlign: "-1px", marginRight: 4 }} />
                tapd {nodeInfo.version || "?"} · {nodeInfo.network || "?"}
              </p>
            )}
            {myPubkey && (
              <div style={{ marginBottom: 10 }}>
                <button
                  className="btn btn-ghost"
                  style={{ fontSize: 11, padding: "2px 8px" }}
                  onClick={() => setPubkeyShown((v) => !v)}
                >
                  🔑 Ma clé Nostr {pubkeyShown ? "▲" : "▼"}
                </button>
                {pubkeyShown && (
                  <div
                    style={{
                      marginTop: 6,
                      background: "rgba(255,255,255,0.03)",
                      borderRadius: 8,
                      padding: 8,
                    }}
                  >
                    <div className="text-muted" style={{ fontSize: 10, marginBottom: 4 }}>
                      Identité qui signe tes requêtes. Le <strong>hex</strong> est ta clé
                      opérateur (à mettre dans <code>OZARK_GATEWAY_ADMIN_PUBKEY</code>).
                    </div>
                    <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                      <code style={{ flex: 1, fontSize: 11, wordBreak: "break-all" }}>
                        {myPubkey.hex}
                      </code>
                      <button className="btn btn-ghost" onClick={copyPubkey}>
                        {pubkeyCopied ? "✓" : "Copier"}
                      </button>
                    </div>
                    <div
                      className="text-muted"
                      style={{ fontSize: 10, marginTop: 4, wordBreak: "break-all" }}
                    >
                      {myPubkey.npub}
                    </div>
                  </div>
                )}
              </div>
            )}
            {assets.length === 0 ? (
              <p className="text-muted" style={{ fontSize: 12 }}>
                Aucun solde. Mint ou reçois un asset ci-dessous.
              </p>
            ) : (
              <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
                {assets.map((a) => (
                  <div
                    key={a.asset_id}
                    style={{
                      background: "rgba(255,255,255,0.03)",
                      borderRadius: 10,
                      padding: "10px 12px",
                    }}
                  >
                    <div
                      style={{
                        display: "flex",
                        justifyContent: "space-between",
                        alignItems: "center",
                      }}
                    >
                      <div>
                        <div style={{ fontWeight: 600, fontSize: 14 }}>{a.name || "(sans nom)"}</div>
                        <div className="text-muted" style={{ fontSize: 11, fontFamily: "monospace" }}>
                          {short(a.asset_id)}
                        </div>
                      </div>
                      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
                        <div style={{ fontWeight: 700, fontSize: 16 }}>{a.amount.toLocaleString()}</div>
                        <button
                          className="btn btn-ghost"
                          style={{ padding: 6 }}
                          title="Détails de l'asset"
                          onClick={() => toggleMeta(a.asset_id)}
                        >
                          <Info size={15} />
                        </button>
                      </div>
                    </div>
                    {metaFor === a.asset_id && (
                      <div
                        style={{
                          marginTop: 8,
                          paddingTop: 8,
                          borderTop: "1px solid rgba(255,255,255,0.06)",
                          fontSize: 11,
                        }}
                      >
                        {metaLoading ? (
                          <span className="text-muted">
                            <span className="spinner" /> Chargement…
                          </span>
                        ) : meta ? (
                          <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
                            <div>
                              <span className="text-muted">Décimales : </span>
                              {meta.decimal_display}
                            </div>
                            <div>
                              <span className="text-muted">Type méta : </span>
                              {meta.meta_type}
                            </div>
                            {meta.data && (
                              <div style={{ wordBreak: "break-word" }}>
                                <span className="text-muted">Méta : </span>
                                {meta.data.length > 200 ? `${meta.data.slice(0, 200)}…` : meta.data}
                              </div>
                            )}
                            <div className="text-muted" style={{ fontFamily: "monospace" }}>
                              hash {short(meta.meta_hash)}
                            </div>
                          </div>
                        ) : (
                          <span className="text-muted">Aucune métadonnée.</span>
                        )}
                      </div>
                    )}
                  </div>
                ))}
              </div>
            )}
          </div>

          {/* Sats balance (funds on-chain operation fees) */}
          <div className="glass-card" style={card}>
            <div
              style={{
                display: "flex",
                justifyContent: "space-between",
                alignItems: "center",
                marginBottom: 6,
              }}
            >
              <strong>⚡ Solde sats (frais réseau)</strong>
              <span style={{ fontWeight: 700, fontSize: 16 }}>
                {satsBalance == null ? "—" : `${satsBalance.toLocaleString()} sats`}
              </span>
            </div>
            <p className="text-muted" style={{ fontSize: 11, marginBottom: 10 }}>
              Ces sats couvrent les frais on-chain de tes opérations (mint / envoi). Recharge en
              Lightning ; ton solde est crédité au règlement de la facture.
            </p>
            <div style={row}>
              <input
                className="input"
                style={{ flex: 1 }}
                type="number"
                placeholder="Montant à recharger (sats)"
                value={depositAmount}
                onChange={(e) => setDepositAmount(e.target.value)}
              />
              <button className="btn btn-primary" onClick={doDeposit} disabled={busy}>
                {busy ? <span className="spinner" /> : null} Recharger
              </button>
            </div>
            {depositInvoice && (
              <div style={{ display: "flex", justifyContent: "center", marginTop: 8 }}>
                <QRImage value={depositInvoice} />
              </div>
            )}
            {depositInvoice && (
              <div
                style={{
                  marginTop: 8,
                  display: "flex",
                  alignItems: "center",
                  gap: 8,
                  background: "rgba(255,255,255,0.03)",
                  borderRadius: 8,
                  padding: 8,
                }}
              >
                <code style={{ flex: 1, fontSize: 11, wordBreak: "break-all" }}>
                  {depositInvoice}
                </code>
                <button className="btn btn-ghost" onClick={copyDeposit}>
                  {depositCopied ? <Check size={16} /> : <Copy size={16} />}
                </button>
              </div>
            )}
          </div>

          {/* History */}
          <div className="glass-card" style={card}>
            <strong>
              <Clock size={16} style={{ verticalAlign: "-3px", marginRight: 6 }} />
              Activité
            </strong>
            {history.length === 0 ? (
              <p className="text-muted" style={{ fontSize: 12, marginTop: 8 }}>
                Aucune activité pour le moment.
              </p>
            ) : (
              <div style={{ display: "flex", flexDirection: "column", gap: 6, marginTop: 10 }}>
                {history.map((e) => {
                  const credit = CREDIT_KINDS.has(e.kind);
                  return (
                    <div
                      key={e.id}
                      style={{
                        display: "flex",
                        justifyContent: "space-between",
                        alignItems: "center",
                        padding: "8px 10px",
                        background: "rgba(255,255,255,0.03)",
                        borderRadius: 8,
                      }}
                    >
                      <div style={{ minWidth: 0 }}>
                        <div style={{ fontSize: 13, fontWeight: 600 }}>{kindLabel(e.kind)}</div>
                        <div className="text-muted" style={{ fontSize: 10, fontFamily: "monospace" }}>
                          {short(e.asset_id)}
                          {e.counterparty ? ` · ${short(e.counterparty, 8, 6)}` : ""}
                        </div>
                      </div>
                      <div
                        style={{
                          fontWeight: 700,
                          fontSize: 14,
                          whiteSpace: "nowrap",
                          color: credit ? "#4ade80" : "#f87171",
                        }}
                      >
                        {credit ? "+" : "−"}
                        {e.amount.toLocaleString()}
                      </div>
                    </div>
                  );
                })}
              </div>
            )}
          </div>

          {/* Mint */}
          <div className="glass-card" style={card}>
            <strong>
              <Coins size={16} style={{ verticalAlign: "-3px", marginRight: 6 }} />
              Émettre un asset
            </strong>
            <div style={{ marginTop: 10 }}>
              <div style={row}>
                <input
                  className="input"
                  style={{ flex: 2 }}
                  placeholder="Nom / ticker"
                  value={mintName}
                  onChange={(e) => setMintName(e.target.value)}
                />
                <input
                  className="input"
                  style={{ flex: 1 }}
                  type="number"
                  placeholder="Quantité"
                  value={mintAmount}
                  onChange={(e) => setMintAmount(e.target.value)}
                  disabled={mintCollectible}
                />
              </div>
              <input
                className="input"
                style={{ width: "100%", marginBottom: 8 }}
                placeholder="Métadonnée (optionnel)"
                value={mintMeta}
                onChange={(e) => setMintMeta(e.target.value)}
              />
              <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13, marginBottom: 6 }}>
                <input
                  type="checkbox"
                  checked={mintCollectible}
                  onChange={(e) => setMintCollectible(e.target.checked)}
                />
                Collectible (pièce unique)
              </label>
              <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13, marginBottom: 2 }}>
                <input
                  type="checkbox"
                  checked={mintGrouped}
                  onChange={(e) => setMintGrouped(e.target.checked)}
                />
                Groupé (réémetable plus tard)
              </label>
              <p className="text-muted" style={{ fontSize: 10, marginBottom: 10 }}>
                Émet une clé de groupe pour pouvoir ré-émettre cet asset ensuite. Sinon = offre figée.
              </p>
              <input
                className="input"
                style={{ width: "100%", marginBottom: 4 }}
                type="number"
                placeholder="Frais sat/vB (optionnel)"
                value={mintFee}
                onChange={(e) => setMintFee(e.target.value)}
                onBlur={async () => setMintQuote(await quoteFee("mint", mintFee))}
              />
              <p className="text-muted" style={{ fontSize: 10, marginBottom: 4 }}>
                Vide = estimation automatique du nœud.
              </p>
              {feeEstimate(mintQuote)}
              <button className="btn btn-primary" onClick={doMint} disabled={busy}>
                {busy ? <span className="spinner" /> : null} Émettre
              </button>
              {lastBatch && (
                <div style={{ marginTop: 10, fontSize: 12 }}>
                  <button className="btn btn-ghost" onClick={checkMint}>
                    Vérifier le statut
                  </button>
                  {mintStatus && (
                    <span className="text-muted" style={{ marginLeft: 8 }}>
                      {mintStatus}
                    </span>
                  )}
                </div>
              )}
            </div>
          </div>

          {/* Receive */}
          <div className="glass-card" style={card}>
            <strong>
              <Download size={16} style={{ verticalAlign: "-3px", marginRight: 6 }} />
              Recevoir
            </strong>
            <div style={{ marginTop: 10 }}>
              <div style={row}>
                <input
                  className="input"
                  style={{ flex: 2 }}
                  placeholder="Asset ID (hex)"
                  value={rcvAssetId}
                  onChange={(e) => setRcvAssetId(e.target.value)}
                />
                <input
                  className="input"
                  style={{ flex: 1 }}
                  type="number"
                  placeholder="Quantité"
                  value={rcvAmount}
                  onChange={(e) => setRcvAmount(e.target.value)}
                />
              </div>
              <button className="btn btn-primary" onClick={doReceive} disabled={busy}>
                {busy ? <span className="spinner" /> : null} Générer une adresse
              </button>
              {rcvAddr && (
                <div style={{ display: "flex", justifyContent: "center", marginTop: 10 }}>
                  <QRImage value={rcvAddr} />
                </div>
              )}
              {rcvAddr && (
                <div
                  style={{
                    marginTop: 10,
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    background: "rgba(255,255,255,0.03)",
                    borderRadius: 8,
                    padding: "8px 10px",
                  }}
                >
                  <code style={{ flex: 1, fontSize: 11, wordBreak: "break-all" }}>{rcvAddr}</code>
                  <button className="btn btn-ghost" onClick={copyAddr}>
                    {copied ? <Check size={16} /> : <Copy size={16} />}
                  </button>
                </div>
              )}
            </div>
          </div>

          {/* Send */}
          <div className="glass-card" style={card}>
            <strong>
              <Send size={16} style={{ verticalAlign: "-3px", marginRight: 6 }} />
              Envoyer (on-chain)
            </strong>
            <div style={{ marginTop: 10 }}>
              <div style={{ ...row, marginBottom: 8 }}>
                <input
                  className="input"
                  style={{ flex: 1 }}
                  placeholder="Adresse Taproot du destinataire"
                  value={sendAddr}
                  onChange={(e) => setSendAddr(e.target.value)}
                />
                <button
                  className="btn btn-ghost"
                  title="Scanner un QR"
                  onClick={async () => {
                    const s = await scanQrCode();
                    if (s) setSendAddr(s.trim());
                  }}
                >
                  <ScanLine size={16} />
                </button>
              </div>
              <div style={row}>
                <input
                  className="input"
                  style={{ flex: 1 }}
                  type="number"
                  placeholder="Frais sat/vB (optionnel)"
                  value={sendFee}
                  onChange={(e) => setSendFee(e.target.value)}
                  onBlur={async () => setSendQuote(await quoteFee("send", sendFee))}
                />
                <button className="btn btn-primary" onClick={doSend} disabled={busy}>
                  {busy ? <span className="spinner" /> : null} Envoyer
                </button>
              </div>
              {feeEstimate(sendQuote)}
              <p className="text-muted" style={{ fontSize: 10 }}>
                Vide = estimation automatique du nœud. Les transferts internes sont gratuits.
              </p>
            </div>
          </div>

          {/* Transfer */}
          <div className="glass-card" style={card}>
            <strong>
              <ArrowLeftRight size={16} style={{ verticalAlign: "-3px", marginRight: 6 }} />
              Transfert interne (instantané, gratuit)
            </strong>
            <div style={{ marginTop: 10 }}>
              <div style={row}>
                <input
                  className="input"
                  style={{ flex: 2 }}
                  placeholder="Asset ID (hex)"
                  value={xferAssetId}
                  onChange={(e) => setXferAssetId(e.target.value)}
                />
                <input
                  className="input"
                  style={{ flex: 1 }}
                  type="number"
                  placeholder="Quantité"
                  value={xferAmount}
                  onChange={(e) => setXferAmount(e.target.value)}
                />
              </div>
              <input
                className="input"
                style={{ width: "100%", marginBottom: 8 }}
                placeholder="Pubkey Nostr du destinataire (64 hex)"
                value={xferTo}
                onChange={(e) => setXferTo(e.target.value)}
              />
              <button className="btn btn-primary" onClick={doTransfer} disabled={busy}>
                {busy ? <span className="spinner" /> : null} Transférer
              </button>
            </div>
          </div>

          {/* Burn */}
          <div className="glass-card" style={card}>
            <strong>
              <Flame size={16} style={{ verticalAlign: "-3px", marginRight: 6 }} />
              Brûler
            </strong>
            <div style={{ marginTop: 10 }}>
              <div style={row}>
                <input
                  className="input"
                  style={{ flex: 2 }}
                  placeholder="Asset ID (hex)"
                  value={burnAssetId}
                  onChange={(e) => setBurnAssetId(e.target.value)}
                />
                <input
                  className="input"
                  style={{ flex: 1 }}
                  type="number"
                  placeholder="Quantité"
                  value={burnAmount}
                  onChange={(e) => setBurnAmount(e.target.value)}
                />
              </div>
              <button className="btn btn-ghost" onClick={doBurn} disabled={busy}>
                {busy ? <span className="spinner" /> : null} Brûler définitivement
              </button>
            </div>
          </div>

          {/* Lightning assets (read-only) */}
          <div className="glass-card" style={card}>
            <strong>⚡ Lightning Assets</strong>
            <p className="text-muted" style={{ fontSize: 11, marginTop: 4, marginBottom: 10 }}>
              {rfq
                ? `RFQ : ${rfq.buy_quotes} devis d'achat · ${rfq.sell_quotes} de vente`
                : "RFQ indisponible (aucun canal d'asset ?)."}
            </p>
            <div style={{ ...row, marginBottom: 8 }}>
              <input
                className="input"
                style={{ flex: 1 }}
                placeholder="Facture Lightning (lnbc…)"
                value={lnPayReq}
                onChange={(e) => setLnPayReq(e.target.value)}
              />
              <button
                className="btn btn-ghost"
                title="Scanner un QR"
                onClick={async () => {
                  const s = await scanQrCode();
                  if (s) setLnPayReq(s.trim());
                }}
              >
                <ScanLine size={16} />
              </button>
            </div>
            <div style={row}>
              <input
                className="input"
                style={{ flex: 2 }}
                placeholder="Asset ID (hex)"
                value={lnAssetId}
                onChange={(e) => setLnAssetId(e.target.value)}
              />
              <button className="btn btn-ghost" onClick={doLnDecode} disabled={busy}>
                {busy ? <span className="spinner" /> : null} Décoder
              </button>
            </div>
            <input
              className="input"
              style={{ width: "100%", marginBottom: 4 }}
              placeholder="ou group key (fongible, optionnel)"
              value={lnGroupKey}
              onChange={(e) => setLnGroupKey(e.target.value)}
            />
            <p className="text-muted" style={{ fontSize: 10, marginBottom: 4 }}>
              Group key = prévisualise une facture contre un groupe d'assets fongible. Le paiement
              utilise ensuite l'asset id concret.
            </p>
            {lnDecoded && (
              <div
                style={{
                  marginTop: 8,
                  paddingTop: 8,
                  borderTop: "1px solid rgba(255,255,255,0.06)",
                  fontSize: 12,
                  display: "flex",
                  flexDirection: "column",
                  gap: 3,
                }}
              >
                <div>
                  <span className="text-muted">Montant asset : </span>
                  {lnDecoded.asset_amount.toLocaleString()}
                </div>
                <div>
                  <span className="text-muted">≈ sats : </span>
                  {lnDecoded.sat_amount.toLocaleString()}
                </div>
                {lnDecoded.description && (
                  <div>
                    <span className="text-muted">Note : </span>
                    {lnDecoded.description}
                  </div>
                )}
                <button
                  className="btn btn-primary"
                  style={{ marginTop: 6 }}
                  onClick={doLnPay}
                  disabled={busy}
                >
                  {busy ? <span className="spinner" /> : null} Payer (débite ton solde)
                </button>
              </div>
            )}
            <p className="text-muted" style={{ fontSize: 10, marginTop: 8 }}>
              Décode puis paie une facture Lightning en asset (débite ton solde, remboursé si
              échec).
            </p>

            {/* Lightning receive: create an asset invoice */}
            <div
              style={{
                marginTop: 12,
                paddingTop: 12,
                borderTop: "1px solid rgba(255,255,255,0.06)",
              }}
            >
              <strong style={{ fontSize: 13 }}>Recevoir (LN)</strong>
              <div style={{ ...row, marginTop: 8 }}>
                <input
                  className="input"
                  style={{ flex: 2 }}
                  placeholder="Asset ID (hex)"
                  value={lnRcvAssetId}
                  onChange={(e) => setLnRcvAssetId(e.target.value)}
                />
                <input
                  className="input"
                  style={{ flex: 1 }}
                  placeholder="Montant"
                  inputMode="numeric"
                  value={lnRcvAmount}
                  onChange={(e) => setLnRcvAmount(e.target.value)}
                />
                <button className="btn btn-ghost" onClick={doLnReceive} disabled={busy}>
                  {busy ? <span className="spinner" /> : null} Créer
                </button>
              </div>
              {lnRcvInvoice && (
                <div style={{ display: "flex", justifyContent: "center", marginTop: 8 }}>
                  <QRImage value={lnRcvInvoice} />
                </div>
              )}
              {lnRcvInvoice && (
                <div
                  style={{
                    marginTop: 8,
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    background: "rgba(255,255,255,0.03)",
                    borderRadius: 8,
                    padding: 8,
                  }}
                >
                  <code style={{ flex: 1, fontSize: 11, wordBreak: "break-all" }}>
                    {lnRcvInvoice}
                  </code>
                  <button className="btn btn-ghost" onClick={copyLnInvoice}>
                    {lnRcvCopied ? "✓" : "Copier"}
                  </button>
                </div>
              )}
              {lnRcvQuote && (
                <p style={{ fontSize: 10, marginTop: 6, color: "#4ade80" }}>
                  Devis RFQ : {formatRfqRate(lnRcvQuote)} · expire{" "}
                  {new Date(lnRcvQuote.expiry * 1000).toLocaleTimeString()}
                </p>
              )}
              <p className="text-muted" style={{ fontSize: 10, marginTop: 8 }}>
                Crée une facture Lightning en asset ; ton solde est crédité à son règlement.
                Nécessite un canal d'asset ouvert côté nœud.
              </p>
            </div>
          </div>

          {/* Operator (admin) — asset channel management */}
          <div className="glass-card" style={card}>
            <button
              className="btn btn-ghost"
              style={{ fontSize: 13, padding: "2px 8px" }}
              onClick={() => setOpShown((v) => !v)}
            >
              ⚙️ Opérateur (canaux d'asset) {opShown ? "▲" : "▼"}
            </button>
            {opShown && (
              <div style={{ marginTop: 10 }}>
                <p className="text-muted" style={{ fontSize: 10, marginBottom: 10 }}>
                  Réservé à l'opérateur du nœud. Si personne n'est encore opérateur (et que le
                  nœud autorise le claim), appuie sur « Devenir opérateur » — un tap, aucun hex à
                  copier. Ouvrir un canal d'asset débloque le routage LN (payer/recevoir).
                </p>

                <button
                  className="btn btn-primary"
                  style={{ width: "100%", marginBottom: 10 }}
                  onClick={doClaimOperator}
                  disabled={busy}
                >
                  {busy ? <span className="spinner" /> : null} Devenir opérateur de ce nœud
                </button>
                <button className="btn btn-ghost" onClick={doListChannels} disabled={busy}>
                  {busy ? <span className="spinner" /> : null} Lister les canaux
                </button>
                {channels &&
                  (channels.length === 0 ? (
                    <p className="text-muted" style={{ fontSize: 11, marginTop: 8 }}>
                      Aucun canal.
                    </p>
                  ) : (
                    <div
                      style={{ marginTop: 8, display: "flex", flexDirection: "column", gap: 6 }}
                    >
                      {channels.map((c) => (
                        <div
                          key={c.chan_id}
                          style={{
                            background: "rgba(255,255,255,0.03)",
                            borderRadius: 8,
                            padding: 8,
                            fontSize: 11,
                          }}
                        >
                          <div>
                            {c.active ? "🟢" : "⚪"}{" "}
                            <code style={{ wordBreak: "break-all" }}>{c.peer.slice(0, 20)}…</code>
                          </div>
                          <div className="text-muted">
                            cap {c.capacity.toLocaleString()} · local{" "}
                            {c.local_balance.toLocaleString()} · remote{" "}
                            {c.remote_balance.toLocaleString()}
                          </div>
                        </div>
                      ))}
                    </div>
                  ))}

                <div
                  style={{
                    marginTop: 12,
                    paddingTop: 12,
                    borderTop: "1px solid rgba(255,255,255,0.06)",
                  }}
                >
                  <strong style={{ fontSize: 12 }}>Connecter un pair</strong>
                  <div style={{ ...row, marginTop: 8 }}>
                    <input
                      className="input"
                      style={{ flex: 2 }}
                      placeholder="Pubkey du pair (hex)"
                      value={peerPubkey}
                      onChange={(e) => setPeerPubkey(e.target.value)}
                    />
                    <input
                      className="input"
                      style={{ flex: 1 }}
                      placeholder="host:port"
                      value={peerHost}
                      onChange={(e) => setPeerHost(e.target.value)}
                    />
                  </div>
                  <button className="btn btn-ghost" onClick={doConnectPeer} disabled={busy}>
                    Connecter
                  </button>
                </div>

                <div
                  style={{
                    marginTop: 12,
                    paddingTop: 12,
                    borderTop: "1px solid rgba(255,255,255,0.06)",
                  }}
                >
                  <strong style={{ fontSize: 12 }}>Ouvrir un canal d'asset</strong>
                  <input
                    className="input"
                    style={{ width: "100%", marginTop: 8, marginBottom: 8 }}
                    placeholder="Asset ID (hex)"
                    value={chAssetId}
                    onChange={(e) => setChAssetId(e.target.value)}
                  />
                  <div style={row}>
                    <input
                      className="input"
                      style={{ flex: 1 }}
                      placeholder="Montant asset"
                      inputMode="numeric"
                      value={chAmount}
                      onChange={(e) => setChAmount(e.target.value)}
                    />
                    <input
                      className="input"
                      style={{ flex: 1 }}
                      placeholder="Frais sat/vB"
                      inputMode="numeric"
                      value={chFee}
                      onChange={(e) => setChFee(e.target.value)}
                    />
                  </div>
                  <input
                    className="input"
                    style={{ width: "100%", marginBottom: 8 }}
                    placeholder="Pubkey du pair (hex, déjà connecté)"
                    value={chPeer}
                    onChange={(e) => setChPeer(e.target.value)}
                  />
                  <button className="btn btn-primary" onClick={doOpenChannel} disabled={busy}>
                    {busy ? <span className="spinner" /> : null} Ouvrir le canal
                  </button>
                </div>
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}
