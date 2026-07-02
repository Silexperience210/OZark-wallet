import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AnimatePresence, motion } from "framer-motion";
import { getCurrent, onOpenUrl } from "@tauri-apps/plugin-deep-link";
import { I18nProvider, useI18n } from "./i18n/I18nContext";
import { NotificationProvider, useNotification } from "./contexts/NotificationContext";
import { NotificationContainer } from "./components/Notification";
import { UpdateBanner } from "./components/UpdateBanner";
import { ConfirmModal } from "./components/ConfirmModal";
import { Welcome } from "./screens/Welcome";
import { CreateWallet } from "./screens/CreateWallet";
import { ImportWallet } from "./screens/ImportWallet";
import { UnlockWallet } from "./screens/UnlockWallet";
import { Dashboard } from "./screens/Dashboard";
import { BackupWallet } from "./screens/BackupWallet";
import { TaprootAssets } from "./screens/TaprootAssets";
import { Lightning } from "./screens/Lightning";
import { Ark } from "./screens/Ark";
import { History } from "./screens/History";
import "./styles/theme.css";

type Screen = "welcome" | "create" | "import" | "unlock" | "dashboard" | "backup" | "taproot" | "lightning" | "ark" | "history";

// Bech32 charset used by BOLT11 invoices (excludes 1, b, i, o).
const BOLT11_RE = /^lnbc[0-9munp]*1[ac-hj-np-z02-9]{6,}$/i;

/** Basic structural validation of a BOLT11 mainnet invoice string. */
function isPlausibleInvoice(s: string): boolean {
  const v = s.trim().toLowerCase();
  // Length bounds guard against truncated/garbage deep-links; full amount and
  // expiry validation is done on the backend via `decode_lightning_invoice`.
  if (v.length < 20 || v.length > 2000) return false;
  return BOLT11_RE.test(v);
}

function extractLightningInvoice(url: string): string | null {
  let candidate: string | null = null;
  try {
    const parsed = new URL(url);
    if (parsed.protocol !== "lightning:" && !parsed.protocol.startsWith("lightning")) {
      return null;
    }
    // Handles both lightning:lnbc... and lightning://lnbc...
    candidate = parsed.pathname || parsed.host || "";
    if (!candidate.toLowerCase().startsWith("lnbc")) {
      candidate = url.replace(/^lightning:\/\//i, "").replace(/^lightning:/i, "");
    }
  } catch {
    // If URL parsing fails, try manual stripping.
    candidate = url.replace(/^lightning:\/\//i, "").replace(/^lightning:/i, "");
  }

  if (candidate && isPlausibleInvoice(candidate)) {
    return candidate.trim();
  }
  return null;
}

function AppContent() {
  const { t } = useI18n();
  const { notify } = useNotification();
  const [screen, setScreen] = useState<Screen>("welcome");
  const [checking, setChecking] = useState(true);
  const [pendingInvoice, setPendingInvoice] = useState<string | null>(null);
  const [resetModalOpen, setResetModalOpen] = useState(false);

  useEffect(() => {
    async function checkWallet() {
      try {
        const exists = await invoke<boolean>("wallet_exists");
        const initial = exists ? "unlock" : "welcome";
        setScreen(initial);
      } catch {
        setScreen("welcome");
      } finally {
        setChecking(false);
      }
    }
    checkWallet();

    // Check if the app was launched via a deep link.
    getCurrent()
      .then((urls) => {
        if (urls && urls.length > 0) {
          const invoice = extractLightningInvoice(urls[0]);
          if (invoice) setPendingInvoice(invoice);
        }
      })
      .catch((e) => console.error("deep-link getCurrent error:", e));

    // Listen for deep links while the app is running.
    let unlisten: (() => void) | undefined;
    onOpenUrl((urls) => {
      if (urls.length > 0) {
        const invoice = extractLightningInvoice(urls[0]);
        if (invoice) setPendingInvoice(invoice);
      }
    })
      .then((fn) => {
        unlisten = fn;
      })
      .catch((e) => console.error("deep-link onOpenUrl error:", e));

    return () => {
      if (unlisten) unlisten();
    };
  }, []);

  // Route to Lightning when a pending invoice is available and the wallet is unlocked.
  useEffect(() => {
    if (pendingInvoice && screen === "dashboard") {
      setScreen("lightning");
    }
  }, [pendingInvoice, screen]);

  function handleReset() {
    setResetModalOpen(true);
  }

  async function confirmReset() {
    setResetModalOpen(false);
    try {
      await invoke("delete_wallet_command");
      setPendingInvoice(null);
      setScreen("welcome");
    } catch (e) {
      notify(String(e), "error");
    }
  }

  if (checking) {
    return (
      <div className="app-container" style={{ display: "flex", alignItems: "center", justifyContent: "center" }}>
        <div className="spinner" style={{ width: "32px", height: "32px" }} />
      </div>
    );
  }

  return (
    <div className="app-container">
      <div className="scanline" />
      <NotificationContainer />
      <UpdateBanner />
      <ConfirmModal
        open={resetModalOpen}
        title={t("reset.title")}
        message={t("reset.message")}
        confirmText={t("reset.confirm")}
        confirmVariant="danger"
        onConfirm={confirmReset}
        onCancel={() => setResetModalOpen(false)}
      />
      <AnimatePresence mode="wait">
        <motion.div
          key={screen}
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.25 }}
          style={{ width: "100%", height: "100%" }}
        >
          {screen === "welcome" && (
            <Welcome
              onCreate={() => setScreen("create")}
              onImport={() => setScreen("import")}
            />
          )}
          {screen === "create" && (
            <CreateWallet
              onBack={() => setScreen("welcome")}
              onCreated={() => setScreen("dashboard")}
            />
          )}
          {screen === "import" && (
            <ImportWallet
              onBack={() => setScreen("welcome")}
              onImported={() => setScreen("dashboard")}
            />
          )}
          {screen === "unlock" && (
            <UnlockWallet
              onUnlocked={() => setScreen("dashboard")}
              onReset={handleReset}
            />
          )}
          {screen === "dashboard" && (
            <Dashboard
              onLogout={() => setScreen("unlock")}
              onBackup={() => setScreen("backup")}
              onTaproot={() => setScreen("taproot")}
              onLightning={() => setScreen("lightning")}
              onArk={() => setScreen("ark")}
              onHistory={() => setScreen("history")}
            />
          )}
          {screen === "history" && (
            <History onBack={() => setScreen("dashboard")} />
          )}
          {screen === "taproot" && (
            <TaprootAssets onBack={() => setScreen("dashboard")} />
          )}
          {screen === "lightning" && (
            <Lightning
              initialInvoice={pendingInvoice ?? undefined}
              onBack={() => {
                setPendingInvoice(null);
                setScreen("dashboard");
              }}
            />
          )}
          {screen === "ark" && (
            <Ark onBack={() => setScreen("dashboard")} />
          )}
          {screen === "backup" && (
            <BackupWallet onBack={() => setScreen("dashboard")} />
          )}
        </motion.div>
      </AnimatePresence>
    </div>
  );
}

function App() {
  return (
    <I18nProvider>
      <NotificationProvider>
        <AppContent />
      </NotificationProvider>
    </I18nProvider>
  );
}

export default App;
