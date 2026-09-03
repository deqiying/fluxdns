import { useQuery } from "@tanstack/react-query";
import { getResources, resourcesKey } from "./api";

export function useResources() {
  return useQuery({
    queryKey: resourcesKey,
    queryFn: ({ signal }) => getResources(signal),
  });
}
