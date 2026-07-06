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
} from "lucide-react";
import { useNotification } from "../contexts/NotificationContext";

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

const card: CSSProperties = { padding: 18, marginBottom: 14 };
const label: CSSProperties = { fontSize: 12, fontWeight: 600, marginBottom: 6, display: "block" };
const row: CSSProperties = { display: "flex", gap: 8, marginBottom: 8 };

function num(v: string): number {
  return Math.floor(Number(v) || 0);
}

function short(s: string, head = 10, tail = 6): string {
  return s.length > head + tail + 1 ? `${s.slice(0, head)}…${s.slice(-tail)}` : s;
}

const CREDIT_KINDS = new Set(["mint", "receive", "transfer_in"]);

function kindLabel(kind: string): string {
  switch (kind) {
    case "mint":
      return "Mint";
    case "receive":
      return "Reçu";
    case "send":
      return "Envoyé";
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
  const [metaFor, setMetaFor] = useState<string | null>(null);
  const [meta, setMeta] = useState<AssetMeta | null>(null);
  const [metaLoading, setMetaLoading] = useState(false);

  // Transaction history (per-user ledger)
  const [history, setHistory] = useState<LedgerEvent[]>([]);

  // Mint
  const [mintName, setMintName] = useState("");
  const [mintAmount, setMintAmount] = useState("");
  const [mintMeta, setMintMeta] = useState("");
  const [mintCollectible, setMintCollectible] = useState(false);
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
      try {
        setHistory(await invoke<LedgerEvent[]>("gateway_history", { limit: 50 }));
      } catch {
        setHistory([]);
      }
    } catch (e) {
      notify(String(e), "error");
    } finally {
      setLoading(false);
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
              <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13, marginBottom: 10 }}>
                <input
                  type="checkbox"
                  checked={mintCollectible}
                  onChange={(e) => setMintCollectible(e.target.checked)}
                />
                Collectible (pièce unique)
              </label>
              <input
                className="input"
                style={{ width: "100%", marginBottom: 4 }}
                type="number"
                placeholder="Frais sat/vB (optionnel)"
                value={mintFee}
                onChange={(e) => setMintFee(e.target.value)}
              />
              <p className="text-muted" style={{ fontSize: 10, marginBottom: 10 }}>
                Vide = estimation automatique du nœud.
              </p>
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
              <input
                className="input"
                style={{ width: "100%", marginBottom: 8 }}
                placeholder="Adresse Taproot du destinataire"
                value={sendAddr}
                onChange={(e) => setSendAddr(e.target.value)}
              />
              <div style={row}>
                <input
                  className="input"
                  style={{ flex: 1 }}
                  type="number"
                  placeholder="Frais sat/vB (optionnel)"
                  value={sendFee}
                  onChange={(e) => setSendFee(e.target.value)}
                />
                <button className="btn btn-primary" onClick={doSend} disabled={busy}>
                  {busy ? <span className="spinner" /> : null} Envoyer
                </button>
              </div>
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
        </>
      )}
    </div>
  );
}
