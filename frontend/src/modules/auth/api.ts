import { ApiError } from "@/shared/api/errors";
import { apiRequest } from "@/shared/api/client";
import type { LoginRequest, Session } from "@/shared/api/types";

export const authKeys = {
  all: ["api", "v1", "auth"] as const,
  session: ["api", "v1", "auth", "session"] as const,
};

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

export function logout(): Promise<void> {
  return apiRequest<void>("/auth/logout", { method: "POST" });
}
