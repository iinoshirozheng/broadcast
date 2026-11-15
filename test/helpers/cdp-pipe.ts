import type { ChildProcess } from "node:child_process";
import type { Readable, Writable } from "node:stream";
import type { ProtocolMapping } from "devtools-protocol/types/protocol-mapping.js";

type Commands = ProtocolMapping.Commands;
type Events = ProtocolMapping.Events;
type Params<M extends keyof Commands> = Commands[M]["paramsType"][0];
type Result<M extends keyof Commands> = Commands[M]["returnType"];
type EventParams<E extends keyof Events> = Events[E][0];
type Waiter = { resolve: (params: unknown) => void; reject: (error: Error) => void };
type Handler = (params: unknown) => unknown;
type Pending = Waiter & { timeout: NodeJS.Timeout };

class CdpEventChannel {
  eventWaiters = new Map<string, Waiter[]>();
  eventHandlers = new Map<string, Handler[]>();
  closed = false;

  dispatch(method: string, params: unknown) {
    const waiters = this.eventWaiters.get(method) || [];
    this.eventWaiters.delete(method);
    waiters.forEach((waiter) => waiter.resolve(params));
    const handlers = this.eventHandlers.get(method) || [];
    handlers.forEach((handler) => {
      try {
        Promise.resolve(handler(params)).catch((error: Error) => {
          console.error(`CDP ${method} handler failed: ${error.message}`);
        });
      } catch (error) {
        console.error(`CDP ${method} handler failed: ${(error as Error).message}`);
      }
    });
  }

  waitFor<E extends keyof Events>(method: E, timeoutMs: number): Promise<EventParams<E>> {
    return new Promise((resolve, reject) => {
      const waiters = this.eventWaiters.get(method) || [];
      const timeout = setTimeout(() => {
        this.eventWaiters.set(
          method,
          (this.eventWaiters.get(method) || []).filter(
            (waiter) => waiter.resolve !== wrappedResolve,
          ),
        );
        reject(new Error(`Timed out waiting for CDP event ${method}`));
      }, timeoutMs);
      const wrappedResolve = (value: unknown) => {
        clearTimeout(timeout);
        resolve(value as EventParams<E>);
      };
      waiters.push({ resolve: wrappedResolve, reject });
      this.eventWaiters.set(method, waiters);
    });
  }

  on<E extends keyof Events>(method: E, handler: (params: EventParams<E>) => unknown) {
    const handlers = this.eventHandlers.get(method) || [];
    handlers.push(handler as Handler);
    this.eventHandlers.set(method, handlers);
    return () => {
      this.eventHandlers.set(
        method,
        (this.eventHandlers.get(method) || []).filter((candidate) => candidate !== handler),
      );
    };
  }

  markClosed(error: Error) {
    if (this.closed) return;
    this.closed = true;
    this.eventWaiters.forEach((waiters) => {
      waiters.forEach((waiter) => waiter.reject(error));
    });
    this.eventWaiters.clear();
    this.eventHandlers.clear();
  }
}

export class CdpPipeSession extends CdpEventChannel {
  browser: CdpPipeBrowser;
  sessionId: string;

  constructor(browser: CdpPipeBrowser, sessionId: string) {
    super();
    this.browser = browser;
    this.sessionId = sessionId;
  }

  send<M extends keyof Commands>(method: M, params?: Params<M>): Promise<Result<M>> {
    if (method === "Target.getTargets") return this.browser.send(method, params);
    if (this.closed) return Promise.reject(new Error("CDP session closed"));
    return this.browser.send(method, params, this.sessionId);
  }

  close() {
    if (this.closed) return;
    this.browser.detach(this.sessionId);
  }
}

export class CdpPipeBrowser extends CdpEventChannel {
  child: ChildProcess;
  // Chrome's --remote-debugging-pipe reads commands on fd 3 and writes replies on fd 4.
  input: Writable;
  output: Readable;
  sequence = 0;
  pending = new Map<number, Pending>();
  sessions = new Map<string, CdpPipeSession>();
  buffer = Buffer.alloc(0);

  constructor(child: ChildProcess) {
    super();
    this.child = child;
    this.input = child.stdio[3] as Writable;
    this.output = child.stdio[4] as Readable;

    this.output.on("data", (chunk: Buffer) => this.receive(chunk));
    this.output.once("error", (error) => this.fail(error));
    this.output.once("end", () => this.fail(new Error("CDP pipe ended")));
    this.output.once("close", () => this.fail(new Error("CDP pipe closed")));
    this.input.once("error", (error) => this.fail(error));
    child.once("exit", (code, signal) => {
      this.fail(new Error(`Codex exited (${signal || code})`));
    });
  }

  async open() {
    await this.send("Browser.getVersion");
    await this.send("Target.setDiscoverTargets", { discover: true });
  }

  receive(chunk: Buffer) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    for (
      let boundary = this.buffer.indexOf(0);
      boundary !== -1;
      boundary = this.buffer.indexOf(0)
    ) {
      const source = this.buffer.subarray(0, boundary).toString("utf8");
      this.buffer = this.buffer.subarray(boundary + 1);
      if (!source) continue;
      const message = JSON.parse(source);
      if (message.id) {
        const pending = this.pending.get(message.id);
        if (!pending) continue;
        clearTimeout(pending.timeout);
        this.pending.delete(message.id);
        if (message.error) pending.reject(new Error(message.error.message));
        else pending.resolve(message.result);
      } else if (message.sessionId) {
        this.sessions.get(message.sessionId)?.dispatch(message.method, message.params);
      } else {
        if (message.method === "Target.detachedFromTarget") {
          this.closeSession(message.params.sessionId, new Error("CDP target detached"));
        }
        this.dispatch(message.method, message.params);
      }
    }
  }

  send<M extends keyof Commands>(
    method: M,
    params: Params<M> = {} as Params<M>,
    sessionId?: string,
  ): Promise<Result<M>> {
    if (this.closed) return Promise.reject(new Error("CDP pipe closed"));
    const id = ++this.sequence;
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Timed out waiting for CDP command ${method}`));
      }, 30_000);
      this.pending.set(id, { resolve: resolve as (result: unknown) => void, reject, timeout });
      const message = sessionId ? { id, method, params, sessionId } : { id, method, params };
      this.input.write(`${JSON.stringify(message)}\0`, (error) => {
        if (error) this.fail(error);
      });
    });
  }

  async targets() {
    const { targetInfos } = await this.send("Target.getTargets");
    return targetInfos;
  }

  async connect(targetId: string) {
    const { sessionId } = await this.send("Target.attachToTarget", {
      targetId,
      flatten: true,
    });
    const session = new CdpPipeSession(this, sessionId);
    this.sessions.set(sessionId, session);
    return session;
  }

  detach(sessionId: string) {
    this.closeSession(sessionId, new Error("CDP session closed"));
    if (!this.closed) {
      this.send("Target.detachFromTarget", { sessionId }).catch(() => {});
    }
  }

  closeSession(sessionId: string, error: Error) {
    const session = this.sessions.get(sessionId);
    if (!session) return;
    this.sessions.delete(sessionId);
    session.markClosed(error);
  }

  fail(error: Error) {
    if (this.closed) return;
    this.pending.forEach((pending) => {
      clearTimeout(pending.timeout);
      pending.reject(error);
    });
    this.pending.clear();
    this.sessions.forEach((session) => session.markClosed(error));
    this.sessions.clear();
    this.markClosed(error);
  }

  close() {
    if (this.closed) return;
    this.input.destroy();
    this.output.destroy();
    this.fail(new Error("CDP pipe closed"));
  }
}
