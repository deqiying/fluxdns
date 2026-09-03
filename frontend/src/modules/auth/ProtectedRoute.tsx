import { Navigate, Outlet, useLocation } from "react-router-dom";
import { Spin } from "antd";
import { PageState } from "@/shared/components/PageState";
import { useAuth } from "./AuthProvider";

export function ProtectedRoute() {
  const auth = useAuth();
  const location = useLocation();

  if (auth.isLoading) {
    return (
      <div className="fullscreen-state" aria-label="正在恢复登录状态">
        <Spin size="large" />
      </div>
    );
  }

  if (auth.error) {
    return (
      <div className="fullscreen-state">
        <PageState error={auth.error} onRetry={() => window.location.reload()} />
      </div>
    );
  }

  if (!auth.session) {
    return (
      <Navigate
        to="/login"
        replace
        state={{ from: location.pathname, sessionExpired: auth.sessionExpired }}
      />
    );
  }

  return <Outlet />;
}
