import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { motion } from "framer-motion";
import {
  ArrowLeft,
  Zap,
  RefreshCw,
  QrCode,
  Copy,
  Check,
  Send,
  ScanLine,
  Anchor,
  Settings,
  AlertTriangle,
  LogOut,
  Shield,
} from "lucide-react";
import { scanQrCode } from "../lib/scan";
import { useNotification } from "../contexts/NotificationContext";
import { useI18n } from "../i18n/I18nContext";
import { ConfirmModal } from "../components/ConfirmModal";
import { PasswordPrompt } from "../components/PasswordPrompt";

interface ArkProps {
  onBack: () => void;
}

interface ArkConfig {
  server_address: string;
  esplora_address?: string;
  server_access_token?: string;
  network: string;
}

interface ExitSummary {
  vtxo_id: string;
  amount_sats: number;
  state: string;
  claimable: boolean;
  pending: boolean;
}

interface ExitStatus {
  has_pending: boolean;
  pending_sats?: number;
  total_sats: number;
  claimable_sats: number;
  exits: ExitSummary[];
}

interface VtxoInfo {
  vtxo_id: string;
  amount_sats: number;
  expiry_height: number;
}

export function Ark({ onBack }: ArkProps) {
  const { t } = useI18n();
  const { notify } = useNotification();
  const [balance, setBalance] = useState(0);
  const [address, setAddress] = useState("");
  const [arkadeAddress, setArkadeAddress] = useState("");
  const [sendAddress, setSendAddress] = useState("");
  const [sendAmount, setSendAmount] = useState("");
  const [sendResult, setSendResult] = useState("");
  const [boardAddress, setBoardAddress] = useState("");
  const [loading, setLoading] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [copied, setCopied] = useState(false);
  const [arkadeCopied, setArkadeCopied] = useState(false);
  const [error, setError] = useState("");
  const syncingRef = useRef(false);

  // ASP config
  const [config, setConfig] = useState<ArkConfig>({
    server_address: "",
    esplora_address: "",
    server_access_token: "",
    network: "bitcoin",
  });
  const [configSaved, setConfigSaved] = useState(false);

  // Off-board
  const [offboardAddress, setOffboardAddress] = useState("");
  const [offboardAmount, setOffboardAmount] = useState("");
  const [offboardAll, setOffboardAll] = useState(false);
  const [offboardResult, setOffboardResult] = useState("");

  // Exit
  const [exitStatus, setExitStatus] = useState<ExitStatus | null>(null);
  const [vtxos, setVtxos] = useState<VtxoInfo[]>([]);
  const [tipHeight, setTipHeight] = useState<number | null>(null);
  const [drainAddress, setDrainAddress] = useState("");
  const [exitResult, setExitResult] = useState("");

  // Destructive-action confirmation
  const [confirm, setConfirm] = useState<{
    open: boolean;
    title: string;
    message: string;
    onConfirm: () => void;
  }>({ open: false, title: "", message: "", onConfirm: () => {} });
  const [passwordPromptOpen, setPasswordPromptOpen] = useState(false);

  useEffect(() => {
    fetchAddress();
    fetchBalance();
    loadConfig();
    fetchVtxos();

    // Auto-sync ARK balance every 30 seconds while this screen is open.
    syncArk();
    const interval = setInterval(() => {
      syncArk();
    }, 30000);
    return () => clearInterval(interval);
  }, []);

  async function fetchAddress() {
    setLoading(true);
    try {
      const addr = await invoke<string>("get_ark_address_command");
      setAddress(addr);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function fetchBalance() {
    try {
      const sats = await invoke<number>("get_ark_balance_command");
      setBalance(sats);
    } catch (e) {
      console.error("Ark balance error:", e);
    }
  }

  async function syncArk() {
    if (syncingRef.current) return;
    syncingRef.current = true;
    setSyncing(true);
    setError("");
    try {
      await invoke("sync_ark_wallet_command");
      await fetchBalance();
    } catch (e) {
      setError(String(e));
    } finally {
      setSyncing(false);
      syncingRef.current = false;
    }
  }

  async function fetchBoardAddress() {
    setLoading(true);
    setError("");
    try {
      const addr = await invoke<string>("get_board_funding_address");
      setBoardAddress(addr);
      notify("Adresse de board créée", "success");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function loadConfig() {
    try {
      const cfg = await invoke<ArkConfig>("load_ark_config_command");
      setConfig({
        server_address: cfg.server_address || "",
        esplora_address: cfg.esplora_address || "",
        server_access_token: cfg.server_access_token || "",
        network: cfg.network || "signet",
      });
    } catch (e) {
      console.error("Failed to load Ark config:", e);
    }
  }

  function saveConfig() {
    setError("");
    // Saving an access token requires the wallet password to encrypt it.
    if (config.server_access_token && config.server_access_token.trim().length > 0) {
      setPasswordPromptOpen(true);
      return;
    }
    doSaveConfig("");
  }

  async function doSaveConfig(password: string) {
    setLoading(true);
    setError("");
    try {
      await invoke("save_ark_config_command", {
        password,
        config: {
          server_address: config.server_address,
          esplora_address: config.esplora_address || null,
          server_access_token: config.server_access_token || null,
          network: config.network,
        },
      });
      notify(t("notifications.configSaved"), "success");
      setConfigSaved(true);
      setTimeout(() => setConfigSaved(false), 2000);
      // Clear the token from UI state so it is not left in memory.
      setConfig((prev) => ({ ...prev, server_access_token: "" }));
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function refreshVtxos() {
    setLoading(true);
    setError("");
    try {
      const result = await invoke<string>("refresh_ark_vtxos_command");
      setError("");
      notify(`Refresh : ${result}`, "success");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  function offboard() {
    if (!offboardAddress || (!offboardAll && !offboardAmount)) return;
    if (!offboardAll) {
      const amount = Number(offboardAmount);
      if (!Number.isInteger(amount) || amount <= 0) {
        setError("Montant invalide");
        return;
      }
    }
    setConfirm({
      open: true,
      title: t("ark.offboardTitle"),
      message: offboardAll
        ? `Retirer l'intégralité du solde ARK vers ${offboardAddress} ?`
        : `Retirer ${offboardAmount} sats vers ${offboardAddress} ?`,
      onConfirm: async () => {
        setLoading(true);
        setError("");
        setOffboardResult("");
        try {
          let txid: string;
          if (offboardAll) {
            txid = await invoke<string>("offboard_all_command", {
              address: offboardAddress,
            });
          } else {
            txid = await invoke<string>("send_ark_onchain_command", {
              address: offboardAddress,
              amountSats: Number(offboardAmount),
            });
          }
          setOffboardResult(txid);
          notify(t("notifications.offboardInitiated"), "success");
          setOffboardAddress("");
          setOffboardAmount("");
          setOffboardAll(false);
          await fetchBalance();
        } catch (e) {
          setError(String(e));
        } finally {
          setLoading(false);
        }
      },
    });
  }

  function startExit() {
    setConfirm({
      open: true,
      title: t("ark.exitTitle"),
      message: t("ark.exitHint"),
      onConfirm: async () => {
        setLoading(true);
        setError("");
        try {
          await invoke("start_ark_exit_command");
          notify(t("notifications.exitStarted"), "info");
          await fetchExitStatus();
        } catch (e) {
          setError(String(e));
        } finally {
          setLoading(false);
        }
      },
    });
  }

  async function syncExits() {
    setLoading(true);
    setError("");
    try {
      await invoke("sync_ark_exits_command");
      await fetchExitStatus();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function fetchVtxos() {
    try {
      setVtxos(await invoke<VtxoInfo[]>("list_ark_vtxos_command"));
    } catch (e) {
      console.error("list vtxos error:", e);
    }
    // Current chain tip so we can turn absolute expiry heights into "in ~N days".
    try {
      const res = await fetch("https://mempool.space/api/blocks/tip/height");
      if (res.ok) {
        const h = parseInt(await res.text(), 10);
        if (!Number.isNaN(h)) setTipHeight(h);
      }
    } catch {
      // offline — no blocks-left estimate, still show expiry heights
    }
  }

  async function fetchExitStatus() {
    setLoading(true);
    setError("");
    try {
      const status = await invoke<ExitStatus>("get_ark_exit_status_command");
      setExitStatus(status);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  function drainExits() {
    if (!drainAddress || !exitStatus || exitStatus.claimable_sats === 0) return;
    setConfirm({
      open: true,
      title: "Claim des exits",
      message: `Claim ${exitStatus.claimable_sats.toLocaleString()} sats vers ${drainAddress} ?`,
      onConfirm: async () => {
        setLoading(true);
        setError("");
        setExitResult("");
        try {
          const txid = await invoke<string>("drain_ark_exits_command", {
            address: drainAddress,
          });
          setExitResult(txid);
          notify(t("notifications.exitsClaimed"), "success");
          setDrainAddress("");
          await fetchExitStatus();
        } catch (e) {
          setError(String(e));
        } finally {
          setLoading(false);
        }
      },
    });
  }

  async function sendArk() {
    if (!sendAddress || !sendAmount) return;
    const amount = Number(sendAmount);
    if (!Number.isInteger(amount) || amount <= 0) {
      setError("Montant invalide");
      return;
    }
    setLoading(true);
    setError("");
    setSendResult("");
    try {
      const result = await invoke<string>("send_ark_payment", {
        address: sendAddress,
        amountSats: amount,
      });
      setSendResult(result);
      notify(t("notifications.paymentSent"), "success");
      setSendAddress("");
      setSendAmount("");
      await fetchBalance();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  function copyAddress() {
    if (!address) return;
    navigator.clipboard.writeText(address);
    notify(t("notifications.addressCopied"), "success");
    setCopied(true);
    setTimeout(() => setCopied(false), 2000);
  }

  async function fetchArkadeAddress() {
    setLoading(true);
    setError("");
    try {
      const addr = await invoke<string>("get_arkade_address_command");
      setArkadeAddress(addr);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  function copyArkadeAddress() {
    if (!arkadeAddress) return;
    navigator.clipboard.writeText(arkadeAddress);
    notify(t("notifications.addressCopied"), "success");
    setArkadeCopied(true);
    setTimeout(() => setArkadeCopied(false), 2000);
  }

  const vbtc = (balance / 100_000_000).toFixed(8);

  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        flexDirection: "column",
        padding: "24px",
        overflow: "auto",
      }}
    >
      <motion.div
        initial={{ opacity: 0, y: -10 }}
        animate={{ opacity: 1, y: 0 }}
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          marginBottom: "24px",
        }}
      >
        <div>
          <h1 className="title-lg">ARK</h1>
          <p className="text-muted">Layer 2 Bitcoin</p>
        </div>
        <button className="btn btn-ghost" onClick={onBack}>
          <ArrowLeft size={18} /> Retour
        </button>
      </motion.div>

      {config.network === "bitcoin" && (
        <motion.div
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          style={{
            padding: "14px",
            borderRadius: "12px",
            background: "rgba(255,68,68,0.12)",
            border: "1px solid var(--error)",
            color: "var(--error)",
            fontSize: "13px",
            marginBottom: "20px",
            display: "flex",
            alignItems: "flex-start",
            gap: "10px",
          }}
        >
          <AlertTriangle size={18} style={{ flexShrink: 0, marginTop: "2px" }} />
          <span>{t("ark.mainnetBanner")}</span>
        </motion.div>
      )}

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        className="glass-card"
        style={{ padding: "24px", marginBottom: "20px" }}
      >
        <div className="text-muted" style={{ marginBottom: "8px" }}>Solde ARK</div>
        <div
          style={{
            fontSize: "36px",
            fontWeight: 700,
            background: "linear-gradient(135deg, #fff, #00f0ff)",
            WebkitBackgroundClip: "text",
            WebkitTextFillColor: "transparent",
          }}
        >
          {vbtc} vBTC
        </div>
        <div className="text-secondary">{balance.toLocaleString()} sats</div>
        <div style={{ display: "flex", gap: "10px", marginTop: "12px", flexWrap: "wrap" }}>
          <button className="btn btn-ghost" onClick={syncArk} disabled={syncing}>
            {syncing ? <span className="spinner" /> : <RefreshCw size={16} />}
            {syncing ? "Sync..." : "Synchroniser"}
          </button>
          <button className="btn btn-ghost" onClick={refreshVtxos} disabled={loading}>
            <Shield size={16} /> Refresh VTXOs
          </button>
        </div>
      </motion.div>

      {vtxos.length > 0 &&
        (() => {
          const soonest = vtxos.reduce((m, v) => Math.min(m, v.expiry_height), Infinity);
          const totalSats = vtxos.reduce((s, v) => s + v.amount_sats, 0);
          const blocksLeft = tipHeight != null ? soonest - tipHeight : null;
          const daysLeft = blocksLeft != null ? Math.floor(blocksLeft / 144) : null;
          const warn = blocksLeft != null && blocksLeft < 1008; // ~1 week of blocks
          return (
            <motion.div
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              transition={{ delay: 0.05 }}
              className="glass-card"
              style={{ padding: "20px", marginBottom: "20px", borderColor: warn ? "rgba(245,158,11,0.5)" : undefined }}
            >
              <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 10 }}>
                <span className="text-secondary">
                  <Shield size={16} style={{ verticalAlign: "middle", marginRight: 8 }} />
                  VTXOs · expiration
                </span>
                <button className="btn btn-ghost" style={{ fontSize: 13 }} disabled={loading} onClick={async () => { await refreshVtxos(); await fetchVtxos(); }}>
                  <RefreshCw size={14} /> Refresh
                </button>
              </div>
              <div className="text-muted" style={{ fontSize: 13 }}>
                {vtxos.length} VTXO{vtxos.length > 1 ? "s" : ""} · {totalSats.toLocaleString()} sats · prochaine expiration : bloc {soonest.toLocaleString()}
                {blocksLeft != null && ` (~${daysLeft} j · ${blocksLeft.toLocaleString()} blocs)`}
              </div>
              {warn && (
                <div style={{ marginTop: 10, padding: "10px 12px", borderRadius: 8, background: "rgba(245,158,11,0.12)", border: "1px solid rgba(245,158,11,0.4)", fontSize: 13 }}>
                  ⚠️ Un VTXO expire bientôt — fais un <b>Refresh</b> pour éviter une sortie on-chain (exit unilatéral).
                </div>
              )}
            </motion.div>
          );
        })()}

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.05 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <Settings size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          Config ASP
        </div>
        <input
          className="input"
          placeholder="URL du serveur ARK"
          value={config.server_address}
          onChange={(e) => setConfig({ ...config, server_address: e.target.value })}
          style={{ marginBottom: "10px" }}
        />
        <input
          className="input"
          placeholder="URL Esplora (optionnel)"
          value={config.esplora_address}
          onChange={(e) => setConfig({ ...config, esplora_address: e.target.value })}
          style={{ marginBottom: "10px" }}
        />
        <input
          className="input"
          placeholder="Access token (optionnel)"
          value={config.server_access_token}
          onChange={(e) => setConfig({ ...config, server_access_token: e.target.value })}
          style={{ marginBottom: "10px" }}
        />
        <input
          className="input"
          readOnly
          value={t("network.bitcoin")}
          style={{ marginBottom: "12px" }}
        />
        {config.network === "bitcoin" && (
          <div
            style={{
              padding: "10px",
              borderRadius: "8px",
              background: "rgba(255,68,68,0.1)",
              border: "1px solid var(--error)",
              color: "var(--error)",
              fontSize: "12px",
              marginBottom: "12px",
            }}
          >
            {t("ark.mainnetWarning")}
          </div>
        )}
        <div className="text-muted" style={{ fontSize: "11px", marginBottom: "12px" }}>
          <AlertTriangle size={12} style={{ marginRight: "6px", verticalAlign: "middle" }} />
          La nouvelle config sera prise en compte au prochain déverrouillage. Changer d'ASP nécessite de réinitialiser le wallet Ark.
        </div>
        <button className="btn btn-secondary" onClick={saveConfig} disabled={loading}>
          {configSaved ? <Check size={16} /> : <Settings size={16} />}
          {configSaved ? "Sauvegardé" : "Sauvegarder"}
        </button>
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.1 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <QrCode size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          Adresse de réception ARK
        </div>
        <div
          style={{
            padding: "12px",
            background: "rgba(0,0,0,0.2)",
            borderRadius: "10px",
            border: "1px solid var(--border)",
            wordBreak: "break-all",
            marginBottom: "12px",
            display: "flex",
            alignItems: "center",
            gap: "10px",
          }}
        >
          <span style={{ fontFamily: "var(--font-mono)", fontSize: "12px", flex: 1 }}>
            {address || "Chargement..."}
          </span>
          <button className="btn btn-ghost" onClick={copyAddress} disabled={!address}>
            {copied ? <Check size={16} /> : <Copy size={16} />}
          </button>
        </div>
        <button className="btn btn-secondary" onClick={fetchAddress} disabled={loading}>
          <RefreshCw size={16} /> Nouvelle adresse
        </button>
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.12 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <QrCode size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          {t("ark.arkadeAddress")}
        </div>
        <div
          style={{
            padding: "12px",
            background: "rgba(0,0,0,0.2)",
            borderRadius: "10px",
            border: "1px solid var(--border)",
            wordBreak: "break-all",
            marginBottom: "12px",
            display: "flex",
            alignItems: "center",
            gap: "10px",
          }}
        >
          <span style={{ fontFamily: "var(--font-mono)", fontSize: "12px", flex: 1 }}>
            {arkadeAddress || t("loading")}
          </span>
          <button className="btn btn-ghost" onClick={copyArkadeAddress} disabled={!arkadeAddress}>
            {arkadeCopied ? <Check size={16} /> : <Copy size={16} />}
          </button>
        </div>
        <div
          style={{
            padding: "10px",
            borderRadius: "8px",
            background: "rgba(255,170,0,0.1)",
            border: "1px solid var(--warning, #fa0)",
            color: "var(--warning, #fa0)",
            fontSize: "12px",
            marginBottom: "12px",
            display: "flex",
            alignItems: "flex-start",
            gap: "8px",
          }}
        >
          <AlertTriangle size={16} style={{ flexShrink: 0, marginTop: "2px" }} />
          <span>{t("ark.arkadeWarning")}</span>
        </div>
        <button className="btn btn-secondary" onClick={fetchArkadeAddress} disabled={loading}>
          <RefreshCw size={16} /> {t("ark.newArkadeAddress")}
        </button>
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.15 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <Send size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          Envoyer ARK
        </div>
        <div style={{ display: "flex", gap: "10px", marginBottom: "10px" }}>
          <input
            className="input"
            placeholder="Adresse ARK"
            value={sendAddress}
            onChange={(e) => setSendAddress(e.target.value)}
            style={{ flex: 1 }}
          />
          <button
            className="btn btn-ghost"
            onClick={async () => {
              const text = await scanQrCode();
              if (text) setSendAddress(text);
            }}
            title="Scanner QR"
          >
            <ScanLine size={20} />
          </button>
        </div>
        <input
          className="input"
          placeholder="Montant (sats)"
          type="number"
          value={sendAmount}
          onChange={(e) => setSendAmount(e.target.value)}
          style={{ marginBottom: "12px" }}
        />
        <button
          className="btn btn-primary"
          onClick={sendArk}
          disabled={loading || !sendAddress || !sendAmount}
        >
          <Zap size={16} /> Envoyer
        </button>
        {sendResult && (
          <div className="text-muted" style={{ marginTop: "12px", fontSize: "12px" }}>
            {sendResult}
          </div>
        )}
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.2 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <Anchor size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          Board on-chain → ARK
        </div>
        <p className="text-muted" style={{ fontSize: "12px", marginBottom: "12px" }}>
          Envoyez des bitcoins on-chain à cette adresse pour les boarder dans ARK.
        </p>
        <div
          style={{
            padding: "12px",
            background: "rgba(0,0,0,0.2)",
            borderRadius: "10px",
            border: "1px solid var(--border)",
            wordBreak: "break-all",
            marginBottom: "12px",
            display: "flex",
            alignItems: "center",
            gap: "10px",
          }}
        >
          <span style={{ fontFamily: "var(--font-mono)", fontSize: "12px", flex: 1 }}>
            {boardAddress || "Chargement..."}
          </span>
          <button
            className="btn btn-ghost"
            onClick={() => {
              if (!boardAddress) return;
              navigator.clipboard.writeText(boardAddress);
              setCopied(true);
              setTimeout(() => setCopied(false), 2000);
            }}
            disabled={!boardAddress}
          >
            {copied ? <Check size={16} /> : <Copy size={16} />}
          </button>
        </div>
        <button className="btn btn-secondary" onClick={fetchBoardAddress} disabled={loading}>
          <RefreshCw size={16} /> Nouvelle adresse de board
        </button>
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.25 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <LogOut size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          Off-board ARK → on-chain
        </div>
        <div style={{ display: "flex", gap: "10px", marginBottom: "10px" }}>
          <input
            className="input"
            placeholder="Adresse Bitcoin on-chain"
            value={offboardAddress}
            onChange={(e) => setOffboardAddress(e.target.value)}
            style={{ flex: 1 }}
          />
          <button
            className="btn btn-ghost"
            onClick={async () => {
              const text = await scanQrCode();
              if (text) setOffboardAddress(text);
            }}
            title="Scanner QR"
          >
            <ScanLine size={20} />
          </button>
        </div>
        {!offboardAll && (
          <input
            className="input"
            placeholder="Montant (sats)"
            type="number"
            value={offboardAmount}
            onChange={(e) => setOffboardAmount(e.target.value)}
            style={{ marginBottom: "10px" }}
          />
        )}
        <label
          style={{
            display: "flex",
            alignItems: "center",
            gap: "8px",
            marginBottom: "12px",
            fontSize: "13px",
            color: "var(--text-muted)",
            cursor: "pointer",
          }}
        >
          <input
            type="checkbox"
            checked={offboardAll}
            onChange={(e) => setOffboardAll(e.target.checked)}
          />
          Tout retirer (offboard all)
        </label>
        <button
          className="btn btn-primary"
          onClick={offboard}
          disabled={loading || !offboardAddress || (!offboardAll && !offboardAmount)}
        >
          <LogOut size={16} /> Retirer
        </button>
        {offboardResult && (
          <div className="text-muted" style={{ marginTop: "12px", fontSize: "12px", wordBreak: "break-all" }}>
            TXID: {offboardResult}
          </div>
        )}
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.3 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <AlertTriangle size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          Sortie unilatérale
        </div>
        <p className="text-muted" style={{ fontSize: "12px", marginBottom: "12px" }}>
          Force le retrait de tous les VTXOs vers une adresse on-chain sans coopération de l'ASP. Lent et on-chain.
        </p>
        <div style={{ display: "flex", gap: "10px", marginBottom: "12px", flexWrap: "wrap" }}>
          <button className="btn btn-secondary" onClick={startExit} disabled={loading}>
            Démarrer
          </button>
          <button className="btn btn-ghost" onClick={syncExits} disabled={loading}>
            <RefreshCw size={16} /> Actualiser
          </button>
          <button className="btn btn-ghost" onClick={fetchExitStatus} disabled={loading}>
            Voir l'état
          </button>
        </div>

        {exitStatus && (
          <div style={{ marginBottom: "12px" }}>
            <div className="text-muted" style={{ fontSize: "12px", marginBottom: "8px" }}>
              Total: {exitStatus.total_sats.toLocaleString()} sats · Claimable: {exitStatus.claimable_sats.toLocaleString()} sats
              {exitStatus.pending_sats !== undefined && ` · En attente: ${exitStatus.pending_sats.toLocaleString()} sats`}
            </div>
            {exitStatus.exits.length > 0 && (
              <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                {exitStatus.exits.map((ex) => (
                  <div
                    key={ex.vtxo_id}
                    style={{
                      padding: "10px",
                      background: "rgba(0,0,0,0.2)",
                      borderRadius: "8px",
                      border: "1px solid var(--border)",
                      fontSize: "12px",
                    }}
                  >
                    <div style={{ fontFamily: "var(--font-mono)", wordBreak: "break-all" }}>{ex.vtxo_id}</div>
                    <div className="text-muted">
                      {ex.amount_sats.toLocaleString()} sats · {ex.state} · {ex.claimable ? "claimable" : ex.pending ? "pending" : "done"}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}

        <div style={{ display: "flex", gap: "10px" }}>
          <input
            className="input"
            placeholder="Adresse de réception on-chain"
            value={drainAddress}
            onChange={(e) => setDrainAddress(e.target.value)}
            style={{ flex: 1 }}
          />
          <button
            className="btn btn-primary"
            onClick={drainExits}
            disabled={loading || !drainAddress || !exitStatus || exitStatus.claimable_sats === 0}
          >
            Claim
          </button>
        </div>
        {exitResult && (
          <div className="text-muted" style={{ marginTop: "12px", fontSize: "12px", wordBreak: "break-all" }}>
            TXID: {exitResult}
          </div>
        )}
      </motion.div>

      {error && <div className="error">{error}</div>}

      <PasswordPrompt
        open={passwordPromptOpen}
        title={t("passwordPrompt.title")}
        onSubmit={(password) => {
          setPasswordPromptOpen(false);
          doSaveConfig(password);
        }}
        onCancel={() => setPasswordPromptOpen(false)}
      />

      <ConfirmModal
        open={confirm.open}
        title={confirm.title}
        message={confirm.message}
        confirmVariant="danger"
        onConfirm={() => {
          setConfirm((c) => ({ ...c, open: false }));
          confirm.onConfirm();
        }}
        onCancel={() => setConfirm((c) => ({ ...c, open: false }))}
      />
    </div>
  );
}
