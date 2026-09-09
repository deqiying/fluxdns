import { ApiError } from "@/shared/api/errors";
import { acceptAuthSession, apiRequest, clearAccessSession } from "@/shared/api/client";
import type { AuthSession, LoginRequest, Session, SetupRequest, SetupStatus } from "@/shared/api/types";

export const authKeys = {
  all: ["api", "v2", "auth"] as const,
  setup: ["api", "v2", "auth", "setup"] as const,
  session: ["api", "v2", "auth", "session"] as const,
};

export function getSetupStatus(signal?: AbortSignal): Promise<SetupStatus> {
  return apiRequest<SetupStatus>("/auth/setup", {
    signal,
    handleUnauthorized: false,
    auth: "public",
  });
}

export async function getSession(signal?: AbortSignal): Promise<Session | null> {
  try {
    return await apiRequest<Session>("/auth/session", {
      signal,
      handleUnauthorized: false,
    });
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) {
      return null;
    }
    throw error;
  }
}

export async function login(credentials: LoginRequest): Promise<Session> {
  clearAccessSession();
  const response = await apiRequest<AuthSession>("/auth/login", {
    method: "POST",
    body: credentials,
    handleUnauthorized: false,
    auth: "public",
  });
  return acceptAuthSession(response);
}

export async function initializeWebUi(credentials: SetupRequest): Promise<Session> {
  clearAccessSession();
  const response = await apiRequest<AuthSession>("/auth/setup", {
    method: "POST",
    body: credentials,
    handleUnauthorized: false,
    auth: "public",
  });
  return acceptAuthSession(response);
}

export async function logout(): Promise<void> {
  try {
    await apiRequest<void>("/auth/logout", { method: "POST", auth: "logout" });
  } finally {
    clearAccessSession();
  }
}
