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
          colorPrimary: "#007aff",
          colorInfo: "#007aff",
          colorSuccess: "#20824e",
          colorWarning: "#b26a00",
          colorError: "#c83a3a",
          colorText: "#202124",
          colorTextSecondary: "#63666d",
          colorBorderSecondary: "#e0e2e6",
          colorBgLayout: "#f7f7f8",
          borderRadius: 6,
          fontFamily: 'Inter, "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif',
        },
        components: {
          Card: { borderRadiusLG: 8 },
          Button: { borderRadius: 6 },
          Table: { headerBg: "#f6f7f8", headerColor: "#535861" },
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
