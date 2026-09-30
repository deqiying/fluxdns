import "@testing-library/jest-dom/vitest";
import { afterAll, afterEach, beforeAll, vi } from "vitest";
import { cleanup } from "@testing-library/react";
import { resetMockState } from "@/mocks/handlers";
import { server } from "@/mocks/server";
import { clearAccessSession } from "@/shared/api/client";

function createMemoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() { return values.size; },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => values.delete(key),
    setItem: (key, value) => values.set(key, String(value)),
  };
}

Object.defineProperty(window, "localStorage", { configurable: true, value: createMemoryStorage() });
Object.defineProperty(window, "sessionStorage", { configurable: true, value: createMemoryStorage() });

class MockResizeObserver implements ResizeObserver {
  disconnect() {}
  observe() {}
  unobserve() {}
}

Object.defineProperty(globalThis, "ResizeObserver", { configurable: true, value: MockResizeObserver });

class MockWebSocket extends EventTarget {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  readonly url: string;
  readonly protocol: string;
  readyState = MockWebSocket.CONNECTING;

  constructor(url: string | URL, protocols?: string | string[]) {
    super();
    this.url = String(url);
    this.protocol = Array.isArray(protocols) ? protocols[0] ?? "" : protocols ?? "";
    queueMicrotask(() => {
      if (this.readyState !== MockWebSocket.CONNECTING) return;
      this.readyState = MockWebSocket.OPEN;
      this.dispatchEvent(new Event("open"));
      this.dispatchEvent(new MessageEvent("message", { data: JSON.stringify({ type: "ready", protocol_version: 1, epoch: "mock-epoch" }) }));
    });
  }

  send() {}

  close(code = 1000, reason = "") {
    if (this.readyState === MockWebSocket.CLOSED) return;
    this.readyState = MockWebSocket.CLOSED;
    this.dispatchEvent(new CloseEvent("close", { code, reason, wasClean: true }));
  }
}

Object.defineProperty(globalThis, "WebSocket", { configurable: true, writable: true, value: MockWebSocket });

const jsdomGetComputedStyle = window.getComputedStyle.bind(window);
Object.defineProperty(window, "getComputedStyle", {
  configurable: true,
  value: (element: Element) => jsdomGetComputedStyle(element),
});

Object.defineProperty(window, "matchMedia", {
  writable: true,
  value: vi.fn().mockImplementation((query: string) => ({
    matches: false,
    media: query,
    onchange: null,
    addListener: vi.fn(),
    removeListener: vi.fn(),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  })),
});

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  cleanup();
  server.resetHandlers();
  resetMockState();
  clearAccessSession(true);
  window.localStorage.clear();
  window.sessionStorage.clear();
  window.history.replaceState({}, "", "/");
});

/**
 * 收尾时清除本文件仍未触发的定时器。
 *
 * antd 的 Button 与 Form 经 @rc-component/util 的 useDelayState 排期 0/10/100ms 定时器，该 hook 只在下一次
 * 赋值时取消上一个，组件卸载时不清理；测试文件跑完后回调仍会触发，此时本文件的 jsdom 已拆除，回调里的
 * React setState 会读取 window 并抛 `ReferenceError: window is not defined`，让 vitest 以 exit 1 失败
 * （CI 上表现为用例全部通过但仍然失败）。此时所有用例与 afterEach 均已完成，不再有代码需要这些定时器。
 */
const pendingTimers = new Set<ReturnType<typeof globalThis.setTimeout>>();
const originalSetTimeout = globalThis.setTimeout;
const originalClearTimeout = globalThis.clearTimeout;

// 运行期是 jsdom，但本仓库的 setTimeout 类型来自 @types/node，因此这里的包装按运行期实际形状断言。
globalThis.setTimeout = ((handler: (...handlerArgs: unknown[]) => void, timeout?: number, ...args: unknown[]) => {
  const handle = originalSetTimeout(() => {
    pendingTimers.delete(handle);
    handler(...args);
  }, timeout);
  pendingTimers.add(handle);
  return handle;
}) as unknown as typeof globalThis.setTimeout;

globalThis.clearTimeout = ((handle: ReturnType<typeof globalThis.setTimeout>) => {
  pendingTimers.delete(handle);
  originalClearTimeout(handle);
}) as unknown as typeof globalThis.clearTimeout;

afterAll(() => {
  for (const handle of pendingTimers) originalClearTimeout(handle);
  pendingTimers.clear();
});
afterAll(() => server.close());
