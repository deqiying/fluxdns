import type { components } from "./generated-v2";
import { ApiError } from "./errors";
import { apiV2Request, onAuthSessionChange, reportUnauthorized } from "./client";

type Schemas = components["schemas"];
export type ServiceMetrics = Schemas["ServiceMetrics"];
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

class ManagementEventClient {
  private socket: WebSocket | undefined;
  private ready = false;
  private attempt = 0;
  private sequence = 0;
  private generation = 0;
  private reconnectTimer: number | undefined;
  private readonly metrics = new Map<string, MetricsSubscription>();

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
      if (this.metrics.size === 0) this.closeIdle();
    };
  }

  private async connect(): Promise<void> {
    if (this.socket || this.metrics.size === 0) return;
    const generation = ++this.generation;
    this.publishState(this.attempt > 0 ? "reconnecting" : "connecting");
    try {
      const ticket = await apiV2Request<Schemas["WebSocketTicket"]>("/events/ticket", {
        method: "POST",
      });
      if (generation !== this.generation || this.metrics.size === 0) return;
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
      return;
    }
    if (message.type === "ping" && typeof message.nonce === "string") {
      this.send({ type: "pong", nonce: message.nonce });
      return;
    }
    if (message.type === "metrics" && typeof message.subscription_id === "string" && isRecord(message.data)) {
      this.metrics.get(message.subscription_id)?.onData(message.data as ServiceMetrics);
    }
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
    if (this.metrics.size === 0 || event.code === 1000) {
      this.publishState("closed");
      return;
    }
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.metrics.size === 0 || this.reconnectTimer !== undefined) return;
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

export const managementEvents = new ManagementEventClient();
