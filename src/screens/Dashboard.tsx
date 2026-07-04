import { useEffect, useState } from "react";
import { motion } from "framer-motion";
import { invoke } from "@tauri-apps/api/core";
import {
  Bitcoin,
  Zap,
  Layers,
  Shield,
  LogOut,
  Eye,
  EyeOff,
  RefreshCw,
  Copy,
  Check,
  QrCode,
  Lock,
  Send,
  ScanLine,
  History,
  Store,
} from "lucide-react";
import { scanQrCode } from "../lib/scan";
import { useI18n } from "../i18n/I18nContext";
import { useNotification } from "../contexts/NotificationContext";
import { MainnetBanner } from "../components/MainnetBanner";
import { PasswordPrompt } from "../components/PasswordPrompt";
import { classifyPaymentInput } from "../lib/paymentInput";

interface DashboardProps {
  onLogout: () => void;
  onBackup: () => void;
  onTaproot: () => void;
  onLightning: () => void;
  onArk: () => void;
  onHistory: () => void;
  onMarket: () => void;
  onLightningPay?: (invoice: string) => void;
}

export function Dashboard({ onLogout, onBackup, onTaproot, onLightning, onArk, onHistory, onMarket, onLightningPay }: DashboardProps) {
  const { t, lang, setLang } = useI18n();
  const { notify } = useNotification();
  const [showSeed, setShowSeed] = useState(false);
  const [mnemonic, setMnemonic] = useState("");
  const [address, setAddress] = useState("");
  const [arkAddress, setArkAddress] = useState("");
  const [balance, setBalance] = useState(0);
  const [arkBalance, setArkBalance] = useState(0);
  const [taprootTokens, setTaprootTokens] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState("");
  const [showSend, setShowSend] = useState(false);
  const [sendAddress, setSendAddress] = useState("");
  const [sendAmount, setSendAmount] = useState("");
  const [sendFee, setSendFee] = useState("10");
  const [sendTxid, setSendTxid] = useState("");
  const [network, setNetwork] = useState("signet");
  const [passwordPromptOpen, setPasswordPromptOpen] = useState(false);
  const [fiatRate, setFiatRate] = useState<{ EUR: number; USD: number } | null>(null);
  const [smartInput, setSmartInput] = useState("");

  function smartSend() {
    const s = smartInput.trim();
    if (!s) return;
    const { kind, value } = classifyPaymentInput(s);
    setSmartInput("");
    switch (kind) {
      case "onchain":
        setSendAddress(value);
        setShowSend(true);
        break;
      case "lightning":
        if (onLightningPay) onLightningPay(value);
        else onLightning();
        break;
      case "asset":
        notify("Adresse Taproot Asset détectée → Assets", "success");
        onTaproot();
        break;
      case "ark":
        notify("Adresse Ark détectée → Ark", "success");
        onArk();
        break;
      case "lnurl":
      case "lnaddress":
        notify("LNURL / Lightning Address — support à venir", "error");
        break;
      default:
        notify("Type non reconnu (adresse / facture)", "error");
    }
  }

  useEffect(() => {
    fetchAddress();
    fetchArkAddress();
    fetchArkBalance();
    fetchOnchainBalance();
    fetchTaprootBalances();
    loadNetwork();
  }, []);

  // Fetch a BTC fiat rate (mempool.space, no key) for an at-a-glance value of the
  // total balance. CSP already allows connect-src https:. Best-effort: on failure
  // (offline/mainnet-only) no fiat line is shown.
  useEffect(() => {
    let cancelled = false;
    fetch("https://mempool.space/api/v1/prices")
      .then((r) => (r.ok ? r.json() : null))
      .then((d) => {
        if (!cancelled && d && typeof d.EUR === "number") {
          setFiatRate({ EUR: d.EUR, USD: d.USD });
        }
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  async function fetchTaprootBalances() {
    try {
      const balances = await invoke<unknown[]>("list_taproot_balances");
      setTaprootTokens(Array.isArray(balances) ? balances.length : 0);
    } catch (e) {
      // tapd may not be connected yet; leave the count unknown rather than lying.
      console.error("taproot balances error:", e);
      setTaprootTokens(null);
    }
  }

  async function fetchOnchainBalance() {
    try {
      const sats = await invoke<number>("get_balance");
      setBalance(sats);
    } catch (e) {
      console.error("onchain balance error:", e);
    }
  }

  async function loadNetwork() {
    try {
      const cfg = await invoke<{ network: string }>("load_ark_config_command");
      setNetwork(cfg.network || "signet");
    } catch (e) {
      console.error("Failed to load network config:", e);
    }
  }

  async function fetchAddress() {
    setLoading(true);
    try {
      const addr = await invoke<string>("get_new_address");
      setAddress(addr);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function fetchArkAddress() {
    try {
      const addr = await invoke<string>("get_ark_address_command");
      setArkAddress(addr);
    } catch (e) {
      console.error("Ark address error:", e);
    }
  }

  async function fetchArkBalance() {
    try {
      const sats = await invoke<number>("get_ark_balance_command");
      setArkBalance(sats);
    } catch (e) {
      console.error("Ark balance error:", e);
    }
  }

  async function syncBalance() {
    setSyncing(true);
    setError("");
    try {
      await invoke("sync_wallet_command");
      const satoshis = await invoke<number>("get_balance");
      setBalance(satoshis);
      await invoke("sync_ark_wallet_command");
      await fetchArkBalance();
      await fetchTaprootBalances();
    } catch (e) {
      setError(String(e));
    } finally {
      setSyncing(false);
    }
  }

  async function sendOnchain() {
    if (!sendAddress || !sendAmount) return;
    setLoading(true);
    setError("");
    setSendTxid("");
    try {
      const txid = await invoke<string>("send_onchain", {
        address: sendAddress,
        amountSats: Number(sendAmount),
        feeRate: Number(sendFee),
      });
      setSendTxid(txid);
      setSendAddress("");
      setSendAmount("");
      await syncBalance();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  function revealSeed() {
    if (showSeed) {
      setShowSeed(false);
      setMnemonic("");
      return;
    }
    setPasswordPromptOpen(true);
  }

  async function handlePasswordSubmit(password: string) {
    setPasswordPromptOpen(false);
    setLoading(true);
    try {
      const words = await invoke<string>("reveal_mnemonic", { password });
      setMnemonic(words);
      setShowSeed(true);
    } catch {
      setError(t("passwordPrompt.error"));
    } finally {
      setLoading(false);
    }
  }

  async function copyAddress() {
    if (!address) return;
    try {
      await navigator.clipboard.writeText(address);
      notify(t("notifications.addressCopied"), "success");
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      notify(t("error"), "error");
    }
  }

  const btc = (balance / 100_000_000).toFixed(8);
  const total = balance + arkBalance;
  const totalBtc = (total / 100_000_000).toFixed(8);

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
          <h1 className="title-lg">{t("dashboard.title")}</h1>
          <p className="text-muted">{t("dashboard.subtitle")}</p>
        </div>
        <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
          <button className="btn btn-ghost" onClick={onMarket}>
            <Store size={18} /> Marché
          </button>
          <button className="btn btn-ghost" onClick={onHistory}>
            <History size={18} /> {t("dashboard.history")}
          </button>
          <button className="btn btn-ghost" onClick={onBackup}>
            <Lock size={18} /> {t("dashboard.backup")}
          </button>
          <select
            value={lang}
            onChange={(e) => setLang(e.target.value as "fr" | "en")}
            style={{
              background: "rgba(0,0,0,0.3)",
              border: "1px solid var(--border)",
              borderRadius: "8px",
              color: "var(--text-secondary)",
              padding: "6px 8px",
              fontSize: "12px",
              cursor: "pointer",
            }}
          >
            <option value="fr">FR</option>
            <option value="en">EN</option>
          </select>
          <button className="btn btn-ghost" onClick={onLogout}>
            <LogOut size={18} /> {t("dashboard.lock")}
          </button>
        </div>
      </motion.div>

      <MainnetBanner network={network} />

      <motion.div
        initial={{ opacity: 0, scale: 0.98 }}
        animate={{ opacity: 1, scale: 1 }}
        transition={{ delay: 0.1 }}
        className="glass-card"
        style={{ padding: "28px", marginBottom: "20px" }}
      >
        <div className="text-muted" style={{ marginBottom: "8px" }}>
          Solde total ({t(`network.${network}` as const)})
        </div>
        <div
          style={{
            fontSize: "42px",
            fontWeight: 700,
            background: "linear-gradient(135deg, #fff, #00f0ff)",
            WebkitBackgroundClip: "text",
            WebkitTextFillColor: "transparent",
          }}
        >
          {totalBtc} BTC
        </div>
        <div className="text-secondary">
          {total.toLocaleString()} sats · on-chain {balance.toLocaleString()} + Ark {arkBalance.toLocaleString()}
        </div>
        {fiatRate && (
          <div className="text-muted" style={{ fontSize: 13, marginTop: 4 }}>
            ≈ {((total / 100_000_000) * fiatRate.EUR).toLocaleString(undefined, { style: "currency", currency: "EUR", maximumFractionDigits: 2 })}
          </div>
        )}
      </motion.div>

      <div className="glass-card" style={{ padding: "14px 16px", marginBottom: "20px" }}>
        <div className="text-muted" style={{ fontSize: 12, marginBottom: 8 }}>Payer / Envoyer — colle n'importe quoi</div>
        <div style={{ display: "flex", gap: 8 }}>
          <input
            className="input"
            style={{ flex: 1 }}
            placeholder="bc1… · ark1… · tapbc1… · lnbc… · LNURL · user@domaine"
            value={smartInput}
            onChange={(e) => setSmartInput(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter") smartSend(); }}
          />
          <button className="btn btn-primary" style={{ flex: "none" }} onClick={smartSend} disabled={!smartInput.trim()}>
            <Send size={16} />
          </button>
        </div>
      </div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.2 }}
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(2, 1fr)",
          gap: "12px",
          marginBottom: "20px",
        }}
      >
        <AssetCard icon={<Bitcoin size={20} />} name={t("dashboard.onchainBalance")} balance={`${btc} BTC`} color="#f59e0b" />
        <AssetCard icon={<Zap size={20} />} name={t("dashboard.arkBalance")} balance={`${(arkBalance / 100_000_000).toFixed(8)} vBTC`} color="#00f0ff" onClick={onArk} />
        <AssetCard
          icon={<Layers size={20} />}
          name="Taproot Assets"
          balance={taprootTokens === null ? "—" : `${taprootTokens} ${taprootTokens === 1 ? "asset" : "assets"}`}
          color="#a855f7"
          onClick={onTaproot}
        />
        <AssetCard
          icon={<Shield size={20} />}
          name="Lightning (via Ark)"
          balance={`${arkBalance.toLocaleString()} sats`}
          color="#f97316"
          onClick={onLightning}
        />
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.25 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          {t("dashboard.onchainAddress")}
        </div>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "12px",
            padding: "14px",
            background: "rgba(0,0,0,0.3)",
            borderRadius: "12px",
            border: "1px solid var(--border)",
            marginBottom: "12px",
            wordBreak: "break-all",
          }}
        >
          <QrCode size={20} color="var(--accent-cyan)" />
          <span style={{ fontFamily: "var(--font-mono)", fontSize: "13px" }}>
            {address || t("loading")}
          </span>
        </div>
        <div style={{ display: "flex", gap: "10px" }}>
          <button className="btn btn-secondary" onClick={copyAddress} disabled={!address}>
            {copied ? <Check size={16} /> : <Copy size={16} />}
            {copied ? t("copied") : t("copy")}
          </button>
          <button type="button" className="btn btn-secondary" onClick={fetchAddress} disabled={loading}>
            <RefreshCw size={16} /> {t("dashboard.newAddress")}
          </button>
        </div>
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.28 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          {t("dashboard.arkAddress")}
        </div>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: "12px",
            padding: "14px",
            background: "rgba(0,0,0,0.3)",
            borderRadius: "12px",
            border: "1px solid var(--border)",
            marginBottom: "12px",
            wordBreak: "break-all",
          }}
        >
          <QrCode size={20} color="var(--accent-cyan)" />
          <span style={{ fontFamily: "var(--font-mono)", fontSize: "13px" }}>
            {arkAddress || "Chargement..."}
          </span>
        </div>
        <div style={{ display: "flex", gap: "10px" }}>
          <button
            className="btn btn-secondary"
            onClick={() => {
              if (!arkAddress) return;
              navigator.clipboard.writeText(arkAddress);
              setCopied(true);
              setTimeout(() => setCopied(false), 2000);
            }}
            disabled={!arkAddress}
          >
            {copied ? <Check size={16} /> : <Copy size={16} />}
            {copied ? t("copied") : t("copy")}
          </button>
          <button type="button" className="btn btn-secondary" onClick={fetchArkAddress} disabled={loading}>
            <RefreshCw size={16} /> {t("dashboard.newAddress")}
          </button>
        </div>
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.3 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div
          style={{
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            marginBottom: "12px",
          }}
        >
          <span className="text-secondary">{t("dashboard.backupSeed")}</span>
          <button className="btn btn-ghost" onClick={revealSeed} disabled={loading}>
            {showSeed ? <EyeOff size={16} /> : <Eye size={16} />}
            {showSeed ? t("dashboard.hide") : t("dashboard.reveal")}
          </button>
        </div>

        {showSeed && (
          <div className="mnemonic-grid" style={{ margin: 0 }}>
            {mnemonic.split(" ").map((word, i) => (
              <div key={i} className="mnemonic-word">
                <span className="index">{(i + 1).toString().padStart(2, "0")}</span>
                <span>{word}</span>
              </div>
            ))}
          </div>
        )}
      </motion.div>

      {showSend && (
        <motion.div
          initial={{ opacity: 0, y: 10 }}
          animate={{ opacity: 1, y: 0 }}
          className="glass-card"
          style={{ padding: "20px", marginBottom: "20px" }}
        >
          <div className="text-secondary" style={{ marginBottom: "12px" }}>
            <Send size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
            {t("dashboard.sendOnchain")}
          </div>
          <div style={{ display: "flex", gap: "10px", marginBottom: "10px" }}>
            <input
              className="input"
              placeholder={t("dashboard.bitcoinAddress")}
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
          <div style={{ display: "grid", gridTemplateColumns: "2fr 1fr", gap: "10px", marginBottom: "12px" }}>
            <input
              className="input"
              placeholder={t("dashboard.amountSats")}
              type="number"
              value={sendAmount}
              onChange={(e) => setSendAmount(e.target.value)}
            />
            <input
              className="input"
              placeholder={t("dashboard.feeRate")}
              type="number"
              value={sendFee}
              onChange={(e) => setSendFee(e.target.value)}
            />
          </div>
          <div style={{ display: "flex", gap: "10px" }}>
            <button type="button" className="btn btn-primary" onClick={sendOnchain} disabled={loading || !sendAddress || !sendAmount}>
              <Send size={16} /> {t("send")}
            </button>
            <button type="button" className="btn btn-ghost" onClick={() => setShowSend(false)}>
              {t("cancel")}
            </button>
          </div>
          {sendTxid && (
            <div className="text-muted" style={{ marginTop: "12px", fontSize: "12px", wordBreak: "break-all" }}>
              {t("dashboard.txid")}: {sendTxid}
            </div>
          )}
        </motion.div>
      )}

      {error && <div className="error" style={{ marginBottom: "12px" }}>{error}</div>}

      <PasswordPrompt
        open={passwordPromptOpen}
        title={t("passwordPrompt.title")}
        onSubmit={handlePasswordSubmit}
        onCancel={() => setPasswordPromptOpen(false)}
      />

      <div
        style={{
          display: "grid",
          gridTemplateColumns: "repeat(2, 1fr)",
          gap: "12px",
        }}
      >
        <button className="btn btn-primary" onClick={syncBalance} disabled={syncing}>
          {syncing ? <span className="spinner" /> : <RefreshCw size={18} />}
          {syncing ? `${t("sync")}...` : t("sync")}
        </button>
        <button className="btn btn-secondary" onClick={() => setShowSend(true)} disabled={showSend}>
          <Send size={18} /> {t("send")}
        </button>
      </div>
    </div>
  );
}

function AssetCard({
  icon,
  name,
  balance,
  color,
  onClick,
}: {
  icon: React.ReactNode;
  name: string;
  balance: string;
  color: string;
  onClick?: () => void;
}) {
  return (
    <div
      onClick={onClick}
      role={onClick ? "button" : undefined}
      style={{
        padding: "16px",
        background: "rgba(255,255,255,0.03)",
        borderRadius: "16px",
        border: "1px solid var(--border)",
        cursor: onClick ? "pointer" : "default",
        transition: "border-color 0.15s ease",
      }}
      onMouseEnter={onClick ? (e) => (e.currentTarget.style.borderColor = color) : undefined}
      onMouseLeave={onClick ? (e) => (e.currentTarget.style.borderColor = "var(--border)") : undefined}
    >
      <div style={{ color, marginBottom: "8px", display: "flex", justifyContent: "space-between", alignItems: "center" }}>
        {icon}
        {onClick && <span className="text-muted" style={{ fontSize: 18, lineHeight: 1 }}>›</span>}
      </div>
      <div className="text-muted" style={{ fontSize: "12px", marginBottom: "4px" }}>
        {name}
      </div>
      <div style={{ fontWeight: 600, fontSize: "14px" }}>{balance}</div>
    </div>
  );
}
