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
const PendingModulePage = lazy(() => import("./PendingModulePage").then((module) => ({ default: module.PendingModulePage })));
const PendingUpstreamsPage = lazy(() => import("./PendingModulePage").then((module) => ({ default: module.PendingUpstreamsPage })));
const NotFoundPage = lazy(() => import("./NotFoundPage").then((module) => ({ default: module.NotFoundPage })));

const pendingRoutes = managementRoutes.filter(
  ({ path }) => path !== "/dashboard" && path !== "/queries" && path !== "/upstreams" && path !== "/hosts" && path !== "/rule-sets" && path !== "/proxies" && path !== "/system-runtime",
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
            <Route path="/upstreams" element={<PendingUpstreamsPage />} />
            <Route path="/hosts" element={<HostsPage />} />
            <Route path="/rule-sets" element={<RuleSetsPage />} />
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
