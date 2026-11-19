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

export function readComposerReferenceId(referenceKey: string): string | null {
  try {
    return decodeComposerReferenceKey(referenceKey);
  } catch {
    return null;
  }
}
