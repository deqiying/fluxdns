import type { ReactNode } from "react";
import { ConfigProvider, App as AntApp } from "antd";
import { QueryClientProvider, type QueryClient } from "@tanstack/react-query";
import { BrowserRouter } from "react-router-dom";
import { AuthProvider } from "@/modules/auth/AuthProvider";
import { createAppQueryClient } from "./query-client";

const defaultQueryClient = createAppQueryClient();

export function AppProviders({ children, queryClient = defaultQueryClient }: { children: ReactNode; queryClient?: QueryClient }) {
  return (
    <ConfigProvider
      theme={{
        token: {
          colorPrimary: "#0f9f93",
          colorInfo: "#0891b2",
          colorText: "#172033",
          colorBgLayout: "#f3f6fa",
          borderRadius: 12,
          fontFamily: 'Inter, "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif',
        },
        components: {
          Card: { borderRadiusLG: 16 },
          Button: { borderRadius: 9 },
          Table: { headerBg: "#f7f9fb", headerColor: "#536174" },
        },
      }}
    >
      <AntApp>
        <QueryClientProvider client={queryClient}>
          <BrowserRouter>
            <AuthProvider>{children}</AuthProvider>
          </BrowserRouter>
        </QueryClientProvider>
      </AntApp>
    </ConfigProvider>
  );
}
