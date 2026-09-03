import { ApiError } from "@/shared/api/errors";
import { apiRequest } from "@/shared/api/client";
import type { LoginRequest, Session, SetupRequest, SetupStatus } from "@/shared/api/types";

export const authKeys = {
  all: ["api", "v1", "auth"] as const,
  setup: ["api", "v1", "auth", "setup"] as const,
  session: ["api", "v1", "auth", "session"] as const,
};

export function getSetupStatus(signal?: AbortSignal): Promise<SetupStatus> {
  return apiRequest<SetupStatus>("/auth/setup", {
    signal,
    handleUnauthorized: false,
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

export function login(credentials: LoginRequest): Promise<Session> {
  return apiRequest<Session>("/auth/login", {
    method: "POST",
    body: credentials,
    handleUnauthorized: false,
  });
}

export function initializeWebUi(credentials: SetupRequest): Promise<Session> {
  return apiRequest<Session>("/auth/setup", {
    method: "POST",
    body: credentials,
    handleUnauthorized: false,
  });
}

export function logout(): Promise<void> {
  return apiRequest<void>("/auth/logout", { method: "POST" });
}
