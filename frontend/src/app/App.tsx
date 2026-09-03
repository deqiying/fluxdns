import { lazy, Suspense } from "react";
import { Spin } from "antd";
import { Navigate, Route, Routes } from "react-router-dom";
import { ProtectedRoute } from "@/modules/auth/ProtectedRoute";

const AppLayout = lazy(() => import("@/shared/components/AppLayout").then((module) => ({ default: module.AppLayout })));
const DashboardPage = lazy(() => import("@/modules/dashboard/DashboardPage").then((module) => ({ default: module.DashboardPage })));
const HealthPage = lazy(() => import("@/modules/health/HealthPage").then((module) => ({ default: module.HealthPage })));
const LoginPage = lazy(() => import("@/modules/auth/LoginPage").then((module) => ({ default: module.LoginPage })));
const InitializePage = lazy(() => import("@/modules/auth/InitializePage").then((module) => ({ default: module.InitializePage })));
const QueriesPage = lazy(() => import("@/modules/queries/QueriesPage").then((module) => ({ default: module.QueriesPage })));
const ResourcesPage = lazy(() => import("@/modules/resources/ResourcesPage").then((module) => ({ default: module.ResourcesPage })));
const RuntimePage = lazy(() => import("@/modules/runtime/RuntimePage").then((module) => ({ default: module.RuntimePage })));
const StatisticsPage = lazy(() => import("@/modules/statistics/StatisticsPage").then((module) => ({ default: module.StatisticsPage })));
const SystemPage = lazy(() => import("@/modules/system/SystemPage").then((module) => ({ default: module.SystemPage })));
const NotFoundPage = lazy(() => import("./NotFoundPage").then((module) => ({ default: module.NotFoundPage })));

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
            <Route path="/runtime" element={<RuntimePage />} />
            <Route path="/health" element={<HealthPage />} />
            <Route path="/statistics" element={<StatisticsPage />} />
            <Route path="/queries" element={<QueriesPage />} />
            <Route path="/resources" element={<ResourcesPage />} />
            <Route path="/system" element={<SystemPage />} />
            <Route path="*" element={<NotFoundPage />} />
          </Route>
        </Route>
      </Routes>
    </Suspense>
  );
}
