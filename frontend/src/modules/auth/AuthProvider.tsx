import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { onUnauthorized } from "@/shared/api/client";
import type { LoginRequest, Session } from "@/shared/api/types";
import { authKeys, getSession, login as requestLogin, logout as requestLogout } from "./api";

interface AuthContextValue {
  session: Session | null | undefined;
  isLoading: boolean;
  error: unknown;
  login: (credentials: LoginRequest) => Promise<Session>;
  logout: () => Promise<void>;
  isLoggingIn: boolean;
  isLoggingOut: boolean;
  sessionExpired: boolean;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const [sessionExpired, setSessionExpired] = useState(false);
  const sessionQuery = useQuery({
    queryKey: authKeys.session,
    queryFn: ({ signal }) => getSession(signal),
    staleTime: 60_000,
    retry: false,
  });

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
      session: sessionQuery.data,
      isLoading: sessionQuery.isLoading,
      error: sessionQuery.error,
      login: performLogin,
      logout: performLogout,
      isLoggingIn: loginMutation.isPending,
      isLoggingOut: logoutMutation.isPending,
      sessionExpired,
    }),
    [
      sessionQuery.data,
      sessionQuery.isLoading,
      sessionQuery.error,
      performLogin,
      performLogout,
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
