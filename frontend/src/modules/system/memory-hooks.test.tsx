import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { memoryAnalysisFixture, memorySnapshotFixture } from "@/mocks/memory-fixtures";
import { ApiError } from "@/shared/api/errors";
import { collectMemorySnapshot, getMemoryAnalysis, memoryAnalysisKey, type MemoryAnalysis } from "./api";
import { useMemoryAnalysis } from "./memory-hooks";

vi.mock("./api", () => ({
  collectMemorySnapshot: vi.fn(), getMemoryAnalysis: vi.fn(),
  memoryAnalysisKey: ["api", "v2", "system", "memory"],
}));

function renderMemory() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: 3, retryDelay: 100, gcTime: 0, staleTime: 10_000 } } });
  const view = renderHook(() => useMemoryAnalysis(), { wrapper: ({ children }) => <QueryClientProvider client={client}>{children}</QueryClientProvider> });
  return { ...view, client };
}

async function flush() {
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
}

describe("memory collection lifecycle", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.mocked(getMemoryAnalysis).mockReset().mockResolvedValue(memoryAnalysisFixture);
    vi.mocked(collectMemorySnapshot).mockReset();
    Object.defineProperty(document, "visibilityState", { configurable: true, value: "visible" });
  });
  afterEach(() => {
    vi.useRealTimers();
    Object.defineProperty(document, "visibilityState", { configurable: true, value: "visible" });
  });

  it("只在可见时每 30 秒 GET，不自动 POST", async () => {
    const { result, unmount } = renderMemory();
    await flush();
    expect(result.current.query.data).toBeDefined();
    expect(getMemoryAnalysis).toHaveBeenCalledTimes(1);
    await act(async () => { await vi.advanceTimersByTimeAsync(30_000); });
    expect(getMemoryAnalysis).toHaveBeenCalledTimes(2);
    Object.defineProperty(document, "visibilityState", { configurable: true, value: "hidden" });
    await act(async () => { await vi.advanceTimersByTimeAsync(90_000); });
    expect(getMemoryAnalysis).toHaveBeenCalledTimes(2);
    expect(collectMemorySnapshot).not.toHaveBeenCalled();
    unmount();
  });

  it("同步双击只有一个 POST，成功更新 GET 缓存", async () => {
    let resolve!: (value: MemoryAnalysis) => void;
    vi.mocked(collectMemorySnapshot).mockImplementation(() => new Promise((done) => { resolve = done; }));
    const { result, client, unmount } = renderMemory();
    await flush();
    let pending!: Promise<void>;
    await act(async () => {
      pending = result.current.collect();
      void result.current.collect();
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(collectMemorySnapshot).toHaveBeenCalledTimes(1);
    expect(result.current.pending).toBe(true);
    await act(async () => { resolve({ ...memoryAnalysisFixture, snapshot: memorySnapshotFixture }); await pending; });
    expect(client.getQueryData<MemoryAnalysis>(memoryAnalysisKey)?.snapshot?.snapshot_id).toBe("1");
    expect(result.current.pending).toBe(false);
    unmount();
  });

  it("超时失败不因 Query 默认 retry=3 而重试扫描，旧缓存不变", async () => {
    const timeout = new ApiError({ code: "REQUEST_TIMEOUT", kind: "timeout", message: "timeout", retryable: true });
    vi.mocked(collectMemorySnapshot).mockRejectedValue(timeout);
    const { result, client, unmount } = renderMemory();
    await flush();
    await act(async () => { await result.current.collect(); });
    await act(async () => { await vi.advanceTimersByTimeAsync(20_000); });
    expect(collectMemorySnapshot).toHaveBeenCalledTimes(1);
    expect(result.current.collectionError).toBe(timeout);
    expect(client.getQueryData<MemoryAnalysis>(memoryAnalysisKey)?.snapshot).toBeNull();
    unmount();
  });

  it("取消等待传播 abort，迟到响应不更新缓存，也不会重试", async () => {
    let signal!: AbortSignal;
    let resolve!: (value: MemoryAnalysis) => void;
    vi.mocked(collectMemorySnapshot).mockImplementation((input) => {
      signal = input;
      return new Promise((done) => { resolve = done; });
    });
    const { result, client, unmount } = renderMemory();
    await flush();
    let pending!: Promise<void>;
    await act(async () => { pending = result.current.collect(); await vi.advanceTimersByTimeAsync(0); });
    act(() => result.current.cancel());
    expect(signal.aborted).toBe(true);
    await act(async () => { resolve({ ...memoryAnalysisFixture, snapshot: memorySnapshotFixture }); await pending; });
    expect(client.getQueryData<MemoryAnalysis>(memoryAnalysisKey)?.snapshot).toBeNull();
    expect(collectMemorySnapshot).toHaveBeenCalledTimes(1);
    unmount();
  });

  it("卸载 abort 在途 POST，禁止迟到结果更新缓存", async () => {
    let signal!: AbortSignal;
    let resolve!: (value: MemoryAnalysis) => void;
    vi.mocked(collectMemorySnapshot).mockImplementation((input) => {
      signal = input;
      return new Promise((done) => { resolve = done; });
    });
    const { result, client, unmount } = renderMemory();
    await flush();
    let pending!: Promise<void>;
    await act(async () => { pending = result.current.collect(); await vi.advanceTimersByTimeAsync(0); });
    unmount();
    expect(signal.aborted).toBe(true);
    await act(async () => { resolve({ ...memoryAnalysisFixture, snapshot: memorySnapshotFixture }); await pending; });
    expect(client.getQueryData<MemoryAnalysis>(memoryAnalysisKey)?.snapshot ?? null).toBeNull();
  });

  it("服务端 collecting 时不再提交 POST", async () => {
    vi.mocked(getMemoryAnalysis).mockResolvedValue({ ...memoryAnalysisFixture, collection_state: "collecting" });
    const { result, unmount } = renderMemory();
    await flush();
    await act(async () => { await result.current.collect(); });
    expect(collectMemorySnapshot).not.toHaveBeenCalled();
    unmount();
  });

  it("POST 200 但仍是原快照时不把旧数据当本次成功", async () => {
    const previous = { ...memoryAnalysisFixture, snapshot: memorySnapshotFixture };
    vi.mocked(getMemoryAnalysis).mockResolvedValue(previous);
    vi.mocked(collectMemorySnapshot).mockResolvedValue(previous);
    const { result, unmount } = renderMemory();
    await flush();
    await act(async () => { await result.current.collect(); });
    expect(result.current.collectionError).toBeInstanceOf(ApiError);
    expect(collectMemorySnapshot).toHaveBeenCalledTimes(1);
    unmount();
  });
});
