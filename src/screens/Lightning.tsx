import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { motion } from "framer-motion";
import { ArrowLeft, Zap, Send, QrCode, Copy, Check, RefreshCw, Link, ScanLine } from "lucide-react";
import { scanQrCode } from "../lib/scan";
import { QRImage } from "../components/QRImage";
import { useNotification } from "../contexts/NotificationContext";
import { useI18n } from "../i18n/I18nContext";
import { MainnetBanner } from "../components/MainnetBanner";

interface LightningProps {
  onBack: () => void;
  initialInvoice?: string;
}

export function Lightning({ onBack, initialInvoice }: LightningProps) {
  const { t } = useI18n();
  const { notify } = useNotification();
  const [arkBalance, setArkBalance] = useState(0);
  const [network, setNetwork] = useState("signet");
  const [payInvoice, setPayInvoice] = useState(initialInvoice || "");
  const [payAmount, setPayAmount] = useState("");
  const [payResult, setPayResult] = useState("");

  const [receiveAmount, setReceiveAmount] = useState("");
  const [receiveDesc, setReceiveDesc] = useState("");
  const [receiveInvoice, setReceiveInvoice] = useState("");

  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    fetchArkBalance();
    loadNetwork();
    if (initialInvoice) {
      setPayInvoice(initialInvoice);
    }
  }, [initialInvoice]);

  async function loadNetwork() {
    try {
      const cfg = await invoke<{ network: string }>("load_ark_config_command");
      setNetwork(cfg.network || "signet");
    } catch (e) {
      console.error("Failed to load network config:", e);
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

  async function pay() {
    if (!payInvoice) return;
    let amount: number | null = null;
    if (payAmount) {
      amount = Number(payAmount);
      if (!Number.isInteger(amount) || amount <= 0) {
        setError("Montant invalide");
        return;
      }
    }
    setLoading(true);
    setError("");
    setPayResult("");
    try {
      const result = await invoke<string>("pay_lightning_invoice", {
        invoice: payInvoice,
        amountSats: amount,
      });
      setPayResult(result);
      notify(t("notifications.paymentSent"), "success");
      setPayInvoice("");
      setPayAmount("");
      await fetchArkBalance();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function createInvoice() {
    if (!receiveAmount) return;
    const amount = Number(receiveAmount);
    if (!Number.isInteger(amount) || amount <= 0) {
      setError("Montant invalide");
      return;
    }
    setLoading(true);
    setError("");
    try {
      const invoice = await invoke<string>("create_bolt11_invoice", {
        amountSats: amount,
        description: receiveDesc,
      });
      setReceiveInvoice(invoice);
      notify(t("notifications.invoiceCreated"), "success");
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function claim() {
    setLoading(true);
    setError("");
    try {
      await invoke("claim_lightning_receives");
      notify("Reçus Lightning réclamés", "success");
      await fetchArkBalance();
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }

  async function copyInvoice() {
    if (!receiveInvoice) return;
    try {
      await navigator.clipboard.writeText(receiveInvoice);
      notify(t("notifications.invoiceCopied"), "success");
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      notify(t("error"), "error");
    }
  }

  const vbtc = (arkBalance / 100_000_000).toFixed(8);

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
          <h1 className="title-lg">{t("lightning.title")}</h1>
          <p className="text-muted">{t("lightning.subtitle")}</p>
        </div>
        <button className="btn btn-ghost" onClick={onBack}>
          <ArrowLeft size={18} /> Retour
        </button>
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        className="glass-card"
        style={{ padding: "24px", marginBottom: "20px" }}
      >
        <div className="text-muted" style={{ marginBottom: "8px" }}>{t("lightning.available")}</div>
        <div
          style={{
            fontSize: "36px",
            fontWeight: 700,
            background: "linear-gradient(135deg, #fff, #f97316)",
            WebkitBackgroundClip: "text",
            WebkitTextFillColor: "transparent",
          }}
        >
          {vbtc} vBTC
        </div>
        <div className="text-secondary">{arkBalance.toLocaleString()} sats</div>
        <button type="button" className="btn btn-ghost" onClick={fetchArkBalance} style={{ marginTop: "12px" }}>
          <RefreshCw size={16} /> {t("refresh")}
        </button>
      </motion.div>

      <MainnetBanner network={network} />

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.1 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <Send size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          {t("lightning.payInvoice")}
        </div>
        <div style={{ display: "flex", gap: "10px", marginBottom: "10px" }}>
          <textarea
            className="input"
            placeholder="lnbc..."
            value={payInvoice}
            onChange={(e) => setPayInvoice(e.target.value)}
            rows={3}
            style={{ flex: 1 }}
          />
          <button
            className="btn btn-ghost"
            onClick={async () => {
              const text = await scanQrCode();
              if (text) setPayInvoice(text);
            }}
            title="Scanner QR"
          >
            <ScanLine size={20} />
          </button>
        </div>
        <input
          className="input"
          placeholder="Montant personnalisé (sats) - optionnel"
          type="number"
          value={payAmount}
          onChange={(e) => setPayAmount(e.target.value)}
          style={{ marginBottom: "12px" }}
        />
        <button type="button" className="btn btn-primary" onClick={pay} disabled={loading || !payInvoice}>
          <Zap size={16} /> {t("lightning.pay")}
        </button>
        {payResult && (
          <div className="text-muted" style={{ marginTop: "12px", fontSize: "12px", wordBreak: "break-all" }}>
            {payResult}
          </div>
        )}
      </motion.div>

      <motion.div
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        transition={{ delay: 0.15 }}
        className="glass-card"
        style={{ padding: "20px", marginBottom: "20px" }}
      >
        <div className="text-secondary" style={{ marginBottom: "12px" }}>
          <QrCode size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          {t("lightning.receiveInvoice")}
        </div>
        <input
          className="input"
          placeholder="Montant (sats)"
          type="number"
          value={receiveAmount}
          onChange={(e) => setReceiveAmount(e.target.value)}
          style={{ marginBottom: "10px" }}
        />
        <input
          className="input"
          placeholder="Description"
          value={receiveDesc}
          onChange={(e) => setReceiveDesc(e.target.value)}
          style={{ marginBottom: "12px" }}
        />
        <button type="button"
          className="btn btn-secondary"
          onClick={createInvoice}
          disabled={loading || !receiveAmount}
        >
          <QrCode size={16} /> {t("lightning.createInvoice")}
        </button>
        {receiveInvoice && (
          <div style={{ display: "flex", justifyContent: "center", marginTop: "12px" }}>
            <QRImage value={receiveInvoice} />
          </div>
        )}
        {receiveInvoice && (
          <div
            style={{
              marginTop: "12px",
              padding: "12px",
              background: "rgba(0,0,0,0.2)",
              borderRadius: "10px",
              display: "flex",
              alignItems: "center",
              gap: "10px",
              wordBreak: "break-all",
            }}
          >
            <span style={{ fontFamily: "var(--font-mono)", fontSize: "12px", flex: 1 }}>
              {receiveInvoice}
            </span>
            <button className="btn btn-ghost" onClick={copyInvoice}>
              {copied ? <Check size={16} /> : <Copy size={16} />}
            </button>
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
          <Link size={16} style={{ marginRight: "8px", verticalAlign: "middle" }} />
          {t("lightning.claimPending")}
        </div>
        <button type="button" className="btn btn-secondary" onClick={claim} disabled={loading}>
          <RefreshCw size={16} /> {t("lightning.claimPending")}
        </button>
      </motion.div>

      {error && <div className="error">{error}</div>}
    </div>
  );
}
