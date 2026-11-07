import { Suspense } from "react";
import { Spin } from "antd";
import { Navigate, Route, Routes } from "react-router-dom";
import { ProtectedRoute } from "@/modules/auth/ProtectedRoute";
// 认证入口随主入口加载，避免服务更新使 session 失效后，还要请求已被替换的旧分片。
import { LoginPage } from "@/modules/auth/LoginPage";
import { InitializePage } from "@/modules/auth/InitializePage";
import { lazyPage } from "./lazy-page";

const AppLayout = lazyPage(() => import("@/shared/components/AppLayout").then((module) => ({ default: module.AppLayout })));
const DashboardPage = lazyPage(() => import("@/modules/dashboard/DashboardPage").then((module) => ({ default: module.DashboardPage })));
const QueriesPage = lazyPage(() => import("@/modules/queries/QueriesPage").then((module) => ({ default: module.QueriesPage })));
const SystemPage = lazyPage(() => import("@/modules/system/SystemPage").then((module) => ({ default: module.SystemPage })));
const ProxiesPage = lazyPage(() => import("@/modules/proxies/ProxiesPage").then((module) => ({ default: module.ProxiesPage })));
const HostsPage = lazyPage(() => import("@/modules/hosts/HostsPage").then((module) => ({ default: module.HostsPage })));
const RuleSetsPage = lazyPage(() => import("@/modules/rule-sets/RuleSetsPage").then((module) => ({ default: module.RuleSetsPage })));
const StrategiesPage = lazyPage(() => import("@/modules/strategies/StrategiesPage").then((module) => ({ default: module.StrategiesPage })));
const ListenersPage = lazyPage(() => import("@/modules/listeners/ListenersPage").then((module) => ({ default: module.ListenersPage })));
const ClientsPage = lazyPage(() => import("@/modules/clients/ClientsPage").then((module) => ({ default: module.ClientsPage })));
const DnsSettingsPage = lazyPage(() => import("@/modules/dns-settings/DnsSettingsPage").then((module) => ({ default: module.DnsSettingsPage })));
const SystemSettingsPage = lazyPage(() => import("@/modules/system-settings/SystemSettingsPage").then((module) => ({ default: module.SystemSettingsPage })));
const UpstreamsPage = lazyPage(() => import("@/modules/upstreams/UpstreamsPage").then((module) => ({ default: module.UpstreamsPage })));
const NotFoundPage = lazyPage(() => import("./NotFoundPage").then((module) => ({ default: module.NotFoundPage })));

export function App() {
  return (
    <Suspense fallback={<div className="fullscreen-state"><Spin size="large" /></div>}>
      <Routes>
        <Route path="/login" element={<LoginPage />} />
        <Route path="/initialize" element={<InitializePage />} />
        <Route element={<ProtectedRoute />}>
          <Route element={<AppLayout />}>
            <Route index element={<Navigate to="/dashboard" replace />} />
            <Route path="/dashboard" element={<DashboardPage />} />
            <Route path="/queries" element={<QueriesPage />} />
            <Route path="/listeners" element={<ListenersPage />} />
            <Route path="/upstreams" element={<UpstreamsPage />} />
            <Route path="/dns-settings" element={<DnsSettingsPage />} />
            <Route path="/hosts" element={<HostsPage />} />
            <Route path="/rule-sets" element={<RuleSetsPage />} />
            <Route path="/strategies" element={<StrategiesPage />} />
            <Route path="/clients" element={<ClientsPage />} />
            <Route path="/proxies" element={<ProxiesPage />} />
            <Route path="/system-settings" element={<SystemSettingsPage />} />
            <Route path="/system-runtime" element={<SystemPage />} />
            <Route path="*" element={<NotFoundPage />} />
          </Route>
        </Route>
      </Routes>
    </Suspense>
  );
}
