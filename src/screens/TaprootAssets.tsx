import { useEffect, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { motion } from "framer-motion";
import { useNotification } from "../contexts/NotificationContext";
import { useI18n } from "../i18n/I18nContext";
import { PasswordPrompt } from "../components/PasswordPrompt";
import { QRImage } from "../components/QRImage";
import { scanQrCode } from "../lib/scan";
import {
  ArrowLeft,
  RefreshCw,
  Plus,
  Send,
  QrCode,
  Copy,
  Check,
  Link,
  Download,
  ShieldCheck,
  Wallet,
  Zap,
  Activity,
  Settings,
  Flame,
  ScanLine,
  ArrowDownLeft,
  ArrowUpRight,
  X,
} from "lucide-react";

interface TaprootAssetsProps {
  onBack: () => void;
}

interface AssetSummary {
  asset_id: string;
  name: string;
  amount: number;
  asset_type: string;
  decimal_display: number;
}

interface ProofBackup {
  asset_id: string;
  name: string;
  amount: number;
  proof_base64: string;
}

interface Transfer {
  timestamp: number;
  anchor_txid: string;
  height_hint: number;
  inputs: number;
  outputs: number;
  total_out: number;
}

interface Batch {
  batch_key: string;
  batch_txid: string;
  state: string;
  assets: number;
}

interface Burn {
  asset_id: string;
  amount: number;
  anchor_txid: string;
}

interface AddrReceive {
  timestamp: number;
  addr: string;
  status: string;
  outpoint: string;
  utxo_amt_sat: number;
}

interface NodeInfo {
  version: string;
  lnd_version: string;
  network: string;
  lnd_pubkey: string;
}

interface DecodedAddr {
  encoded: string;
  asset_id: string;
  asset_type: string;
  amount: number;
}

interface AssetMeta {
  data: string;
  meta_type: string;
  meta_hash: string;
  decimal_display: number;
}

interface UniverseStats {
  runtime_id: number;
  num_assets: number;
  num_groups: number;
  num_syncs: number;
  num_proofs: number;
}

interface UniverseRoot {
  asset_id: string;
  asset_name: string;
}

interface DecodedAssetInvoice {
  asset_amount: number;
  sat_amount: number;
  description: string;
  destination: string;
}

interface RfqQuotes {
  buy_quotes: number;
  sell_quotes: number;
}

interface TaskState {
  state: "idle" | "pending" | "ready" | "failed";
  error?: string;
}

interface BackgroundInit {
  ark: TaskState;
  tapd: TaskState;
}

type Tab = "portfolio" | "receive" | "send" | "ln" | "activity" | "advanced";

function fmtAmount(amount: number, decimals: number): string {
  if (!decimals) return amount.toLocaleString();
  return (amount / Math.pow(10, decimals)).toLocaleString(undefined, {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  });
}

const inner: CSSProperties = {
  background: "rgba(0,0,0,0.3)",
  border: "1px solid var(--border)",
  borderRadius: 12,
  padding: 12,
};

export function TaprootAssets({ onBack }: TaprootAssetsProps) {
  const { t } = useI18n();
  const { notify } = useNotification();
  const [host, setHost] = useState("https://localhost:10029");
  const [cert, setCert] = useState("");
  const [macaroon, setMacaroon] = useState("");
  const [useTor, setUseTor] = useState(false);
  const [forceTor, setForceTor] = useState(false);
  const [torStatus, setTorStatus] = useState("Stopped");
  const [connected, setConnected] = useState(false);
  const [autoConnecting, setAutoConnecting] = useState(true);
  const [assets, setAssets] = useState<AssetSummary[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [hasDefaults, setHasDefaults] = useState(false);
  const [connectStatus, setConnectStatus] = useState<string>("");

  const [mintName, setMintName] = useState("");
  const [mintAmount, setMintAmount] = useState("");
  const [mintMeta, setMintMeta] = useState("");

  const [addrAssetId, setAddrAssetId] = useState("");
  const [addrAmount, setAddrAmount] = useState("");
  const [address, setAddress] = useState("");

  const [sendAddress, setSendAddress] = useState("");
  const [sendTxid, setSendTxid] = useState("");

  const [copied, setCopied] = useState(false);

  const [proofs, setProofs] = useState<ProofBackup[]>([]);
  const [proofText, setProofText] = useState("");
  const [verifyResult, setVerifyResult] = useState<boolean | null>(null);
  const [proofLoading, setProofLoading] = useState(false);
  const [passwordPromptOpen, setPasswordPromptOpen] = useState(false);

  // Advanced mint + fees + send preview
  const [mintCollectible, setMintCollectible] = useState(false);
  const [mintNewGroup, setMintNewGroup] = useState(false);
  const [mintFee, setMintFee] = useState("");
  const [sendFee, setSendFee] = useState("");
  const [decoded, setDecoded] = useState<DecodedAddr | null>(null);

  // Extra tapd data
  const [transfers, setTransfers] = useState<Transfer[]>([]);
  const [batches, setBatches] = useState<Batch[]>([]);
  const [burns, setBurns] = useState<Burn[]>([]);
  const [receives, setReceives] = useState<AddrReceive[]>([]);
  const [nodeInfo, setNodeInfo] = useState<NodeInfo | null>(null);
  const [metas, setMetas] = useState<Record<string, AssetMeta>>({});
  const [universeStats, setUniverseStats] = useState<UniverseStats | null>(null);
  const [universeRoots, setUniverseRoots] = useState<UniverseRoot[]>([]);
  const [syncHost, setSyncHost] = useState("");

  // Lightning assets (litd)
  const [rfqQuotes, setRfqQuotes] = useState<RfqQuotes | null>(null);
  const [lnPayReq, setLnPayReq] = useState("");
  const [lnAssetId, setLnAssetId] = useState("");
  const [lnPeer, setLnPeer] = useState("");
  const [lnDecoded, setLnDecoded] = useState<DecodedAssetInvoice | null>(null);
  const [lnRecvAmount, setLnRecvAmount] = useState("");
  const [lnMemo, setLnMemo] = useState("");
  const [lnBolt11, setLnBolt11] = useState("");
  const [chanAmount, setChanAmount] = useState("");
  const [chanFee, setChanFee] = useState("");

  // UI: tabbed navigation, mint sheet, burn confirmation modal
  const [tab, setTab] = useState<Tab>("portfolio");
  const [mintOpen, setMintOpen] = useState(false);
  const [burnTarget, setBurnTarget] = useState<AssetSummary | null>(null);
  const [burnModalAmt, setBurnModalAmt] = useState("");

  const decimalById: Record<string, number> = {};
  assets.forEach((a) => {
    decimalById[a.asset_id] = a.decimal_display;
  });
  const nameById: Record<string, string> = {};
  assets.forEach((a) => {
    nameById[a.asset_id] = a.name;
  });

  useEffect(() => {
    let cancelled = false;
    (async () => {
      await loadTorConfig();
      // Check if defaults are configured
      try {
        const defaults = await invoke<{
          host: string;
          cert_pem: string;
          macaroon_hex: string;
        }>("get_tapd_defaults");
        setHasDefaults(
          defaults.host.trim().length > 0 &&
            defaults.macaroon_hex.trim().length > 0
        );
      } catch {
        setHasDefaults(false);
      }
      // Poll the background init status for rich feedback instead of a blind
      // boolean. The backend auto-connects tapd on unlock (saved node or default).
      for (let i = 0; i < 40 && !cancelled; i++) {
        try {
          const status = await invoke<BackgroundInit>("get_background_init_status");
          const tapd = status.tapd;

          if (tapd.state === "ready") {
            if (cancelled) return;
            setConnected(true);
            setAutoConnecting(false);
            refreshTorStatus();
            await fetchAssets();
            return;
          }

          if (tapd.state === "failed") {
            if (cancelled) return;
            setAutoConnecting(false);
            refreshTorStatus();
            setError(
              tapd.error ||
                "La connexion au nœud tapd a échoué. Veuillez vérifier l'adresse, le certificat et le macaroon, puis réessayer."
            );
            return;
          }

          if (tapd.state === "idle") {
            // No saved config and no defaults — user must connect manually
            if (cancelled) return;
            setAutoConnecting(false);
            refreshTorStatus();
            return;
          }

          // Pending — show live status
          if (tapd.state === "pending") {
            setConnectStatus(
              i < 5
                ? "Démarrage de la connexion au nœud tapd..."
                : `Connexion via Tor en cours... (${i * 2}s — peut prendre 30-60s au premier démarrage)`
            );
          }
        } catch {
          // keep polling
        }
        await new Promise((r) => setTimeout(r, 2000));
      }
      // Timeout after ~80s
      if (!cancelled) {
        setAutoConnecting(false);
        refreshTorStatus();
        setError(
          "Délai de connexion dépassé (80s). Veuillez entrer les informations de votre nœud tapd manuellement, ou utiliser le bouton \"Nœud par défaut\" si configuré."
        );
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Live updates: tapd streams receive/send/mint events; refresh quietly on each.
  useEffect(() => {
    const un = listen("tapd-event", () => {
      invoke<AssetSummary[]>("list_taproot_assets").then(setAssets).catch(() => {});
      fetchExtras();
    });
    return () => {
      un.then((f) => f());
    };
  }, []);

  // Auto-select the only asset for Lightning flows so the user never pastes a hex id.
  useEffect(() => {
    if (connected && !lnAssetId && assets.length === 1) {
      setLnAssetId(assets[0].asset_id);
    }
  }, [connected, assets, lnAssetId]);

  // Auto-decode a Lightning invoice as soon as it's scanned/pasted (asset known).
  useEffect(() => {
    const inv = lnPayReq.trim().toLowerCase();
    if (connected && lnAssetId && inv.startsWith("lnbc") && inv.length > 60) {
      decodeLnInvoice();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [lnPayReq, lnAssetId]);

  async function loadTorConfig() {
    try {
      const cfg = await invoke<{ enabled: boolean; force_tor: boolean }>("load_tor_config");
      setUseTor(cfg.enabled);
      setForceTor(cfg.force_tor);
    } catch (e) {
      console.error("load tor config error:", e);
    }
    refreshTorStatus();
  }

  async function refreshTorStatus() {
    try {
      const status = await invoke<string>("get_tor_status");
      setTorStatus(status);
    } catch (e) {
      setTorStatus("Unknown");
    }
  }

  async function applyTorDefaults() {
    try {
      await invoke("save_tor_config", { config: { enabled: useTor, force_tor: forceTor } });
    } catch (e) {
      console.error("save tor config error:", e);
    }
  }

  // Connect to the embedded default node directly, no password required.
  async function connectDefaultNode() {
    setError("");
    setAutoConnecting(true);
    // Arti's first onion connection is often flaky ("Protocol error while
    // launching a data stream"); the descriptor gets cached, so retry a few times.
    const maxAttempts = 3;
    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      setConnectStatus(
        attempt === 1
          ? "Connexion au nœud par défaut via Tor…"
          : `Nouvelle tentative ${attempt}/${maxAttempts} via Tor… (1ʳᵉ connexion onion souvent lente)`
      );
      try {
        await invoke("connect_default_tapd");
        setConnected(true);
        setAutoConnecting(false);
        notify(t("taproot.connected"), "success");
        await fetchAssets();
        refreshTorStatus();
        return;
      } catch (e) {
        const msg = String(e);
        if (msg.includes("no default tapd node is configured")) {
          setError(
            "Aucun nœud par défaut n'est configuré. Le wallet doit être compilé avec les variables d'environnement OZARK_DEFAULT_TAPD_HOST, OZARK_DEFAULT_TAPD_CERT et OZARK_DEFAULT_TAPD_MACAROON, ou avec un fichier tapd-defaults.json. Veuillez vous connecter manuellement avec l'adresse de votre nœud."
          );
          break;
        }
        if (attempt < maxAttempts) {
          await new Promise((r) => setTimeout(r, 3000));
          continue;
        }
        setError(msg);
      }
    }
    setAutoConnecting(false);
    refreshTorStatus();
  }

  // Disconnect so the user can connect to a different (e.g. their own) tapd node.
  async function changeNode() {
    try {
      await invoke("disconnect_tapd");
    } catch (e) {
      console.error("disconnect tapd error:", e);
    }
    setAssets([]);
    setError("");
    setConnected(false);
    setAutoConnecting(false);
    setTab("portfolio");
  }

  function connect() {
    setError("");
    setPasswordPromptOpen(true);
  }

  async function doConnect(password: string) {
    setLoading(true);
    setError("");
    try {
      await applyTorDefaults();
      await invoke("connect_tapd", {
        password,
        host,
        certPem: cert,
        macaroonHex: macaroon,
        useTor,
      });
      setConnected(true);
      notify(t("taproot.connected"), "success");
      setMacaroon("");
      await fetchAssets();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
      refreshTorStatus();
    }
  }

  async function fetchAssets() {
    setLoading(true);
    setError("");
    try {
      const list = await invoke<AssetSummary[]>("list_taproot_assets");
      setAssets(list);
      fetchExtras();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function mint() {
    if (!mintName || (!mintCollectible && !mintAmount)) return;
    setLoading(true);
    setError("");
    try {
      const batch = await invoke<string>("mint_taproot_asset", {
        name: mintName,
        amount: Number(mintAmount),
        metadata: mintMeta,
        collectible: mintCollectible,
        newGroup: mintNewGroup,
        feeRateSatVb: Number(mintFee) || 0,
      });
      notify(`Batch finalisé: ${batch}`, "success");
      setMintName("");
      setMintAmount("");
      setMintMeta("");
      setMintOpen(false);
      await fetchAssets();
      await fetchBatches();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function newAddress() {
    if (!addrAssetId || !addrAmount) return;
    setLoading(true);
    setError("");
    try {
      const addr = await invoke<string>("new_taproot_address", {
        assetId: addrAssetId,
        amount: Number(addrAmount),
      });
      setAddress(addr);
      notify(t("notifications.addressCopied"), "success");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function send() {
    if (!sendAddress) return;
    setLoading(true);
    setError("");
    try {
      const txid = await invoke<string>("send_taproot_asset", {
        address: sendAddress,
        feeRateSatVb: Number(sendFee) || 0,
      });
      setSendTxid(txid);
      notify(t("notifications.paymentSent"), "success");
      setSendAddress("");
      setDecoded(null);
      await fetchAssets();
      await fetchTransfers();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  function copyText(text: string) {
    if (!text) return;
    navigator.clipboard.writeText(text);
    notify(t("notifications.addressCopied"), "success");
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }

  async function fetchExtras() {
    try { setTransfers(await invoke<Transfer[]>("list_taproot_transfers")); } catch {}
    try { setBatches(await invoke<Batch[]>("list_taproot_batches")); } catch {}
    try { setBurns(await invoke<Burn[]>("list_taproot_burns")); } catch {}
    try { setReceives(await invoke<AddrReceive[]>("taproot_addr_receives")); } catch {}
    try { setNodeInfo(await invoke<NodeInfo>("get_taproot_info")); } catch {}
    try { setUniverseStats(await invoke<UniverseStats>("get_universe_stats")); } catch {}
    try { setUniverseRoots(await invoke<UniverseRoot[]>("list_universe_roots")); } catch {}
    try { setRfqQuotes(await invoke<RfqQuotes>("list_rfq_quotes")); } catch {}
  }

  async function decodeLnInvoice() {
    if (!lnPayReq || !lnAssetId) return;
    setError("");
    setLnDecoded(null);
    try {
      setLnDecoded(
        await invoke<DecodedAssetInvoice>("decode_asset_invoice", {
          payReq: lnPayReq,
          assetId: lnAssetId,
        })
      );
    } catch (e) {
      setError(String(e));
    }
  }

  async function payLnInvoice() {
    if (!lnPayReq || !lnAssetId) return;
    setLoading(true);
    setError("");
    try {
      const status = await invoke<string>("pay_asset_invoice", {
        payReq: lnPayReq,
        assetId: lnAssetId,
        peerPubkey: lnPeer,
      });
      notify(`Paiement: ${status}`, status === "Succeeded" ? "success" : "error");
      setLnPayReq("");
      setLnDecoded(null);
      await fetchAssets();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function createLnInvoice() {
    if (!lnAssetId || !lnRecvAmount) return;
    setLoading(true);
    setError("");
    setLnBolt11("");
    try {
      const b = await invoke<string>("create_asset_invoice", {
        assetId: lnAssetId,
        assetAmount: Number(lnRecvAmount),
        peerPubkey: lnPeer,
        memo: lnMemo,
      });
      setLnBolt11(b);
      notify("Invoice asset créée", "success");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function fundChannel() {
    if (!lnAssetId || !chanAmount || !lnPeer) return;
    setLoading(true);
    setError("");
    try {
      const out = await invoke<string>("fund_asset_channel", {
        assetId: lnAssetId,
        assetAmount: Number(chanAmount),
        peerPubkey: lnPeer,
        feeRateSatVb: Number(chanFee) || 1,
      });
      notify(`Canal en ouverture: ${out}`, "success");
      await fetchAssets();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function doSyncUniverse() {
    if (!syncHost) return;
    setLoading(true);
    setError("");
    try {
      const n = await invoke<number>("sync_universe", { host: syncHost });
      notify(`Universe sync : ${n} univers`, "success");
      await fetchExtras();
      await fetchAssets();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function fetchBatches() {
    try {
      setBatches(await invoke<Batch[]>("list_taproot_batches"));
    } catch (e) {
      setError(String(e));
    }
  }

  async function fetchTransfers() {
    try {
      setTransfers(await invoke<Transfer[]>("list_taproot_transfers"));
    } catch (e) {
      setError(String(e));
    }
  }

  async function cancelBatch() {
    setLoading(true);
    setError("");
    try {
      await invoke("cancel_taproot_batch");
      notify("Batch en attente annulé", "success");
      await fetchBatches();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function decodeAddrForSend() {
    if (!sendAddress) return;
    setError("");
    setDecoded(null);
    try {
      setDecoded(await invoke<DecodedAddr>("decode_taproot_addr", { address: sendAddress }));
    } catch (e) {
      setError(String(e));
    }
  }

  async function showMeta(assetId: string) {
    try {
      const m = await invoke<AssetMeta>("fetch_taproot_asset_meta", { assetId });
      setMetas((prev) => ({ ...prev, [assetId]: m }));
    } catch (e) {
      setError(String(e));
    }
  }

  async function burnAsset(assetId: string, amount: number) {
    setLoading(true);
    setError("");
    try {
      const txid = await invoke<string>("burn_taproot_asset", { assetId, amount });
      notify(`Burn diffusé: ${txid.slice(0, 16)}…`, "success");
      await fetchAssets();
      await fetchExtras();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function exportProofs() {
    setProofLoading(true);
    setError("");
    try {
      const list = await invoke<ProofBackup[]>("export_taproot_proofs");
      setProofs(list);
      notify(t("backup.saved"), "success");
    } catch (e) {
      setError(String(e));
    } finally {
      setProofLoading(false);
    }
  }

  function downloadProofs() {
    if (!proofs.length) return;
    const payload = {
      created_at: new Date().toISOString(),
      proofs,
    };
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `taproot-proofs-${Date.now()}.json`;
    a.click();
    URL.revokeObjectURL(url);
    notify(t("backup.download"), "success");
  }

  async function verifyProof() {
    if (!proofText) return;
    setProofLoading(true);
    setError("");
    setVerifyResult(null);
    try {
      const ok = await invoke<boolean>("verify_taproot_proof", {
        proofBase64: proofText.trim(),
      });
      setVerifyResult(ok);
      notify(ok ? t("taproot.proofValid") : t("taproot.proofInvalid"), ok ? "success" : "error");
    } catch (e) {
      setError(String(e));
    } finally {
      setProofLoading(false);
    }
  }

  // --- UI helpers -----------------------------------------------------------

  async function scanInto(setter: (v: string) => void) {
    const content = await scanQrCode();
    if (content) setter(content.trim());
  }

  function receiveAsset(a: AssetSummary) {
    setAddrAssetId(a.asset_id);
    setAddress("");
    setTab("receive");
  }

  function openBurn(a: AssetSummary) {
    setBurnTarget(a);
    setBurnModalAmt(String(a.amount));
  }

  async function confirmBurn() {
    if (!burnTarget) return;
    const amt = Number(burnModalAmt);
    setBurnTarget(null);
    if (amt > 0) await burnAsset(burnTarget.asset_id, amt);
  }

  const assetBadge = (name: string) => (name || "?").trim().charAt(0).toUpperCase();

  const TABS: { id: Tab; label: string; Icon: typeof Wallet }[] = [
    { id: "portfolio", label: t("taproot.tabPortfolio"), Icon: Wallet },
    { id: "receive", label: t("taproot.tabReceive"), Icon: ArrowDownLeft },
    { id: "send", label: t("taproot.tabSend"), Icon: ArrowUpRight },
    { id: "ln", label: t("taproot.tabLightning"), Icon: Zap },
    { id: "activity", label: t("taproot.tabActivity"), Icon: Activity },
  ];

  const errorBox = error ? (
    <div
      style={{
        marginBottom: 16,
        padding: 12,
        borderRadius: 10,
        background: "rgba(239,68,68,0.1)",
        border: "1px solid var(--danger)",
        color: "var(--danger)",
        fontSize: 13,
        wordBreak: "break-word",
        whiteSpace: "pre-wrap",
      }}
    >
      {error}
    </div>
  ) : null;

  // --- Render: not connected -----------------------------------------------

  function renderConnect() {
    if (autoConnecting) {
      return (
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          className="glass-card"
          style={{ padding: 32, marginBottom: 20, textAlign: "center" }}
        >
          <span className="spinner" />
          <div className="text-secondary" style={{ marginTop: 14 }}>
            {connectStatus || "Connexion au nœud tapd..."}
          </div>
          <div className="text-muted" style={{ fontSize: 12, marginTop: 6 }}>
            Premier démarrage de Tor : 30–60 s possibles.
          </div>
          <div className="text-muted" style={{ fontSize: 12, marginTop: 4 }}>
            {t("taproot.torStatus")}: {torStatus}
          </div>
        </motion.div>
      );
    }
    return (
      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        className="glass-card"
        style={{ padding: 24, marginBottom: 20 }}
      >
        <div className="text-secondary" style={{ marginBottom: 16 }}>
          <Link size={16} style={{ marginRight: 8, verticalAlign: "middle" }} />
          {t("taproot.connect")}
        </div>
        <input className="input" placeholder={t("taproot.host")} value={host} onChange={(e) => setHost(e.target.value)} style={{ marginBottom: 12 }} />
        <textarea className="input" placeholder={t("taproot.cert")} value={cert} onChange={(e) => setCert(e.target.value)} rows={4} style={{ marginBottom: 12 }} />
        <input className="input" placeholder={t("taproot.macaroon")} value={macaroon} onChange={(e) => setMacaroon(e.target.value)} style={{ marginBottom: 12 }} />
        <div style={{ display: "flex", alignItems: "center", gap: 12, marginBottom: 8 }}>
          <label style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer" }}>
            <input type="checkbox" checked={useTor} onChange={(e) => setUseTor(e.target.checked)} />
            {t("taproot.useTor")}
          </label>
          <span className="text-muted" style={{ fontSize: 12 }}>{t("taproot.torStatus")}: {torStatus}</span>
        </div>
        <label style={{ display: "flex", alignItems: "center", gap: 6, marginBottom: 16, cursor: "pointer" }}>
          <input type="checkbox" checked={forceTor} onChange={(e) => setForceTor(e.target.checked)} />
          {t("taproot.forceTor")}
        </label>
        {errorBox}
        <button className="btn btn-primary" onClick={connectDefaultNode} disabled={loading} style={{ marginBottom: 10, width: "100%" }}>
          {loading ? <span className="spinner" /> : <RefreshCw size={18} />} {t("taproot.retryDefault")}
        </button>
        {!hasDefaults && (
          <div className="text-muted" style={{ fontSize: 11, marginBottom: 10 }}>{t("taproot.noDefaultDetected")}</div>
        )}
        <button className="btn btn-secondary" onClick={connect} disabled={loading} style={{ width: "100%" }}>
          <Link size={18} /> {t("taproot.connectCustom")}
        </button>
      </motion.div>
    );
  }

  // --- Render: tabs --------------------------------------------------------

  function renderHero() {
    return (
      <div
        className="glass-card"
        style={{
          padding: 20,
          marginBottom: 16,
          background: "linear-gradient(135deg, rgba(0,240,255,0.12), rgba(168,85,247,0.06))",
          borderColor: "rgba(0,240,255,0.2)",
        }}
      >
        <div className="text-muted" style={{ fontSize: 12 }}>{t("taproot.portfolioTitle")}</div>
        <div className="title-lg" style={{ marginTop: 4 }}>
          {assets.length} <span className="text-muted" style={{ fontSize: 15 }}>asset{assets.length > 1 ? "s" : ""}</span>
        </div>
        <div style={{ display: "flex", gap: 16, marginTop: 8, fontSize: 12 }} className="text-muted">
          {nodeInfo && <span>{t("taproot.network")} : <b style={{ color: "var(--accent-cyan)" }}>{nodeInfo.network}</b></span>}
          {universeStats && <span>Universe : <b style={{ color: "var(--accent-cyan)" }}>{universeStats.num_assets}</b></span>}
        </div>
      </div>
    );
  }

  function renderPortfolio() {
    return (
      <>
        {renderHero()}
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", margin: "2px 4px 12px" }}>
          <span className="text-secondary" style={{ fontWeight: 600 }}>{t("taproot.assets")} ({assets.length})</span>
          <button className="btn btn-ghost" onClick={fetchAssets} disabled={loading}><RefreshCw size={16} /> {t("refresh")}</button>
        </div>
        {assets.length === 0 ? (
          <div className="glass-card" style={{ padding: 20 }}><div className="text-muted">{t("taproot.noAssets")}</div></div>
        ) : (
          assets.map((a) => (
            <div key={a.asset_id} className="glass-card" style={{ padding: 14, marginBottom: 12 }}>
              <div style={{ display: "flex", alignItems: "center", gap: 11 }}>
                <div style={{ width: 40, height: 40, borderRadius: 12, flex: "none", display: "grid", placeItems: "center", fontWeight: 800, color: "#000", background: "linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))" }}>{assetBadge(a.name)}</div>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ fontWeight: 700 }}>{a.name || t("taproot.noName")}</div>
                  <div className="text-muted" style={{ fontSize: 11 }}>{a.asset_type}{a.decimal_display ? ` · ${a.decimal_display} déc.` : ""}</div>
                </div>
                <div style={{ fontWeight: 700, fontVariantNumeric: "tabular-nums" }}>{fmtAmount(a.amount, a.decimal_display)}</div>
              </div>
              <div className="text-muted" style={{ fontSize: 10.5, fontFamily: "var(--font-mono)", wordBreak: "break-all", marginTop: 8 }}>{a.asset_id}</div>
              {metas[a.asset_id] && (
                <div className="text-muted" style={{ fontSize: 11, marginTop: 6, wordBreak: "break-all" }}>{metas[a.asset_id].meta_type}: {metas[a.asset_id].data || "(vide)"}</div>
              )}
              <div style={{ display: "flex", gap: 8, marginTop: 11 }}>
                <button className="btn btn-secondary" style={{ flex: 1, padding: "8px" }} onClick={() => receiveAsset(a)}><ArrowDownLeft size={15} /> {t("taproot.tabReceive")}</button>
                <button className="btn btn-secondary" style={{ flex: 1, padding: "8px" }} onClick={() => setTab("send")}><ArrowUpRight size={15} /> {t("taproot.tabSend")}</button>
                <button className="btn btn-ghost" style={{ padding: "8px 10px" }} onClick={() => showMeta(a.asset_id)} title={t("taproot.meta")}><QrCode size={15} /></button>
                <button className="btn btn-ghost" style={{ padding: "8px 10px", color: "var(--danger)" }} onClick={() => openBurn(a)} title={t("taproot.burn")}><Flame size={15} /></button>
              </div>
            </div>
          ))
        )}
      </>
    );
  }

  const assetSelect = (value: string, onChange: (v: string) => void) => (
    <select className="input" value={value} onChange={(e) => onChange(e.target.value)} style={{ marginBottom: 11 }}>
      <option value="">{t("taproot.chooseAsset")}</option>
      {assets.map((a) => (
        <option key={a.asset_id} value={a.asset_id}>{a.name || a.asset_id.slice(0, 10)} — {fmtAmount(a.amount, a.decimal_display)}</option>
      ))}
    </select>
  );

  function renderReceive() {
    return (
      <>
        <div className="title-lg" style={{ margin: "6px 0 14px" }}>{t("taproot.tabReceive")}</div>
        <div className="glass-card" style={{ padding: 20 }}>
          <label className="text-muted" style={{ fontSize: 11, display: "block", marginBottom: 6 }}>{t("taproot.assetToReceive")}</label>
          {assetSelect(addrAssetId, (v) => { setAddrAssetId(v); setAddress(""); })}
          <label className="text-muted" style={{ fontSize: 11, display: "block", marginBottom: 6 }}>{t("taproot.amount")}</label>
          <input className="input" type="number" value={addrAmount} onChange={(e) => setAddrAmount(e.target.value)} style={{ marginBottom: 12 }} />
          <button className="btn btn-primary" onClick={newAddress} disabled={loading || !addrAssetId || !addrAmount} style={{ width: "100%" }}>
            {loading ? <span className="spinner" /> : <QrCode size={16} />} {t("taproot.generateAddress")}
          </button>
          {address && (
            <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 12, marginTop: 16 }}>
              <QRImage value={address} />
              <div style={{ display: "flex", gap: 8, alignItems: "center", ...inner, width: "100%" }}>
                <span style={{ fontFamily: "var(--font-mono)", fontSize: 11, flex: 1, wordBreak: "break-all" }}>{address}</span>
                <button className="btn btn-ghost" onClick={() => copyText(address)}>{copied ? <Check size={16} /> : <Copy size={16} />}</button>
              </div>
              <div className="text-muted" style={{ fontSize: 11 }}>{t("taproot.addressOneTime")}</div>
            </div>
          )}
        </div>
      </>
    );
  }

  function renderSend() {
    return (
      <>
        <div className="title-lg" style={{ margin: "6px 0 14px" }}>{t("taproot.tabSend")}</div>
        <div className="glass-card" style={{ padding: 20 }}>
          <label className="text-muted" style={{ fontSize: 11, display: "block", marginBottom: 6 }}>{t("taproot.taprootAddress")}</label>
          <div style={{ display: "flex", gap: 10, marginBottom: 12 }}>
            <input className="input" style={{ flex: 1 }} placeholder="taprt1…" value={sendAddress} onChange={(e) => { setSendAddress(e.target.value); setDecoded(null); }} />
            <button className="btn btn-secondary" style={{ flex: "none", padding: "0 14px" }} onClick={() => scanInto((v) => { setSendAddress(v); setDecoded(null); })}><ScanLine size={18} /></button>
          </div>
          {decoded && (
            <div style={{ ...inner, marginBottom: 12 }}>
              <div style={{ fontWeight: 600 }}>{nameById[decoded.asset_id] || "Asset"} · {decoded.amount.toLocaleString()}</div>
              <div className="text-muted" style={{ fontSize: 11, fontFamily: "var(--font-mono)", wordBreak: "break-all" }}>{decoded.asset_id}</div>
            </div>
          )}
          <label className="text-muted" style={{ fontSize: 11, display: "block", marginBottom: 6 }}>{t("taproot.feeRate")}</label>
          <input className="input" type="number" placeholder="0" value={sendFee} onChange={(e) => setSendFee(e.target.value)} style={{ marginBottom: 12 }} />
          <div style={{ display: "flex", gap: 10 }}>
            <button className="btn btn-secondary" style={{ flex: 1 }} onClick={decodeAddrForSend} disabled={loading || !sendAddress}>{t("taproot.decode")}</button>
            <button className="btn btn-primary" style={{ flex: 1 }} onClick={send} disabled={loading || !sendAddress}>{loading ? <span className="spinner" /> : <Send size={16} />} {t("taproot.send")}</button>
          </div>
          {sendTxid && <div className="text-muted" style={{ marginTop: 12, fontSize: 12, wordBreak: "break-all" }}>TX: {sendTxid}</div>}
        </div>
      </>
    );
  }

  function renderLightning() {
    return (
      <>
        <div className="title-lg" style={{ margin: "6px 0 6px" }}><Zap size={20} style={{ verticalAlign: "middle" }} /> {t("taproot.tabLightning")}</div>
        {assets.length > 1 && (
          <div className="glass-card" style={{ padding: 14, marginBottom: 14 }}>
            <label className="text-muted" style={{ fontSize: 11, display: "block", marginBottom: 6 }}>{t("taproot.lnAsset")}</label>
            {assetSelect(lnAssetId, setLnAssetId)}
          </div>
        )}

        {/* PAY (primary) */}
        <div className="glass-card" style={{ padding: 20, marginBottom: 14, borderColor: "rgba(0,240,255,0.22)" }}>
          <div className="text-secondary" style={{ fontWeight: 600, marginBottom: 12 }}><ArrowUpRight size={16} style={{ verticalAlign: "middle" }} /> {t("taproot.payInvoice")}</div>
          <button className="btn btn-primary" style={{ width: "100%", marginBottom: 12 }} onClick={() => scanInto((v) => { setLnPayReq(v); setLnDecoded(null); })}><ScanLine size={18} /> {t("taproot.scanQr")}</button>
          <div className="text-muted" style={{ textAlign: "center", fontSize: 11, marginBottom: 10 }}>{t("taproot.orPasteInvoice")}</div>
          <input className="input" placeholder="lnbc…" value={lnPayReq} onChange={(e) => { setLnPayReq(e.target.value); setLnDecoded(null); }} style={{ marginBottom: 11 }} />
          {lnDecoded && (
            <div style={{ ...inner, marginBottom: 11, borderColor: "rgba(0,240,255,0.22)" }}>
              <div style={{ fontWeight: 700 }}>{lnDecoded.asset_amount.toLocaleString()} {nameById[lnAssetId] || "asset"} <span className="text-muted" style={{ fontWeight: 400 }}>≈ {lnDecoded.sat_amount.toLocaleString()} sat</span></div>
              <div className="text-muted" style={{ fontSize: 12 }}>{lnDecoded.description || "(sans description)"}</div>
              <div className="text-muted" style={{ fontSize: 11, marginTop: 6 }}>{t("taproot.paidViaChannel")}</div>
            </div>
          )}
          <div style={{ display: "flex", gap: 10 }}>
            <button className="btn btn-secondary" style={{ flex: 1 }} onClick={decodeLnInvoice} disabled={loading || !lnPayReq || !lnAssetId}>{t("taproot.decode")}</button>
            <button className="btn btn-primary" style={{ flex: 1 }} onClick={payLnInvoice} disabled={loading || !lnPayReq || !lnAssetId}>{loading ? <span className="spinner" /> : <Zap size={16} />} {t("taproot.pay")}</button>
          </div>
          <details style={{ marginTop: 12 }}>
            <summary className="text-muted" style={{ fontSize: 11, cursor: "pointer" }}>{t("taproot.peerAdvanced")}</summary>
            <input className="input" placeholder={t("taproot.peerPlaceholder")} value={lnPeer} onChange={(e) => setLnPeer(e.target.value)} style={{ marginTop: 8 }} />
          </details>
        </div>

        {/* RECEIVE */}
        <div className="glass-card" style={{ padding: 20, marginBottom: 14 }}>
          <div className="text-secondary" style={{ fontWeight: 600, marginBottom: 12 }}><ArrowDownLeft size={16} style={{ verticalAlign: "middle" }} /> {t("taproot.receiveAssets")}</div>
          <input className="input" type="number" placeholder={t("taproot.amountAssets")} value={lnRecvAmount} onChange={(e) => setLnRecvAmount(e.target.value)} style={{ marginBottom: 10 }} />
          <input className="input" placeholder={t("taproot.memoOptional")} value={lnMemo} onChange={(e) => setLnMemo(e.target.value)} style={{ marginBottom: 10 }} />
          <button className="btn btn-secondary" style={{ width: "100%" }} onClick={createLnInvoice} disabled={loading || !lnAssetId || !lnRecvAmount}>{t("taproot.createInvoice")}</button>
          {lnBolt11 && (
            <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 12, marginTop: 14 }}>
              <QRImage value={lnBolt11} />
              <div style={{ display: "flex", gap: 8, alignItems: "center", ...inner, width: "100%" }}>
                <span style={{ fontFamily: "var(--font-mono)", fontSize: 11, flex: 1, wordBreak: "break-all" }}>{lnBolt11}</span>
                <button className="btn btn-ghost" onClick={() => copyText(lnBolt11)}><Copy size={16} /></button>
              </div>
            </div>
          )}
        </div>

        <details className="glass-card" style={{ padding: 16, marginBottom: 14 }}>
          <summary className="text-secondary" style={{ fontWeight: 600, cursor: "pointer" }}>{t("taproot.openChannel")}</summary>
          <input className="input" type="number" placeholder={t("taproot.assetsToCommit")} value={chanAmount} onChange={(e) => setChanAmount(e.target.value)} style={{ margin: "12px 0 10px" }} />
          <input className="input" type="number" placeholder={t("taproot.feeSatVb")} value={chanFee} onChange={(e) => setChanFee(e.target.value)} style={{ marginBottom: 12 }} />
          <button className="btn btn-secondary" style={{ width: "100%" }} onClick={fundChannel} disabled={loading || !lnAssetId || !chanAmount || !lnPeer}>{t("taproot.openTheChannel")}</button>
        </details>

        {rfqQuotes && (
          <div className="text-muted" style={{ textAlign: "center", fontSize: 11 }}>⚡ RFQ : {rfqQuotes.buy_quotes} {t("taproot.rfqBuy")} · {rfqQuotes.sell_quotes} {t("taproot.rfqSell")}</div>
        )}
      </>
    );
  }

  function renderActivity() {
    return (
      <>
        <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", margin: "6px 0 14px" }}>
          <span className="title-lg">{t("taproot.activity")}</span>
          <button className="btn btn-ghost" onClick={fetchExtras} disabled={loading}><RefreshCw size={16} /> {t("refresh")}</button>
        </div>

        <div className="glass-card" style={{ padding: 20, marginBottom: 14 }}>
          <div className="text-secondary" style={{ fontWeight: 600, marginBottom: 12 }}>{t("taproot.transfers")} ({transfers.length})</div>
          {transfers.length === 0 ? <div className="text-muted">{t("taproot.noTransfers")}</div> : transfers.map((tr) => (
            <div key={tr.anchor_txid + tr.timestamp} style={{ ...inner, marginBottom: 8 }}>
              <div style={{ fontWeight: 600 }}>{tr.total_out.toLocaleString()} · {tr.inputs}→{tr.outputs}</div>
              <div className="text-muted" style={{ fontSize: 11 }}>{new Date(tr.timestamp * 1000).toLocaleString()} · bloc ~{tr.height_hint}</div>
              <div className="text-muted" style={{ fontSize: 11, fontFamily: "var(--font-mono)", wordBreak: "break-all" }}>{tr.anchor_txid}</div>
            </div>
          ))}
        </div>

        <div className="glass-card" style={{ padding: 20, marginBottom: 14 }}>
          <div className="text-secondary" style={{ fontWeight: 600, marginBottom: 12 }}>{t("taproot.receives")} ({receives.length})</div>
          {receives.length === 0 ? <div className="text-muted">{t("taproot.noReceives")}</div> : receives.map((r, i) => (
            <div key={r.outpoint + i} style={{ ...inner, marginBottom: 8 }}>
              <div style={{ fontWeight: 600 }}>{r.status}</div>
              <div className="text-muted" style={{ fontSize: 11 }}>{new Date(r.timestamp * 1000).toLocaleString()} · {r.utxo_amt_sat.toLocaleString()} sat</div>
            </div>
          ))}
        </div>

        <div className="glass-card" style={{ padding: 20, marginBottom: 14 }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 12 }}>
            <span className="text-secondary" style={{ fontWeight: 600 }}>{t("taproot.batches")} ({batches.length})</span>
            <button className="btn btn-ghost" onClick={cancelBatch} disabled={loading} style={{ fontSize: 12 }}>{t("taproot.cancelPending")}</button>
          </div>
          {batches.length === 0 ? <div className="text-muted">{t("taproot.noBatches")}</div> : batches.map((b) => (
            <div key={b.batch_key} style={{ ...inner, marginBottom: 8 }}>
              <div style={{ fontWeight: 600 }}>{b.state} · {b.assets} asset(s)</div>
              {b.batch_txid && <div className="text-muted" style={{ fontSize: 11, fontFamily: "var(--font-mono)", wordBreak: "break-all" }}>tx: {b.batch_txid}</div>}
            </div>
          ))}
        </div>

        {burns.length > 0 && (
          <div className="glass-card" style={{ padding: 20 }}>
            <div className="text-secondary" style={{ fontWeight: 600, marginBottom: 12 }}>{t("taproot.burns")} ({burns.length})</div>
            {burns.map((b, i) => (
              <div key={b.anchor_txid + i} className="text-muted" style={{ fontSize: 11, fontFamily: "var(--font-mono)", wordBreak: "break-all", marginBottom: 6 }}>
                <span style={{ color: "var(--danger)" }}>−{b.amount}</span> · {nameById[b.asset_id] || b.asset_id}
              </div>
            ))}
          </div>
        )}
      </>
    );
  }

  function renderAdvanced() {
    return (
      <>
        <div className="title-lg" style={{ margin: "6px 0 14px" }}><Settings size={20} style={{ verticalAlign: "middle" }} /> {t("taproot.advanced")}</div>

        {nodeInfo && (
          <div className="glass-card" style={{ padding: 20, marginBottom: 14 }}>
            <div className="text-secondary" style={{ fontWeight: 600, marginBottom: 12 }}>{t("taproot.nodeTapd")}</div>
            <div className="text-muted" style={{ fontSize: 12 }}>{t("taproot.network")} : {nodeInfo.network}</div>
            <div className="text-muted" style={{ fontSize: 12 }}>tapd : {nodeInfo.version}</div>
            <div className="text-muted" style={{ fontSize: 12 }}>lnd : {nodeInfo.lnd_version}</div>
            <div className="text-muted" style={{ fontSize: 11, fontFamily: "var(--font-mono)", wordBreak: "break-all" }}>{nodeInfo.lnd_pubkey}</div>
          </div>
        )}

        <div className="glass-card" style={{ padding: 20, marginBottom: 14 }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 12 }}>
            <span className="text-secondary" style={{ fontWeight: 600 }}>Universe</span>
            <button className="btn btn-ghost" onClick={fetchExtras} disabled={loading}><RefreshCw size={16} /></button>
          </div>
          {universeStats && (
            <div className="text-muted" style={{ fontSize: 12, marginBottom: 10 }}>{universeStats.num_assets} assets · {universeStats.num_groups} groupes · {universeStats.num_syncs} syncs · {universeStats.num_proofs} preuves</div>
          )}
          <div style={{ display: "flex", gap: 10, marginBottom: 12 }}>
            <input className="input" placeholder={t("taproot.universeHost")} value={syncHost} onChange={(e) => setSyncHost(e.target.value)} />
            <button className="btn btn-secondary" style={{ flex: "none" }} onClick={doSyncUniverse} disabled={loading || !syncHost}>{t("taproot.syncBtn")}</button>
          </div>
          {universeRoots.map((r) => (
            <div key={r.asset_id} style={{ ...inner, marginBottom: 8 }}>
              <div style={{ fontWeight: 600 }}>{r.asset_name || t("taproot.noName")}</div>
              <div className="text-muted" style={{ fontSize: 11, fontFamily: "var(--font-mono)", wordBreak: "break-all" }}>{r.asset_id}</div>
            </div>
          ))}
        </div>

        <div className="glass-card" style={{ padding: 20, marginBottom: 14 }}>
          <div className="text-secondary" style={{ fontWeight: 600, marginBottom: 12 }}><Download size={16} style={{ verticalAlign: "middle" }} /> {t("taproot.proofBackup")}</div>
          <div style={{ display: "flex", gap: 10, marginBottom: 12 }}>
            <button className="btn btn-secondary" style={{ flex: 1 }} onClick={exportProofs} disabled={proofLoading}>{proofLoading ? <span className="spinner" /> : <Download size={16} />} {t("taproot.exportProofs")}</button>
            {proofs.length > 0 && <button className="btn btn-ghost" onClick={downloadProofs}><Download size={16} /> {t("backup.download")}</button>}
          </div>
          <textarea className="input" placeholder={t("taproot.proofPlaceholder")} value={proofText} onChange={(e) => setProofText(e.target.value)} rows={3} style={{ marginBottom: 12 }} />
          <button className="btn btn-secondary" style={{ width: "100%" }} onClick={verifyProof} disabled={proofLoading || !proofText}>{proofLoading ? <span className="spinner" /> : <ShieldCheck size={16} />} {t("taproot.verify")}</button>
          {verifyResult !== null && (
            <div style={{ marginTop: 12, padding: 12, borderRadius: 10, background: verifyResult ? "rgba(34,197,94,0.1)" : "rgba(239,68,68,0.1)", border: `1px solid ${verifyResult ? "var(--success)" : "var(--danger)"}`, color: verifyResult ? "var(--success)" : "var(--danger)" }}>
              {verifyResult ? t("taproot.proofValid") : t("taproot.proofInvalid")}
            </div>
          )}
        </div>

        <div className="glass-card" style={{ padding: 20 }}>
          <div className="text-secondary" style={{ fontWeight: 600, marginBottom: 12 }}>{t("taproot.connection")}</div>
          <label style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 10, cursor: "pointer" }}>
            <input type="checkbox" checked={useTor} onChange={(e) => { setUseTor(e.target.checked); }} /> <span className="text-muted" style={{ fontSize: 13 }}>{t("taproot.useTor")} — {torStatus}</span>
          </label>
          <button className="btn btn-ghost" onClick={changeNode} style={{ width: "100%" }}>{t("taproot.changeNode")}</button>
        </div>
      </>
    );
  }

  const tabContent =
    tab === "portfolio" ? renderPortfolio()
    : tab === "receive" ? renderReceive()
    : tab === "send" ? renderSend()
    : tab === "ln" ? renderLightning()
    : tab === "activity" ? renderActivity()
    : renderAdvanced();

  return (
    <div style={{ width: "100%", height: "100%", display: "flex", flexDirection: "column", position: "relative" }}>
      {/* Header */}
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", padding: "20px 18px 8px" }}>
        <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
          <button className="btn btn-ghost" onClick={onBack} style={{ padding: "8px 10px" }}><ArrowLeft size={18} /></button>
          <div>
            <div className="title-lg" style={{ fontSize: 18 }}>{t("taproot.title")}</div>
          </div>
        </div>
        {connected && (
          <button className="btn btn-ghost" onClick={() => setTab("advanced")} style={{ padding: "8px 10px", color: tab === "advanced" ? "var(--accent-cyan)" : undefined }} title="Avancé"><Settings size={18} /></button>
        )}
      </div>

      {connected && (
        <div style={{ display: "flex", alignItems: "center", gap: 7, padding: "0 18px 6px" }}>
          <span style={{ width: 7, height: 7, borderRadius: "50%", background: "var(--success)", boxShadow: "0 0 8px var(--success)" }} />
          <span className="text-muted" style={{ fontSize: 11 }}>{t("taproot.connectedStatus")}{useTor ? " · " + t("taproot.viaTor") : ""}{nodeInfo ? ` · ${nodeInfo.network}` : ""}</span>
        </div>
      )}

      {/* Scroll body */}
      <div style={{ flex: 1, overflowY: "auto", padding: connected ? "6px 16px 96px" : "6px 16px 24px" }}>
        {!connected ? renderConnect() : tabContent}
        {connected && errorBox}
      </div>

      {/* Bottom tab bar + FAB (connected only) */}
      {connected && (
        <>
          <div style={{ position: "absolute", bottom: 88, right: 16, zIndex: 25 }}>
            <button
              onClick={() => setMintOpen(true)}
              title={t("taproot.mintTitle")}
              style={{ width: 56, height: 56, borderRadius: 18, border: "none", cursor: "pointer", color: "#000", background: "linear-gradient(135deg, var(--accent-cyan), var(--accent-purple))", boxShadow: "0 10px 30px rgba(0,240,255,0.35)", display: "grid", placeItems: "center" }}
            >
              <Plus size={26} />
            </button>
          </div>
          <div style={{ position: "absolute", bottom: 0, left: 0, right: 0, height: 72, display: "flex", padding: "8px 6px 12px", background: "rgba(5,5,8,0.92)", backdropFilter: "blur(16px)", borderTop: "1px solid var(--border)", zIndex: 20 }}>
            {TABS.map(({ id, label, Icon }) => (
              <button
                key={id}
                onClick={() => setTab(id)}
                style={{ flex: 1, display: "flex", flexDirection: "column", alignItems: "center", gap: 3, background: "none", border: "none", cursor: "pointer", fontSize: 10, paddingTop: 4, color: tab === id ? "var(--accent-cyan)" : "var(--text-muted)" }}
              >
                <Icon size={21} />
                {label}
              </button>
            ))}
          </div>
        </>
      )}

      {/* Mint sheet */}
      {mintOpen && (
        <div style={{ position: "absolute", inset: 0, background: "var(--bg-primary)", zIndex: 30, display: "flex", flexDirection: "column", padding: "20px 16px 16px", overflowY: "auto" }}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 14 }}>
            <span className="title-lg"><Plus size={20} style={{ verticalAlign: "middle" }} /> {t("taproot.mintTitle")}</span>
            <button className="btn btn-ghost" onClick={() => setMintOpen(false)} style={{ padding: "8px 10px" }}><X size={18} /></button>
          </div>
          <div className="glass-card" style={{ padding: 20 }}>
            <input className="input" placeholder={t("taproot.mintName")} value={mintName} onChange={(e) => setMintName(e.target.value)} style={{ marginBottom: 10 }} />
            <input className="input" type="number" placeholder={t("taproot.mintSupply")} value={mintAmount} onChange={(e) => setMintAmount(e.target.value)} style={{ marginBottom: 10 }} disabled={mintCollectible} />
            <input className="input" placeholder={t("taproot.mintMetadata")} value={mintMeta} onChange={(e) => setMintMeta(e.target.value)} style={{ marginBottom: 12 }} />
            <label style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 10, cursor: "pointer" }}>
              <input type="checkbox" checked={mintCollectible} onChange={(e) => setMintCollectible(e.target.checked)} /> <span className="text-secondary" style={{ fontSize: 13 }}>{t("taproot.collectible")}</span>
            </label>
            <label style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 12, cursor: "pointer" }}>
              <input type="checkbox" checked={mintNewGroup} onChange={(e) => setMintNewGroup(e.target.checked)} /> <span className="text-secondary" style={{ fontSize: 13 }}>{t("taproot.newGroup")}</span>
            </label>
            <input className="input" type="number" placeholder={t("taproot.feeAuto")} value={mintFee} onChange={(e) => setMintFee(e.target.value)} style={{ marginBottom: 12 }} />
            {errorBox}
            <button className="btn btn-primary" style={{ width: "100%" }} onClick={mint} disabled={loading || !mintName || (!mintCollectible && !mintAmount)}>{loading ? <span className="spinner" /> : <Plus size={16} />} {t("taproot.finalizeBatch")}</button>
          </div>
        </div>
      )}

      {/* Burn confirmation modal */}
      {burnTarget && (
        <div style={{ position: "absolute", inset: 0, zIndex: 40, background: "rgba(2,4,7,0.72)", backdropFilter: "blur(4px)", display: "flex", alignItems: "center", justifyContent: "center", padding: 22 }}>
          <div className="glass-card" style={{ padding: 22, width: "100%", maxWidth: 330 }}>
            <div style={{ width: 52, height: 52, borderRadius: 16, background: "rgba(239,68,68,0.12)", border: "1px solid rgba(239,68,68,0.3)", display: "grid", placeItems: "center", margin: "0 auto 12px", color: "var(--danger)" }}><Flame size={24} /></div>
            <h3 style={{ textAlign: "center", fontSize: 17, marginBottom: 6 }}>{t("taproot.burnAction")} {burnTarget.name || t("taproot.noName")} ?</h3>
            <p className="text-muted" style={{ textAlign: "center", fontSize: 13, marginBottom: 16 }}>{t("taproot.burnWarning")}</p>
            <label className="text-muted" style={{ fontSize: 11, display: "block", marginBottom: 6 }}>{t("taproot.burnAmount")} ({t("taproot.max")} {fmtAmount(burnTarget.amount, burnTarget.decimal_display)})</label>
            <input className="input" type="number" value={burnModalAmt} onChange={(e) => setBurnModalAmt(e.target.value)} style={{ marginBottom: 14 }} />
            <div style={{ display: "flex", gap: 10 }}>
              <button className="btn btn-secondary" style={{ flex: 1 }} onClick={() => setBurnTarget(null)}>{t("cancel")}</button>
              <button className="btn btn-danger" style={{ flex: 1 }} onClick={confirmBurn} disabled={loading || !burnModalAmt || Number(burnModalAmt) <= 0}>{loading ? <span className="spinner" /> : <Flame size={16} />} {t("taproot.burnAction")}</button>
            </div>
          </div>
        </div>
      )}

      <PasswordPrompt
        open={passwordPromptOpen}
        title={t("passwordPrompt.title")}
        onSubmit={(password) => {
          setPasswordPromptOpen(false);
          doConnect(password);
        }}
        onCancel={() => setPasswordPromptOpen(false)}
      />
    </div>
  );
}
