declare global {
  // The embedding host sets this before the app loads.
  var __BROADCAST_FRAME_CAPABILITY__: unknown;
}

function frameCapability(): string {
  return typeof globalThis.__BROADCAST_FRAME_CAPABILITY__ === "string"
    ? globalThis.__BROADCAST_FRAME_CAPABILITY__
    : "";
}

let activeFrameChallenge = "";

export function setEmbeddedFrameChallenge(challenge: string): void {
  activeFrameChallenge = typeof challenge === "string" ? challenge : "";
}

export function postEmbeddedHostMessage(message: Record<string, unknown>): void {
  window.parent.postMessage(
    {
      ...message,
      capability: frameCapability(),
      challenge: activeFrameChallenge,
    },
    "*",
  );
}

export function installEmbeddedExternalLinkHandler(): () => void {
  const handleClick = (event: MouseEvent) => {
    const link =
      event.target instanceof Element ? event.target.closest('a[target="_blank"]') : null;
    if (!link) return;

    const rawHref = link.getAttribute("href");
    if (!rawHref) return;

    let url: URL;
    try {
      url = new URL(rawHref);
    } catch {
      return;
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") return;
    event.preventDefault();
    postEmbeddedHostMessage({
      type: "broadcast:open-external",
      payload: { url: url.href },
    });
  };

  document.addEventListener("click", handleClick, true);
  return () => document.removeEventListener("click", handleClick, true);
}
