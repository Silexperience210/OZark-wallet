// Classifies a pasted/scanned payment string into the rail that can handle it,
// so a single "paste anything" field can route the user to the right flow.

export type PaymentKind =
  | "lightning" // BOLT11 invoice
  | "lnurl" // LNURL (bech32 or lnurlp)
  | "lnaddress" // Lightning Address user@domain
  | "asset" // Taproot Assets address (tapbc1…)
  | "ark" // Ark address (ark1…/tark1…)
  | "onchain" // on-chain BTC address
  | "unknown";

export interface ClassifiedInput {
  kind: PaymentKind;
  value: string;
}

/** Best-effort classifier. Order matters: check specific prefixes before generic ones. */
export function classifyPaymentInput(raw: string): ClassifiedInput {
  // Strip common URI schemes (lightning:…, bitcoin:…) and surrounding whitespace.
  const value = raw.trim().replace(/^(lightning:|bitcoin:)/i, "").trim();
  const low = value.toLowerCase();

  if (/^ln(bc|tb|bcrt)[0-9]/.test(low)) return { kind: "lightning", value };
  if (low.startsWith("lnurl")) return { kind: "lnurl", value };
  // Lightning Address: an email-like handle (and not a BOLT11, already handled).
  if (/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(value)) return { kind: "lnaddress", value };
  if (/^(tapbc1|taptb1|taprt1|tapts1)/.test(low)) return { kind: "asset", value };
  if (/^(ark1|tark1)/.test(low)) return { kind: "ark", value };
  // On-chain: bech32 (bc1/tb1/bcrt1) or legacy base58 (1…/3…).
  if (/^(bc1|tb1|bcrt1)/.test(low)) return { kind: "onchain", value };
  if (/^[13][a-km-zA-HJ-NP-Z1-9]{25,34}$/.test(value)) return { kind: "onchain", value };

  return { kind: "unknown", value };
}
