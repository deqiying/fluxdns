import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { expect, it } from "vitest";
import { sessionFixture } from "@/mocks/fixtures";
import { setMockAuthenticated } from "@/mocks/handlers";
import { acceptAuthSession, reportUnauthorized } from "@/shared/api/client";
import { AuthProvider, useAuth } from "./AuthProvider";

function SessionProbe() {
  const auth = useAuth();
  return <div>{auth.session?.user.name ?? "未登录"}</div>;
}

it("认证失效取消并清除上一会话的业务数据缓存", async () => {
  setMockAuthenticated(true);
  acceptAuthSession({ session: sessionFixture, access_token: "A".repeat(43),
    token_type: "Bearer", access_expires_at_ms: Date.now() + 300_000 });
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(<MemoryRouter><QueryClientProvider client={queryClient}>
    <AuthProvider><SessionProbe /></AuthProvider>
  </QueryClientProvider></MemoryRouter>);
  await screen.findByText(sessionFixture.user.name);
  queryClient.setQueryData(["queries", "previous-session"], [{ id: "private-record" }]);
  queryClient.setQueryData(["config", "previous-session"], { value: "previous-draft" });
  act(() => reportUnauthorized());
  await waitFor(() => expect(screen.getByText("未登录")).toBeInTheDocument());
  expect(queryClient.getQueryData(["queries", "previous-session"])).toBeUndefined();
  expect(queryClient.getQueryData(["config", "previous-session"])).toBeUndefined();
});
