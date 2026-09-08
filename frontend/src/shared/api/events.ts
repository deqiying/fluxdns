import type { components } from "./generated-v2";
import { ApiError } from "./errors";
import { apiV2Request, onAuthSessionChange, reportUnauthorized } from "./client";

type Schemas = components["schemas"];
export type ServiceMetrics = Schemas["ServiceMetrics"];
export type QueryFilter = Schemas["QueryFilter"];
export type QueryRecord = Schemas["QueryRecord"];
export type CommitCursor = Schemas["CommitCursor"];
export type ResyncReason = Extract<Schemas["ServerMessage"], { type: "resync_required" }>["reason"];
export type EventConnectionState = "connecting" | "open" | "reconnecting" | "closed" | "error";

const WS_PROTOCOL = "fluxdns.v1";
const WS_TICKET_PROTOCOL_PREFIX = "fluxdns.ticket.";
const BASE_RECONNECT_DELAY_MS = 500;
const MAX_RECONNECT_DELAY_MS = 10_000;

interface MetricsSubscription {
  id: string;
  onData: (metrics: ServiceMetrics) => void;
  onState: (state: EventConnectionState) => void;
}

export interface QueryBatch {
  cursor: CommitCursor;
  directoryRevision: string;
  items: QueryRecord[];
}

interface QuerySubscription {
  id: string;
  filter: QueryFilter;
  after: CommitCursor;
  retentionRevision: string;
  suspended: boolean;
  onData: (batch: QueryBatch) => void;
  onResync: (reason: ResyncReason) => void;
  onState: (state: EventConnectionState) => void;
}

class ManagementEventClient {
  private socket: WebSocket | undefined;
  private ready = false;
  private attempt = 0;
  private sequence = 0;
  private generation = 0;
  private reconnectTimer: number | undefined;
  private readonly metrics = new Map<string, MetricsSubscription>();
  private readonly queries = new Map<string, QuerySubscription>();

  constructor() {
    onAuthSessionChange(() => this.resetForAuthChange());
  }

  subscribeMetrics(
    onData: MetricsSubscription["onData"],
    onState: MetricsSubscription["onState"],
  ): () => void {
    const id = `metrics:${++this.sequence}`;
    this.metrics.set(id, { id, onData, onState });
    onState(this.ready ? "open" : this.attempt > 0 ? "reconnecting" : "connecting");
    if (this.ready) this.send({ type: "subscribe_metrics", subscription_id: id });
    else void this.connect();
    return () => {
      if (this.ready) this.send({ type: "unsubscribe", subscription_id: id });
      this.metrics.delete(id);
      if (!this.hasSubscriptions()) this.closeIdle();
    };
  }

  subscribeQueries(
    options: {
      filter: QueryFilter;
      after: CommitCursor;
      retentionRevision: string;
    },
    onData: QuerySubscription["onData"],
    onResync: QuerySubscription["onResync"],
    onState: QuerySubscription["onState"],
  ): () => void {
    const id = `queries:${++this.sequence}`;
    const subscription: QuerySubscription = {
      id,
      filter: options.filter,
      after: options.after,
      retentionRevision: options.retentionRevision,
      suspended: false,
      onData,
      onResync,
      onState,
    };
    this.queries.set(id, subscription);
    onState(this.ready ? "open" : this.attempt > 0 ? "reconnecting" : "connecting");
    if (this.ready) this.sendQuerySubscription(subscription);
    else void this.connect();
    return () => {
      if (this.ready && !subscription.suspended) this.send({ type: "unsubscribe", subscription_id: id });
      this.queries.delete(id);
      if (!this.hasSubscriptions()) this.closeIdle();
    };
  }

  private async connect(): Promise<void> {
    if (this.socket || !this.hasSubscriptions()) return;
    const generation = ++this.generation;
    this.publishState(this.attempt > 0 ? "reconnecting" : "connecting");
    try {
      const ticket = await apiV2Request<Schemas["WebSocketTicket"]>("/events/ticket", {
        method: "POST",
      });
      if (generation !== this.generation || !this.hasSubscriptions()) return;
      if (!/^[A-Za-z0-9_-]{43}$/.test(ticket.ticket) || ticket.expires_at_ms <= Date.now()) {
        throw new ApiError({
          code: "INVALID_RESPONSE",
          message: "invalid WebSocket ticket",
          kind: "invalid-response",
        });
      }
      const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
      const url = `${protocol}//${window.location.host}/api/v2/events`;
      const socket = new WebSocket(url, [WS_PROTOCOL, `${WS_TICKET_PROTOCOL_PREFIX}${ticket.ticket}`]);
      this.socket = socket;
      socket.addEventListener("open", () => {
        if (generation !== this.generation || socket.protocol !== WS_PROTOCOL) {
          socket.close(1008, "protocol mismatch");
        }
      });
      socket.addEventListener("message", (event) => this.receive(event));
      socket.addEventListener("close", (event) => this.closed(socket, event));
      socket.addEventListener("error", () => {
        if (this.socket === socket) this.publishState("error");
      });
    } catch (error) {
      if (generation !== this.generation) return;
      if (error instanceof ApiError && error.status === 401) {
        this.publishState("error");
        return;
      }
      this.scheduleReconnect();
    }
  }

  private receive(event: MessageEvent): void {
    if (typeof event.data !== "string") {
      this.socket?.close(1003, "text messages required");
      return;
    }
    let message: unknown;
    try {
      message = JSON.parse(event.data);
    } catch {
      this.socket?.close(1008, "invalid server message");
      return;
    }
    if (!isRecord(message) || typeof message.type !== "string") {
      this.socket?.close(1008, "invalid server message");
      return;
    }
    if (message.type === "ready") {
      if (message.protocol_version !== 1 || typeof message.epoch !== "string") {
        this.socket?.close(1008, "protocol mismatch");
        return;
      }
      this.ready = true;
      this.attempt = 0;
      this.publishState("open");
      for (const subscription of this.metrics.values()) {
        this.send({ type: "subscribe_metrics", subscription_id: subscription.id });
      }
      for (const subscription of this.queries.values()) {
        if (!subscription.suspended) this.sendQuerySubscription(subscription);
      }
      return;
    }
    if (message.type === "ping" && typeof message.nonce === "string") {
      this.send({ type: "pong", nonce: message.nonce });
      return;
    }
    if (message.type === "metrics" && typeof message.subscription_id === "string" && isRecord(message.data)) {
      this.metrics.get(message.subscription_id)?.onData(message.data as ServiceMetrics);
      return;
    }
    if (message.type === "queries" && typeof message.subscription_id === "string"
        && isCommitCursor(message.cursor) && typeof message.directory_revision === "string"
        && Array.isArray(message.items)) {
      const subscription = this.queries.get(message.subscription_id);
      if (!subscription || subscription.suspended) return;
      subscription.after = message.cursor;
      subscription.onData({
        cursor: message.cursor,
        directoryRevision: message.directory_revision,
        items: message.items as QueryRecord[],
      });
      return;
    }
    if (message.type === "resync_required" && typeof message.subscription_id === "string"
        && isResyncReason(message.reason)) {
      const subscription = this.queries.get(message.subscription_id);
      if (!subscription) return;
      subscription.suspended = true;
      subscription.onResync(message.reason);
    }
  }

  private sendQuerySubscription(subscription: QuerySubscription): void {
    this.send({
      type: "subscribe_queries",
      subscription_id: subscription.id,
      filter: subscription.filter,
      after: subscription.after,
      retention_revision: subscription.retentionRevision,
    });
  }

  private send(message: Schemas["ClientMessage"]): void {
    if (!this.ready || this.socket?.readyState !== WebSocket.OPEN) return;
    this.socket.send(JSON.stringify(message));
  }

  private closed(socket: WebSocket, event: CloseEvent): void {
    if (this.socket !== socket) return;
    this.socket = undefined;
    this.ready = false;
    if (event.code === 4401) {
      this.publishState("error");
      reportUnauthorized();
      return;
    }
    if (!this.hasSubscriptions() || event.code === 1000) {
      this.publishState("closed");
      return;
    }
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (!this.hasSubscriptions() || this.reconnectTimer !== undefined) return;
    this.attempt += 1;
    this.publishState("reconnecting");
    const delay = Math.min(MAX_RECONNECT_DELAY_MS, BASE_RECONNECT_DELAY_MS * 2 ** (this.attempt - 1));
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = undefined;
      void this.connect();
    }, delay);
  }

  private publishState(state: EventConnectionState): void {
    for (const subscription of this.metrics.values()) subscription.onState(state);
    for (const subscription of this.queries.values()) subscription.onState(state);
  }

  private hasSubscriptions(): boolean {
    return this.metrics.size > 0 || this.queries.size > 0;
  }

  private closeIdle(): void {
    this.generation += 1;
    if (this.reconnectTimer !== undefined) window.clearTimeout(this.reconnectTimer);
    this.reconnectTimer = undefined;
    this.attempt = 0;
    this.ready = false;
    this.socket?.close(1000, "no active subscriptions");
    this.socket = undefined;
  }

  private resetForAuthChange(): void {
    this.generation += 1;
    if (this.reconnectTimer !== undefined) window.clearTimeout(this.reconnectTimer);
    this.reconnectTimer = undefined;
    this.attempt = 0;
    this.ready = false;
    this.socket?.close(1000, "authentication changed");
    this.socket = undefined;
    this.publishState("closed");
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isCommitCursor(value: unknown): value is CommitCursor {
  return isRecord(value) && typeof value.epoch === "string" && typeof value.sequence === "string";
}

function isResyncReason(value: unknown): value is ResyncReason {
  return value === "epoch_changed" || value === "cursor_expired" || value === "buffer_overflow"
    || value === "observation_gap" || value === "retention_changed";
}

export const managementEvents = new ManagementEventClient();
