import { lazy, Suspense } from "react";
import { Spin } from "antd";
import { Navigate, Route, Routes } from "react-router-dom";
import { ProtectedRoute } from "@/modules/auth/ProtectedRoute";
import { managementRoutes } from "./route-contract";

const AppLayout = lazy(() => import("@/shared/components/AppLayout").then((module) => ({ default: module.AppLayout })));
const DashboardPage = lazy(() => import("@/modules/dashboard/DashboardPage").then((module) => ({ default: module.DashboardPage })));
const LoginPage = lazy(() => import("@/modules/auth/LoginPage").then((module) => ({ default: module.LoginPage })));
const InitializePage = lazy(() => import("@/modules/auth/InitializePage").then((module) => ({ default: module.InitializePage })));
const QueriesPage = lazy(() => import("@/modules/queries/QueriesPage").then((module) => ({ default: module.QueriesPage })));
const SystemPage = lazy(() => import("@/modules/system/SystemPage").then((module) => ({ default: module.SystemPage })));
const ProxiesPage = lazy(() => import("@/modules/proxies/ProxiesPage").then((module) => ({ default: module.ProxiesPage })));
const HostsPage = lazy(() => import("@/modules/hosts/HostsPage").then((module) => ({ default: module.HostsPage })));
const RuleSetsPage = lazy(() => import("@/modules/rule-sets/RuleSetsPage").then((module) => ({ default: module.RuleSetsPage })));
const StrategiesPage = lazy(() => import("@/modules/strategies/StrategiesPage").then((module) => ({ default: module.StrategiesPage })));
const ListenersPage = lazy(() => import("@/modules/listeners/ListenersPage").then((module) => ({ default: module.ListenersPage })));
const ClientsPage = lazy(() => import("@/modules/clients/ClientsPage").then((module) => ({ default: module.ClientsPage })));
const DnsSettingsPage = lazy(() => import("@/modules/dns-settings/DnsSettingsPage").then((module) => ({ default: module.DnsSettingsPage })));
const PendingModulePage = lazy(() => import("./PendingModulePage").then((module) => ({ default: module.PendingModulePage })));
const UpstreamsPage = lazy(() => import("@/modules/upstreams/UpstreamsPage").then((module) => ({ default: module.UpstreamsPage })));
const NotFoundPage = lazy(() => import("./NotFoundPage").then((module) => ({ default: module.NotFoundPage })));

const pendingRoutes = managementRoutes.filter(
  ({ path }) => path !== "/dashboard" && path !== "/queries" && path !== "/listeners" && path !== "/upstreams" && path !== "/dns-settings" && path !== "/strategies" && path !== "/hosts" && path !== "/rule-sets" && path !== "/clients" && path !== "/proxies" && path !== "/system-runtime",
);

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
            <Route path="/system-runtime" element={<SystemPage />} />
            {pendingRoutes.map((route) => (
              <Route key={route.path} path={route.path} element={<PendingModulePage title={route.title} />} />
            ))}
            <Route path="*" element={<NotFoundPage />} />
          </Route>
        </Route>
      </Routes>
    </Suspense>
  );
}
