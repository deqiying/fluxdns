import { useEffect, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { managementEvents, type EventConnectionState } from "@/shared/api/events";
import { getServiceMetrics, serviceMetricsKey } from "./api";

export function useServiceMetrics() {
  const queryClient = useQueryClient();
  const [connectionState, setConnectionState] = useState<EventConnectionState>("connecting");
  const [clock, setClock] = useState(() => Date.now());
  const query = useQuery({
    queryKey: serviceMetricsKey,
    queryFn: ({ signal }) => getServiceMetrics(signal),
    staleTime: 3_000,
    refetchOnWindowFocus: false,
  });

  useEffect(() => {
    if (!query.isSuccess) return;
    let active = true;
    let unsubscribe: (() => void) | undefined;
    const subscribe = () => {
      if (!active || document.visibilityState === "hidden") return;
      unsubscribe = managementEvents.subscribeMetrics(
        (metrics) => queryClient.setQueryData(serviceMetricsKey, metrics),
        setConnectionState,
      );
    };
    const visibilityChanged = () => {
      unsubscribe?.();
      unsubscribe = undefined;
      if (document.visibilityState !== "hidden") {
        void query.refetch().then(() => subscribe());
      }
    };
    subscribe();
    document.addEventListener("visibilitychange", visibilityChanged);
    return () => {
      active = false;
      document.removeEventListener("visibilitychange", visibilityChanged);
      unsubscribe?.();
    };
  }, [query.isSuccess, query.refetch, queryClient]);

  useEffect(() => {
    const timer = window.setInterval(() => setClock(Date.now()), 1_000);
    return () => window.clearInterval(timer);
  }, []);

  const stale = query.data !== undefined
    && (clock - query.data.sampled_at_ms > 3_000 || connectionState === "error");
  return { ...query, connectionState, stale };
}
