import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { onUnauthorized } from "@/shared/api/client";
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

  const initializeMutation = useMutation({ mutationFn: initializeWebUi });
  const loginMutation = useMutation({ mutationFn: requestLogin });
  const logoutMutation = useMutation({ mutationFn: requestLogout });

  useEffect(
    () =>
      onUnauthorized(() => {
        void queryClient.cancelQueries();
        // 由 ProtectedRoute 统一跳转，避免命令式导航与 session 更新产生竞争。
        setSessionExpired(true);
        queryClient.setQueryData(authKeys.session, null);
      }),
    [queryClient],
  );

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
