import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  createRevisionPoller,
  createRevisionWebSocketClient,
  getRevisionPollingInterval,
  getRevisionWebSocketConfig,
  type RevisionSocket,
} from "../src/revisionPolling.ts";

type RevisionResult = { changed: boolean; revision: number };
type FakeTimer = { callback: () => unknown; delay: number };

class FakeSocket implements RevisionSocket {
  onopen: ((event: Event) => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  onclose: ((event: CloseEvent) => void) | null = null;
  closeCalls: { code?: number; reason?: string }[] = [];
  url: string;

  constructor(url = "") {
    this.url = url;
  }

  close(code?: number, reason?: string) {
    this.closeCalls.push({ code, reason });
  }

  emitOpen() {
    this.onopen?.(new Event("open"));
  }

  emitMessage(data: string) {
    this.onmessage?.(new MessageEvent("message", { data }));
  }

  emitClose() {
    this.onclose?.(new CloseEvent("close"));
  }
}

function createTimeoutHarness() {
  const timeouts: FakeTimer[] = [];
  return {
    timeouts,
    setTimeout(callback: () => unknown, delay: number) {
      const timer = { callback, delay };
      timeouts.push(timer);
      return timer;
    },
    clearTimeout() {},
  };
}

function createTimerHarness() {
  const intervals: FakeTimer[] = [];
  const cleared: unknown[] = [];

  return {
    setInterval(callback: () => unknown, delay: number) {
      const timer = { callback, delay };
      intervals.push(timer);
      return timer;
    },
    clearInterval(timer: unknown) {
      cleared.push(timer);
    },
    fire(index = 0) {
      return intervals[index].callback();
    },
    intervals,
    cleared,
  };
}

async function flush() {
  await Promise.resolve();
  await Promise.resolve();
}

async function runFirstPoll(
  poller: { start(): void },
  timer: ReturnType<typeof createTimerHarness>,
  calls: number[],
) {
  poller.start();
  await flush();
  if (calls.length === 0) {
    await timer.fire();
    await flush();
  }
}

test("polls the current revision every 2000ms by default and ignores unchanged responses", async () => {
  const timer = createTimerHarness();
  const calls: number[] = [];
  let invalidations = 0;
  const poller = createRevisionPoller({
    fetchRevision: async (since) => {
      calls.push(since);
      return { changed: false, revision: 0 };
    },
    onInvalidate: () => {
      invalidations += 1;
    },
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });

  await runFirstPoll(poller, timer, calls);

  assert.equal(timer.intervals.length, 1);
  assert.equal(timer.intervals[0].delay, 2000);
  assert.deepEqual(calls, [0]);
  assert.equal(invalidations, 0);
});

test("advances the revision on change and invalidates once", async () => {
  const timer = createTimerHarness();
  const calls: number[] = [];
  let invalidations = 0;
  const responses = [
    { changed: false, revision: 0 },
    { changed: true, revision: 7 },
    { changed: false, revision: 7 },
  ];
  const poller = createRevisionPoller({
    fetchRevision: async (since) => {
      calls.push(since);
      const response = responses.shift();
      assert.ok(response);
      return response;
    },
    onInvalidate: () => {
      invalidations += 1;
    },
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });

  await runFirstPoll(poller, timer, calls);
  await timer.fire();
  await flush();
  await timer.fire();
  await flush();

  assert.deepEqual(calls, [0, 0, 7]);
  assert.equal(invalidations, 1);
});

test("does not start another request while a revision request is pending", async () => {
  const timer = createTimerHarness();
  const calls: number[] = [];
  let invalidations = 0;
  let resolveRequest: (result: RevisionResult) => void = () => {};
  const request = new Promise<RevisionResult>((resolve) => {
    resolveRequest = resolve;
  });
  const poller = createRevisionPoller({
    fetchRevision: (since) => {
      calls.push(since);
      return request;
    },
    onInvalidate: () => {
      invalidations += 1;
    },
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });

  poller.start();
  await flush();
  if (calls.length === 0) timer.fire();
  await flush();
  timer.fire();
  timer.fire();
  await flush();

  assert.deepEqual(calls, [0]);

  resolveRequest({ changed: true, revision: 9 });
  await flush();
  await timer.fire();
  await flush();

  assert.deepEqual(calls, [0, 9]);
  assert.equal(invalidations, 1);
});

test("keeps the old revision after an error and retries it on the next tick", async () => {
  const timer = createTimerHarness();
  const calls: number[] = [];
  let attempt = 0;
  let invalidations = 0;
  const poller = createRevisionPoller({
    fetchRevision: async (since) => {
      calls.push(since);
      attempt += 1;
      if (attempt === 1) throw new Error("temporarily unavailable");
      return { changed: true, revision: 4 };
    },
    onInvalidate: () => {
      invalidations += 1;
    },
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });

  await runFirstPoll(poller, timer, calls);
  await timer.fire();
  await flush();

  assert.deepEqual(calls, [0, 0]);
  assert.equal(invalidations, 1);
});

test("uses an explicit interval and clears its timer when stopped", async () => {
  const timer = createTimerHarness();
  const poller = createRevisionPoller({
    fetchRevision: async () => ({ changed: false, revision: 0 }),
    onInvalidate: () => {},
    intervalMs: 750,
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });

  poller.start();
  poller.stop();

  assert.equal(timer.intervals.length, 1);
  assert.equal(timer.intervals[0].delay, 750);
  assert.deepEqual(timer.cleared, [timer.intervals[0]]);
});

test("does not invalidate after being stopped while a request is pending", async () => {
  const timer = createTimerHarness();
  let invalidations = 0;
  let resolveRequest: (result: RevisionResult) => void = () => {};
  const poller = createRevisionPoller({
    fetchRevision: () =>
      new Promise<RevisionResult>((resolve) => {
        resolveRequest = resolve;
      }),
    onInvalidate: () => {
      invalidations += 1;
    },
    setInterval: timer.setInterval,
    clearInterval: timer.clearInterval,
  });

  poller.start();
  await flush();
  poller.stop();
  resolveRequest({ changed: true, revision: 1 });
  await flush();

  assert.equal(invalidations, 0);
});

test("cloud metadata selects polling and local metadata does not", () => {
  assert.equal(
    getRevisionPollingInterval({
      mode: "cloud",
      realtime: { transport: "poll", intervalMs: 2000 },
    }),
    2000,
  );
  assert.equal(
    getRevisionPollingInterval({
      mode: "local",
      realtime: { transport: "poll", intervalMs: 2000 },
    }),
    null,
  );
  assert.equal(getRevisionPollingInterval({ mode: "cloud" }), null);
});

test("cloud metadata selects WebSocket push without periodic polling", () => {
  assert.deepEqual(
    getRevisionWebSocketConfig({
      mode: "cloud",
      realtime: {
        transport: "websocket",
        endpoint: "/api/events",
      },
    }),
    {
      endpoint: "/api/events",
    },
  );
  assert.equal(
    getRevisionWebSocketConfig({
      mode: "local",
      realtime: {
        transport: "websocket",
        endpoint: "/api/events",
      },
    }),
    null,
  );
});

test("WebSocket push invalidates on changes and checks revision once per connection", async () => {
  const sockets: FakeSocket[] = [];
  const { timeouts, ...timers } = createTimeoutHarness();
  const states: string[] = [];
  const revisionCalls: number[] = [];
  let invalidations = 0;

  const client = createRevisionWebSocketClient({
    url: "wss://broadcast.example.test/api/events",
    fetchRevision: async (since) => {
      revisionCalls.push(since);
      return { changed: false, revision: since };
    },
    onInvalidate: () => {
      invalidations += 1;
    },
    onConnectionChange: (state) => states.push(state),
    createWebSocket: (url) => {
      const socket = new FakeSocket(url);
      sockets.push(socket);
      return socket;
    },
    ...timers,
  });

  client.start();
  assert.equal(sockets.length, 1);
  assert.equal(sockets[0].url, "wss://broadcast.example.test/api/events");
  assert.deepEqual(states, ["connecting"]);

  sockets[0].emitOpen();
  await flush();
  assert.deepEqual(states, ["connecting", "live"]);
  assert.deepEqual(revisionCalls, [0]);

  sockets[0].emitMessage(JSON.stringify({ type: "revision", revision: 4 }));
  assert.equal(invalidations, 1);
  sockets[0].emitMessage(JSON.stringify({ type: "revision", revision: 4 }));
  sockets[0].emitMessage("malformed");
  assert.equal(invalidations, 1);

  sockets[0].emitClose();
  await flush();
  assert.equal(states.at(-1), "reconnecting");
  assert.deepEqual(revisionCalls, [0]);
  assert.equal(timeouts[0].delay, 1_000);

  timeouts[0].callback();
  assert.equal(sockets.length, 2);
  sockets[1].emitOpen();
  await flush();
  assert.equal(states.at(-1), "live");
  assert.deepEqual(revisionCalls, [0, 4]);

  client.stop();
  assert.deepEqual(sockets[1].closeCalls, [{ code: 1000, reason: "Client stopped" }]);
});

test("a failed reconnect revision check closes the socket so compensation retries", async () => {
  const sockets: FakeSocket[] = [];
  const { timeouts, ...timers } = createTimeoutHarness();
  let revisionRequest = 0;

  const client = createRevisionWebSocketClient({
    url: "wss://broadcast.example.test/api/events",
    fetchRevision: async () => {
      revisionRequest += 1;
      if (revisionRequest === 1) throw new Error("temporary revision failure");
      return { changed: true, revision: 7 };
    },
    onInvalidate() {},
    createWebSocket() {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
    ...timers,
  });

  client.start();
  sockets[0].emitOpen();
  await flush();
  assert.deepEqual(sockets[0].closeCalls, [{ code: 4000, reason: "Revision check failed" }]);

  sockets[0].emitClose();
  assert.equal(timeouts[0].delay, 1_000);
  timeouts[0].callback();
  sockets[1].emitOpen();
  await flush();
  assert.equal(revisionRequest, 2);
  client.stop();
});

test("the app connects the selected realtime transport without reloading the page", async () => {
  const [appSource, apiSource] = await Promise.all([
    readFile(new URL("../src/App.tsx", import.meta.url), "utf8"),
    readFile(new URL("../src/api.ts", import.meta.url), "utf8"),
  ]);

  assert.match(apiSource, /\/api\/revisions\?\$\{query\}/);
  assert.match(appSource, /getRevisionPollingInterval\(broadcastMetadata\)/);
  assert.match(appSource, /createRevisionPoller\(\{/);
  assert.match(appSource, /getRevisionWebSocketConfig\(broadcastMetadata\)/);
  assert.match(appSource, /createRevisionWebSocketClient\(\{/);
  assert.doesNotMatch(appSource, /revisionFallbackInterval|fallbackIntervalMs/);
  assert.match(appSource, /controller\.abort\(\);\s*poller\.stop\(\)/);
  assert.match(
    appSource,
    /const eventsUrl = resolveBroadcastUrl\("\/api\/events"\);[\s\S]*new EventSource\(eventsUrl\)/,
  );
  assert.doesNotMatch(appSource, /location\.reload\(/);

  const pollingEffect = appSource.slice(
    appSource.indexOf("if (revisionPollingInterval === null) return;"),
    appSource.indexOf("function pushUndo"),
  );
  assert.doesNotMatch(
    pollingEffect,
    /\bdetailTaskId\b|\bbroadcastMetadata\b|\bselectedProjectId\b/,
  );
  assert.match(appSource, /const projectId = taskScopeProjectIdRef\.current/);
});
