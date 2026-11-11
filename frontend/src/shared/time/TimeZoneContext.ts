import { createContext, useContext } from "react";

export interface ServerTimeZone {
  timeZone: string;
  fallbackReason: string | null;
}

// 独立组件/测试可显式使用 UTC；正式受保护页面由应用入口提供服务端时区。
export const TimeZoneContext = createContext<ServerTimeZone>({ timeZone: "UTC", fallbackReason: null });

export function useServerTimeZone(): ServerTimeZone {
  return useContext(TimeZoneContext);
}
