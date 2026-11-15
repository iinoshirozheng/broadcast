export const COMPOSER_REFERENCE_FORMAT = "broadcast.composer-reference.v1";
const REFERENCE_PREFIX = "broadcast://composer-reference/v1";
type ReferenceKind = "skill" | "agent";

function requiredString(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new TypeError(`${name} must be a non-empty string`);
  }
  return value;
}

export function encodeComposerReferenceKey(stableId: string): string {
  let binary = "";
  for (const byte of new TextEncoder().encode(requiredString(stableId, "stableId"))) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function decodeComposerReferenceKey(referenceKey: string): string {
  const key = requiredString(referenceKey, "referenceKey");
  if (!/^[A-Za-z0-9_-]+$/.test(key)) {
    throw new TypeError("referenceKey must be unpadded base64url");
  }
  let decoded: string;
  try {
    const padded = `${key.replace(/-/g, "+").replace(/_/g, "/")}${"=".repeat((4 - (key.length % 4)) % 4)}`;
    const bytes = Uint8Array.from(atob(padded), (character) => character.charCodeAt(0));
    decoded = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(bytes);
  } catch {
    throw new TypeError("referenceKey is not canonical base64url UTF-8");
  }
  if (!decoded || encodeComposerReferenceKey(decoded) !== key) {
    throw new TypeError("referenceKey is not canonical base64url UTF-8");
  }
  return decoded;
}

export function readComposerReferenceId(referenceKey: string, kind?: string): string | null {
  try {
    const stableId = decodeComposerReferenceKey(referenceKey);
    return kind === "skill" && stableId !== stableId.normalize("NFC") ? null : stableId;
  } catch {
    return null;
  }
}

function assertReferenceKind(kind: string): ReferenceKind {
  if (kind !== "skill" && kind !== "agent") {
    throw new TypeError("kind must be 'skill' or 'agent'");
  }
  return kind;
}

function escapedMarkdownLabel(label: string): string {
  return requiredString(label, "label").replace(/[\\[\]]/g, "\\$&");
}

export function composerReferenceUri(kind: ReferenceKind, stableId: string): string {
  return `${REFERENCE_PREFIX}/${assertReferenceKind(kind)}/${encodeComposerReferenceKey(stableId)}`;
}

export function composerReferencePersistence(
  kind: ReferenceKind,
  stableId: string,
  label: string,
): {
  format: typeof COMPOSER_REFERENCE_FORMAT;
  kind: ReferenceKind;
  referenceKey: string;
  markdown: string;
} {
  const normalizedStableId =
    kind === "skill"
      ? requiredString(stableId, "stableId").normalize("NFC")
      : requiredString(stableId, "stableId");
  const referenceKey = encodeComposerReferenceKey(normalizedStableId);
  const uri = `${REFERENCE_PREFIX}/${assertReferenceKind(kind)}/${referenceKey}`;
  return {
    format: COMPOSER_REFERENCE_FORMAT,
    kind,
    referenceKey,
    markdown: `[${escapedMarkdownLabel(label)}](${uri})`,
  };
}

export function parseComposerReferenceUri(uri: string): {
  format: typeof COMPOSER_REFERENCE_FORMAT;
  kind: ReferenceKind;
  referenceKey: string;
  stableId: string;
} {
  const value = requiredString(uri, "uri");
  const match = /^broadcast:\/\/composer-reference\/v1\/([^/]+)\/([^/]+)$/.exec(value);
  if (!match) throw new TypeError("uri is not a composer reference v1 URI");
  const kind = assertReferenceKind(match[1]);
  const stableId = decodeComposerReferenceKey(match[2]);
  if (kind === "skill" && stableId !== stableId.normalize("NFC")) {
    throw new TypeError("skill reference identity must use NFC normalization");
  }
  return { format: COMPOSER_REFERENCE_FORMAT, kind, referenceKey: match[2], stableId };
}
