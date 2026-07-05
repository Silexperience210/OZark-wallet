import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Download, X } from "lucide-react";
import { useI18n } from "../i18n/I18nContext";

const REPO = "Silexperience210/OZark-wallet";
const APK_ASSET = "app-universal-release.apk";
const DISMISS_KEY = "ozark_update_dismissed";

/** Semver-ish compare: returns true if `latest` (e.g. "v0.3.6") is newer than `current`. */
function isNewer(latest: string, current: string): boolean {
  const a = latest.replace(/^v/, "").split(".").map((n) => parseInt(n, 10) || 0);
  const b = current.replace(/^v/, "").split(".").map((n) => parseInt(n, 10) || 0);
  for (let i = 0; i < Math.max(a.length, b.length); i++) {
    const x = a[i] || 0;
    const y = b[i] || 0;
    if (x > y) return true;
    if (x < y) return false;
  }
  return false;
}

/**
 * Non-intrusive in-app update check. On mount it polls the GitHub Releases API
 * (CSP already allows `connect-src https:`), and if a newer version's universal
 * APK is published it shows a dismissible banner. The button opens the APK
 * download URL in the system browser via the opener plugin; Android then offers
 * to install it. No native installer / extra permission required.
 */
export function UpdateBanner() {
  const { t } = useI18n();
  const [info, setInfo] = useState<{ version: string; url: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const current = await getVersion();
        const res = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
          headers: { Accept: "application/vnd.github+json" },
        });
        if (!res.ok) return;
        const data = await res.json();
        const tag: string = data?.tag_name ?? "";
        // Pick the APK robustly: prefer the universal name, else an arm64/aarch64
        // split, else any .apk — so this keeps working whether the release ships a
        // universal APK or per-ABI splits.
        const assets: { name?: string; browser_download_url?: string }[] = data?.assets ?? [];
        const isApk = (n?: string) => !!n && /\.apk$/i.test(n);
        const asset =
          assets.find((a) => a.name === APK_ASSET) ||
          assets.find((a) => isApk(a.name) && /arm64|aarch64/i.test(a.name || "")) ||
          assets.find((a) => isApk(a.name));
        if (!tag || !asset?.browser_download_url) return;
        if (!isNewer(tag, current)) return;
        if (localStorage.getItem(DISMISS_KEY) === tag) return;
        if (!cancelled) setInfo({ version: tag, url: asset.browser_download_url });
      } catch {
        // offline / rate-limited — stay silent
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  if (!info) return null;

  async function download() {
    if (!info) return;
    setBusy(true);
    setNote(null);
    try {
      await openUrl(info.url);
    } catch (e) {
      // openUrl can fail (permission/scope/no browser handler). Don't fail
      // silently: copy the link so the user can paste it, and surface why.
      try {
        await navigator.clipboard.writeText(info.url);
        setNote("Ouverture impossible — lien copié, colle-le dans ton navigateur.");
      } catch {
        setNote(`Ouverture impossible : ${String(e)}`);
      }
    } finally {
      setBusy(false);
    }
  }

  function dismiss() {
    if (info) localStorage.setItem(DISMISS_KEY, info.version);
    setInfo(null);
  }

  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        flexWrap: "wrap",
        gap: 10,
        padding: "10px 14px",
        margin: "8px 12px 0",
        borderRadius: 10,
        background: "rgba(0,240,255,0.08)",
        border: "1px solid rgba(0,240,255,0.28)",
        position: "relative",
        zIndex: 5,
      }}
    >
      <Download size={16} />
      <span style={{ flex: 1, fontSize: 13 }}>
        {t("update.available")} <b>{info.version}</b>
      </span>
      <button
        className="btn btn-primary"
        onClick={download}
        disabled={busy}
        style={{ padding: "5px 12px", fontSize: 13 }}
      >
        {busy ? <span className="spinner" /> : t("update.download")}
      </button>
      <button className="btn btn-ghost" onClick={dismiss} style={{ padding: 4 }} aria-label="dismiss">
        <X size={16} />
      </button>
      {note && (
        <div style={{ flexBasis: "100%", fontSize: 11, opacity: 0.85, wordBreak: "break-all" }}>
          {note} <span style={{ opacity: 0.7 }}>{info.url}</span>
        </div>
      )}
    </div>
  );
}
