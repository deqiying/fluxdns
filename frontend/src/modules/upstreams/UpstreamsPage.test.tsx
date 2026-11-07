import { QueryClient } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { App } from "@/app/App";
import { AppProviders } from "@/app/providers";
import { setMockAuthenticated } from "@/mocks/handlers";
import { upstreamsConfigReadFixture } from "@/mocks/fixtures";
import { server } from "@/mocks/server";
import type { ApplyRequest, ConfigRead } from "@/shared/config/api";

const validJson = '{\n  "first.example": {"A": ["192.0.2.1"]},\n  "second.example": {"A": ["192.0.2.2"]}\n}';
const missingComma = validJson.replace(']},\n', ']}\n');

function mount(hosts = validJson) {
  const fixture: ConfigRead = structuredClone(upstreamsConfigReadFixture);
  fixture.values[0] = { module: "upstreams", value: { name: "local", type: "hosts", format: "json", hosts } };
  const requests: ApplyRequest[] = [];
  server.use(
    http.get("/api/v2/config/modules/upstreams", () => HttpResponse.json(fixture)),
    http.post("/api/v2/config/modules/upstreams/apply", async ({ request }) => {
      const body = await request.json() as ApplyRequest;
      requests.push(body);
      return HttpResponse.json({ operation_id: body.operation_id, status: { state: "applied_synced", active_revision: "a2", persisted_revision: "a2" } });
    }),
  );
  setMockAuthenticated(true);
  window.history.replaceState({}, "", "/upstreams");
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  render(<AppProviders queryClient={client}><App /></AppProviders>);
  return requests;
}

async function editLocal(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: "编辑上游 local" }, { timeout: 10_000 }));
  return screen.findByRole("dialog", { name: "编辑上游" });
}

async function selectFormat(user: ReturnType<typeof userEvent.setup>, dialog: HTMLElement, label: string) {
  await user.click(within(dialog).getByRole("combobox", { name: "Hosts 格式" }));
  await user.click(await screen.findByText(label, { selector: ".ant-select-item-option-content" }));
}

describe("上游 JSON 内联映射", () => {
  it("缺逗号时显示位置并拦截保存，修正后原样提交一次", async () => {
    const user = userEvent.setup();
    const requests = mount();
    const dialog = await editLocal(user);
    const editor = within(dialog).getByLabelText("内联映射");
    expect(within(dialog).getByText(/JSON 条目之间用英文逗号分隔/)).toBeInTheDocument();
    expect(editor).toHaveAttribute("spellcheck", "false");
    expect(editor).toHaveAttribute("placeholder", expect.stringContaining('"first.example"'));
    await user.clear(editor);
    await user.paste(missingComma);
    expect(await within(dialog).findByText(/JSON 格式错误（第 3 行，第 3 列）/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(editor).toHaveValue(missingComma);
    expect(editor).toHaveAttribute("aria-invalid", "true");
    expect(requests).toHaveLength(0);

    await user.clear(editor);
    await user.paste(validJson);
    await waitFor(() => expect(within(dialog).queryByText(/JSON 格式错误/)).not.toBeInTheDocument());
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
    expect(requests).toHaveLength(1);
    expect(requests[0].changes).toEqual([{ module: "upstreams", change: {
      action: "update", original_name: "local", value: { name: "local", type: "hosts", format: "json", hosts: validJson },
    } }]);
  });

  it("尚未编辑的非法顶层值也在保存前被拦截", async () => {
    const user = userEvent.setup();
    const requests = mount("[]");
    const dialog = await editLocal(user);
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await within(dialog).findByText("JSON 顶层必须是域名映射对象，不能是数组、null 或其他值。")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("内联映射")).toHaveValue("[]");
    expect(requests).toHaveLength(0);
  });

  it("切换格式会重新校验，Hosts 行格式不受 JSON 规则影响", async () => {
    const user = userEvent.setup();
    const requests = mount();
    const dialog = await editLocal(user);
    const editor = within(dialog).getByLabelText("内联映射");
    const hosts = "192.0.2.1 first.example\n2001:db8::1 second.example";
    await user.clear(editor);
    await user.paste(hosts);
    expect(await within(dialog).findByText(/JSON 格式错误/)).toBeInTheDocument();

    await selectFormat(user, dialog, "Hosts 行格式");
    await waitFor(() => expect(within(dialog).queryByText(/JSON 格式错误/)).not.toBeInTheDocument());
    expect(within(dialog).queryByText(/JSON 条目之间用英文逗号分隔/)).not.toBeInTheDocument();
    expect(editor).toHaveValue(hosts);
    await selectFormat(user, dialog, "JSON");
    expect(await within(dialog).findByText(/JSON 格式错误/)).toBeInTheDocument();
    expect(requests).toHaveLength(0);
    await selectFormat(user, dialog, "Hosts 行格式");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
    expect(requests).toHaveLength(1);
    expect(requests[0].changes[0]).toMatchObject({ module: "upstreams", change: { value: { format: "hosts", hosts } } });
  });
});
