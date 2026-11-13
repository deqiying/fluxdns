import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { getSummaryPollInterval } from "@/app/query-client";
import { ApiError } from "@/shared/api/errors";
import { collectMemorySnapshot, getMemoryAnalysis, memoryAnalysisKey, type MemoryAnalysis } from "./api";

/** OS 自动读取与手动模块扫描分离；POST 不进入 Query 重试机制。 */
export function useMemoryAnalysis() {
  const client = useQueryClient();
  const active = useRef<AbortController | null>(null);
  const mounted = useRef(false);
  const [pending, setPending] = useState(false);
  const [collectionError, setCollectionError] = useState<unknown>(null);
  const query = useQuery({
    queryKey: memoryAnalysisKey,
    queryFn: ({ signal }) => getMemoryAnalysis(signal),
    enabled: !pending,
    staleTime: 10_000,
    refetchInterval: () => getSummaryPollInterval(),
    refetchIntervalInBackground: false,
    retry: false,
  });

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      active.current?.abort();
    };
  }, []);

  const collect = async () => {
    const before = client.getQueryData<MemoryAnalysis>(memoryAnalysisKey);
    if (active.current || before?.collection_state === "collecting") return;
    const controller = new AbortController();
    active.current = controller;
    setPending(true);
    setCollectionError(null);
    try {
      // 先终止旧 GET，避免它晚到后覆盖 POST 返回的新快照。
      await client.cancelQueries({ queryKey: memoryAnalysisKey, exact: true });
      controller.signal.throwIfAborted();
      const data = await collectMemorySnapshot(controller.signal);
      if (controller.signal.aborted || !mounted.current) return;
      client.setQueryData(memoryAnalysisKey, data);
      const unchanged = before?.process_id === data.process_id && before.started_at_ms === data.started_at_ms
        && before.snapshot !== null && before.snapshot.snapshot_id === data.snapshot?.snapshot_id;
      if (data.collection_state !== "idle" || data.snapshot === null || unchanged) {
        setCollectionError(new ApiError({ code: "MEMORY_COLLECTION_FAILED", kind: "invalid-response", message: "collection did not complete" }));
      }
    } catch (error) {
      if (mounted.current) setCollectionError(error);
    } finally {
      if (active.current === controller) active.current = null;
      if (mounted.current) setPending(false);
    }
  };

  return { query, collect, pending, collectionError, cancel: () => active.current?.abort() };
}
