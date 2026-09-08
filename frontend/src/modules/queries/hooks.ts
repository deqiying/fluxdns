import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  managementEvents,
  type EventConnectionState,
  type ResyncReason,
} from "@/shared/api/events";
import { getQueries, queryRecordKeys, type QueryRecord, type QueryRequest } from "./api";
import {
  appendRealtimeBuffer,
  emptyRealtimeBuffer,
  mergeLatestRecords,
  type RealtimeBuffer,
} from "./realtime";

export interface QueryRealtimeOptions {
  autoRefresh: boolean;
  holdUpdates: boolean;
  applyImmediately: boolean;
}

export function useQueries(request: QueryRequest, options: QueryRealtimeOptions) {
  const query = useQuery({
    queryKey: queryRecordKeys.list(request),
    queryFn: ({ signal }) => getQueries(request, signal),
    placeholderData: keepPreviousData,
  });
  const [items, setItems] = useState<QueryRecord[]>([]);
  const [directoryRevisions, setDirectoryRevisions] = useState<Map<string, string>>(new Map());
  const [buffer, setBuffer] = useState<RealtimeBuffer>(emptyRealtimeBuffer);
  const [connectionState, setConnectionState] = useState<EventConnectionState>("closed");
  const [lastUpdatedAt, setLastUpdatedAt] = useState<number>();
  const [resyncReason, setResyncReason] = useState<ResyncReason>();
  const [subscriptionGeneration, setSubscriptionGeneration] = useState(0);
  const [visible, setVisible] = useState(() => document.visibilityState !== "hidden");
  const holdUpdates = useRef(options.holdUpdates);
  const visibleIds = useRef<Set<string>>(new Set());
  const bufferedRevisions = useRef(new Map<string, string>());
  const requestKey = useMemo(() => JSON.stringify(request), [request]);
  const latestPage = options.applyImmediately && request.cursor === null && request.direction === "older"
    && request.sort === "occurred_at" && request.order === "desc";

  holdUpdates.current = options.holdUpdates;
  visibleIds.current = new Set(items.map(({ id }) => id));

  useEffect(() => {
    if (!query.data || query.isPlaceholderData) return;
    setItems(query.data.items);
    setDirectoryRevisions(new Map(query.data.items.map((record) => [record.id, query.data!.directory_revision])));
    bufferedRevisions.current.clear();
    setBuffer(emptyRealtimeBuffer());
    setResyncReason(undefined);
  }, [query.data, query.isPlaceholderData, requestKey]);

  useEffect(() => {
    setDirectoryRevisions((current) => {
      const next = new Map(items.map((record) => [record.id, current.get(record.id) ?? "unknown"]));
      if (next.size === current.size && [...next].every(([id, revision]) => current.get(id) === revision)) return current;
      return next;
    });
  }, [items]);

  const resynchronize = useCallback(async () => {
    const result = await query.refetch();
    if (!result.data) return;
    setItems(result.data.items);
    setDirectoryRevisions(new Map(result.data.items.map((record) => [record.id, result.data!.directory_revision])));
    bufferedRevisions.current.clear();
    setBuffer(emptyRealtimeBuffer());
    setResyncReason(undefined);
    setSubscriptionGeneration((value) => value + 1);
  }, [query.refetch]);

  useEffect(() => {
    let active = true;
    const visibilityChanged = () => {
      if (document.visibilityState === "hidden") {
        setVisible(false);
        return;
      }
      if (!options.autoRefresh) {
        setVisible(true);
        return;
      }
      void resynchronize().finally(() => {
        if (active) setVisible(true);
      });
    };
    document.addEventListener("visibilitychange", visibilityChanged);
    return () => {
      active = false;
      document.removeEventListener("visibilitychange", visibilityChanged);
    };
  }, [options.autoRefresh, resynchronize]);

  useEffect(() => {
    if (!options.autoRefresh || !visible) {
      setConnectionState("closed");
      if (!options.autoRefresh) {
        bufferedRevisions.current.clear();
        setBuffer(emptyRealtimeBuffer());
        setResyncReason(undefined);
      }
      return;
    }
    if (!query.data || query.isPlaceholderData) return;
    const pageSize = request.page_size;
    return managementEvents.subscribeQueries(
      {
        filter: request.filter,
        after: query.data.snapshot_cursor,
        retentionRevision: query.data.retention_revision,
      },
      (batch) => {
        setLastUpdatedAt(Date.now());
        if (latestPage && !holdUpdates.current) {
          setDirectoryRevisions((current) => {
            const next = new Map(current);
            for (const record of batch.items) {
              if (!next.has(record.id)) next.set(record.id, batch.directoryRevision);
            }
            return next;
          });
          setItems((current) => mergeLatestRecords(current, batch.items, pageSize));
          return;
        }
        setBuffer((current) => {
          const next = appendRealtimeBuffer(current, batch.items, visibleIds.current);
          if (next.overflow) bufferedRevisions.current.clear();
          else for (const record of batch.items) {
            if (!visibleIds.current.has(record.id) && !bufferedRevisions.current.has(record.id)) {
              bufferedRevisions.current.set(record.id, batch.directoryRevision);
            }
          }
          return next;
        });
      },
      setResyncReason,
      setConnectionState,
    );
  }, [
    latestPage,
    options.applyImmediately,
    options.autoRefresh,
    query.data,
    query.isPlaceholderData,
    request.filter,
    request.page_size,
    requestKey,
    subscriptionGeneration,
    visible,
  ]);

  useEffect(() => {
    if (!latestPage || options.holdUpdates) return;
    if (buffer.overflow || resyncReason) {
      void resynchronize();
      return;
    }
    if (buffer.items.length === 0) return;
    const pendingRevisions = new Map(bufferedRevisions.current);
    setDirectoryRevisions((current) => {
      const next = new Map(current);
      for (const record of buffer.items) {
        if (!next.has(record.id)) next.set(record.id, pendingRevisions.get(record.id) ?? query.data?.directory_revision ?? "unknown");
      }
      return next;
    });
    setItems((current) => mergeLatestRecords(current, buffer.items, request.page_size));
    bufferedRevisions.current.clear();
    setBuffer(emptyRealtimeBuffer());
  }, [buffer, latestPage, options.holdUpdates, query.data?.directory_revision, request.page_size, resyncReason, resynchronize]);

  return {
    ...query,
    items,
    directoryRevisions,
    connectionState,
    lastUpdatedAt,
    pendingCount: buffer.overflow || resyncReason ? null : buffer.items.length,
    requiresResync: buffer.overflow || resyncReason !== undefined,
    resyncReason,
    resynchronize,
  };
}
