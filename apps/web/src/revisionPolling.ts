import type { BroadcastMetadata } from "./types.ts";

type RevisionResult = { changed: boolean; revision: number };
type ConnectionState = "connecting" | "live" | "reconnecting";

// The timers and the socket are injectable so the lifecycle can be driven by tests.
export interface RevisionSocket {
  onopen: ((event: Event) => void) | null;
  onmessage: ((event: MessageEvent) => void) | null;
  onerror: ((event: Event) => void) | null;
  onclose: ((event: CloseEvent) => void) | null;
  close(code?: number, reason?: string): void;
}

interface RevisionPollerOptions {
  fetchRevision: (since: number) => Promise<RevisionResult>;
  onInvalidate: () => void;
  intervalMs?: number;
  setInterval?(callback: () => void, delay: number): unknown;
  clearInterval?(timer: unknown): void;
}

interface RevisionWebSocketClientOptions {
  url: string;
  fetchRevision: (since: number) => Promise<RevisionResult>;
  onInvalidate: () => void;
  onConnectionChange?: (state: ConnectionState) => void;
  reconnectDelays?: number[];
  createWebSocket?: (url: string) => RevisionSocket;
  setTimeout?(callback: () => void, delay: number): unknown;
  clearTimeout?(timer: unknown): void;
}

export function getRevisionPollingInterval(metadata: BroadcastMetadata | null | undefined) {
  if (metadata?.mode !== "cloud" || metadata.realtime?.transport !== "poll") return null;
  return metadata.realtime.intervalMs;
}

export function getRevisionWebSocketConfig(metadata: BroadcastMetadata | null | undefined) {
  if (metadata?.mode !== "cloud" || metadata.realtime?.transport !== "websocket") return null;
  return {
    endpoint: metadata.realtime.endpoint,
  };
}

export function createRevisionPoller({
  fetchRevision,
  onInvalidate,
  intervalMs = 2000,
  setInterval: scheduleInterval = globalThis.setInterval,
  clearInterval: cancelInterval = globalThis.clearInterval,
}: RevisionPollerOptions) {
  let revision = 0;
  let timer: unknown;
  let running = false;
  let requestPending = false;

  async function poll() {
    if (!running || requestPending) return;
    requestPending = true;
    try {
      const result = await fetchRevision(revision);
      if (!running) return;
      const advanced = result.revision > revision;
      revision = Math.max(revision, result.revision);
      if (result.changed && advanced) onInvalidate();
    } catch {
      // Keep the current revision so the next interval retries the same boundary.
    } finally {
      requestPending = false;
    }
  }

  return {
    start() {
      if (running) return;
      running = true;
      timer = scheduleInterval(poll, intervalMs);
      void poll();
    },
    stop() {
      if (!running) return;
      running = false;
      cancelInterval(timer);
      timer = undefined;
    },
  };
}

export function createRevisionWebSocketClient({
  url,
  fetchRevision,
  onInvalidate,
  onConnectionChange = () => {},
  reconnectDelays = [1_000, 2_000, 5_000, 10_000, 30_000],
  createWebSocket = (target: string): RevisionSocket => new WebSocket(target),
  setTimeout: scheduleTimeout = globalThis.setTimeout,
  clearTimeout: cancelTimeout = globalThis.clearTimeout,
}: RevisionWebSocketClientOptions) {
  let revision = 0;
  let socket: RevisionSocket | undefined;
  let reconnectTimer: unknown;
  let running = false;
  let requestPending = false;
  let reconnectAttempt = 0;

  async function checkRevision() {
    if (!running || requestPending) return false;
    requestPending = true;
    try {
      const result = await fetchRevision(revision);
      if (!running) return false;
      const advanced = result.revision > revision;
      revision = Math.max(revision, result.revision);
      if (result.changed && advanced) onInvalidate();
      return true;
    } catch {
      return false;
    } finally {
      requestPending = false;
    }
  }

  function scheduleReconnect() {
    if (!running || reconnectTimer !== undefined) return;
    const delay = reconnectDelays[Math.min(reconnectAttempt, reconnectDelays.length - 1)];
    reconnectAttempt += 1;
    reconnectTimer = scheduleTimeout(() => {
      reconnectTimer = undefined;
      connect();
    }, delay);
  }

  function connect(): void {
    if (!running || socket) return;
    onConnectionChange(reconnectAttempt === 0 ? "connecting" : "reconnecting");
    try {
      socket = createWebSocket(url);
    } catch {
      socket = undefined;
      onConnectionChange("reconnecting");
      scheduleReconnect();
      return;
    }
    const currentSocket = socket;
    currentSocket.onopen = () => {
      if (!running || socket !== currentSocket) return;
      void checkRevision().then((succeeded) => {
        if (!running || socket !== currentSocket) return;
        if (!succeeded) {
          currentSocket.close(4000, "Revision check failed");
          return;
        }
        reconnectAttempt = 0;
        onConnectionChange("live");
      });
    };
    currentSocket.onmessage = (event) => {
      if (!running || socket !== currentSocket) return;
      try {
        const payload = JSON.parse(event.data);
        if (payload?.type !== "revision" || !Number.isSafeInteger(payload.revision)) return;
        if (payload.revision <= revision) return;
        revision = payload.revision;
        onInvalidate();
      } catch {
        // Ignore malformed messages and keep the connection alive.
      }
    };
    currentSocket.onerror = () => {
      if (running && socket === currentSocket) onConnectionChange("reconnecting");
    };
    currentSocket.onclose = () => {
      if (socket !== currentSocket) return;
      socket = undefined;
      if (!running) return;
      onConnectionChange("reconnecting");
      scheduleReconnect();
    };
  }

  return {
    start() {
      if (running) return;
      running = true;
      connect();
    },
    stop() {
      if (!running) return;
      running = false;
      if (reconnectTimer !== undefined) cancelTimeout(reconnectTimer);
      reconnectTimer = undefined;
      const currentSocket = socket;
      socket = undefined;
      currentSocket?.close(1000, "Client stopped");
    },
  };
}
