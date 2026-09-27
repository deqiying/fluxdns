import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { ApiError } from "@/shared/api/errors";
import { SESSION_KEEPALIVE_INTERVAL_MS, onUnauthorized, renewAccessSession, reportUnauthorized } from "@/shared/api/client";
import type { LoginRequest, Session, SetupRequest, SetupStatus } from "@/shared/api/types";
import {
  authKeys,
  getSetupStatus,
  getSession,
  initializeWebUi,
  login as requestLogin,
  logout as requestLogout,
} from "./api";

interface AuthContextValue {
  setupStatus: SetupStatus | undefined;
  setupRequired: boolean;
  session: Session | null | undefined;
  isLoading: boolean;
  error: unknown;
  initialize: (credentials: SetupRequest) => Promise<Session>;
  refreshSetup: () => Promise<SetupStatus | undefined>;
  login: (credentials: LoginRequest) => Promise<Session>;
  logout: () => Promise<void>;
  isInitializing: boolean;
  isLoggingIn: boolean;
  isLoggingOut: boolean;
  sessionExpired: boolean;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [sessionExpired, setSessionExpired] = useState(false);
  const setupQuery = useQuery({
    queryKey: authKeys.setup,
    queryFn: ({ signal }) => getSetupStatus(signal),
    staleTime: 60_000,
    retry: false,
  });
  const setupReady = setupQuery.data?.state === "ready";
  const sessionQuery = useQuery({
    queryKey: authKeys.session,
    queryFn: ({ signal }) => getSession(signal),
    staleTime: 60_000,
    retry: false,
    enabled: setupReady,
  });
  const hasSession = Boolean(sessionQuery.data);

  const initializeMutation = useMutation({ mutationFn: initializeWebUi });
  const loginMutation = useMutation({ mutationFn: requestLogin });
  const logoutMutation = useMutation({ mutationFn: requestLogout });

  useEffect(
    () =>
      onUnauthorized(() => {
        void queryClient.cancelQueries();
        // 与登出一样回收上一会话的数据，避免重新登录后同名 query key 复用旧值。
        queryClient.clear();
        // 由 ProtectedRoute 统一跳转，避免命令式导航与 session 更新产生竞争。
        setSessionExpired(true);
        queryClient.setQueryData(authKeys.session, null);
      }),
    [queryClient],
  );

  // 空闲保活：同源刷新既提前换发访问凭据，也更新服务端会话活跃时间，
  // 隐藏标签或休眠回来不会再因服务端空闲期限到达而直接退出到登录页。
  useEffect(() => {
    if (!hasSession) return;
    const keepAlive = () => {
      void renewAccessSession().catch((error: unknown) => {
        // 只有刷新凭据失效（401）才是登录态结束；手动登出后的迟到 401 不改写登录页状态，
        // 网络和超时等瞬时错误留给下一次保活或下一次业务请求重试。
        if (error instanceof ApiError && error.status === 401 && queryClient.getQueryData(authKeys.session)) {
          reportUnauthorized();
        }
      });
    };
    const timer = window.setInterval(keepAlive, SESSION_KEEPALIVE_INTERVAL_MS);
    const onVisibilityChange = () => {
      if (document.visibilityState === "visible") keepAlive();
    };
    document.addEventListener("visibilitychange", onVisibilityChange);
    window.addEventListener("online", keepAlive);
    return () => {
      window.clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      window.removeEventListener("online", keepAlive);
    };
  }, [hasSession, queryClient]);

  const performLogin = useCallback(
    async (credentials: LoginRequest) => {
      const session = await loginMutation.mutateAsync(credentials);
      setSessionExpired(false);
      queryClient.setQueryData(authKeys.session, session);
      return session;
    },
    [loginMutation, queryClient],
  );

  const performInitialize = useCallback(
    async (credentials: SetupRequest) => {
      const session = await initializeMutation.mutateAsync(credentials);
      setSessionExpired(false);
      queryClient.setQueryData<SetupStatus>(authKeys.setup, { state: "ready" });
      queryClient.setQueryData(authKeys.session, session);
      return session;
    },
    [initializeMutation, queryClient],
  );

  const refreshSetup = useCallback(async () => {
    const result = await setupQuery.refetch();
    return result.data;
  }, [setupQuery]);

  const performLogout = useCallback(async () => {
    try {
      await logoutMutation.mutateAsync();
    } finally {
      setSessionExpired(false);
      await queryClient.cancelQueries();
      queryClient.clear();
      queryClient.setQueryData(authKeys.session, null);
      navigate("/login", { replace: true });
    }
  }, [logoutMutation, navigate, queryClient]);

  const value = useMemo<AuthContextValue>(
    () => ({
      setupStatus: setupQuery.data,
      setupRequired: setupQuery.data?.state === "required",
      session: setupReady ? sessionQuery.data : setupQuery.data ? null : undefined,
      isLoading: setupQuery.isLoading || (setupReady && sessionQuery.isLoading),
      error: setupQuery.error ?? (setupReady ? sessionQuery.error : undefined),
      initialize: performInitialize,
      refreshSetup,
      login: performLogin,
      logout: performLogout,
      isInitializing: initializeMutation.isPending,
      isLoggingIn: loginMutation.isPending,
      isLoggingOut: logoutMutation.isPending,
      sessionExpired,
    }),
    [
      setupQuery.data,
      setupQuery.isLoading,
      setupQuery.error,
      setupReady,
      sessionQuery.data,
      sessionQuery.isLoading,
      sessionQuery.error,
      performInitialize,
      refreshSetup,
      performLogin,
      performLogout,
      initializeMutation.isPending,
      loginMutation.isPending,
      logoutMutation.isPending,
      sessionExpired,
    ],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthContextValue {
  const value = useContext(AuthContext);
  if (!value) {
    throw new Error("useAuth 必须在 AuthProvider 内调用");
  }
  return value;
}
