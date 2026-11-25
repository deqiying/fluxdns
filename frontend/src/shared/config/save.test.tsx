import { useState } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { App, ConfigProvider, Form, Input } from "antd";
import { http, HttpResponse } from "msw";
import { beforeEach, expect, it } from "vitest";
import { configStateFixture, logsConfigReadFixture, systemConfigReadFixture } from "@/mocks/fixtures";
import { setMockAuthenticated } from "@/mocks/handlers";
import { server } from "@/mocks/server";
import type { ErrorEnvelope } from "@/shared/api/types";
import { ConfigFormModal } from "@/shared/components/ConfigFormModal";
import { DiskStatusBanner } from "@/shared/components/DiskStatusBanner";
import type { ConfigState, SaveRequest } from "./api";
import { useConfigModule, useConfigSave, useConfigState } from "./hooks";
import { configKeys } from "./query-keys";

const logs = logsConfigReadFixture.values[0].value;
const changedState: ConfigState = { config_digest: "b".repeat(64), runtime_revision: "13", disk: { status: "in_sync" } };

function rejected(status: number, code: ErrorEnvelope["code"], fields: Partial<ErrorEnvelope> = {}) {
  return HttpResponse.json({ code, request_id: "save-test", message: "request could not be completed", retryable: false, field_errors: [], ...fields }, { status });
}

function Editor() {
  const [open, setOpen] = useState(true);
  const [form] = Form.useForm<typeof logs>();
  const config = useConfigModule("logs");
  const state = useConfigState();
  const mutation = useConfigSave();
  const value = config.data?.values.find((item) => item.module === "logs")?.value;
  if (!value) return null;
  return (
    <ConfigFormModal open={open} title="测试配置编辑" dirty busy={mutation.isPending} error={mutation.error}
      form={form} module="logs" configDigest={state.data?.config_digest} moduleDigest={config.data?.state.config_digest}
      onCancel={() => setOpen(false)} onSubmit={() => form.submit()}>
      <Form form={form} initialValues={value} onFinish={({ path }) => mutation.save({ module: "logs", change: { ...logs, path } }, () => setOpen(false))}>
        <Form.Item name="path" label="日志路径"><Input /></Form.Item>
      </Form>
    </ConfigFormModal>
  );
}

async function edit() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  client.setQueryData(configKeys.system, systemConfigReadFixture);
  const user = userEvent.setup();
  render(<QueryClientProvider client={client}><ConfigProvider theme={{ token: { motion: false } }}><App><DiskStatusBanner /><Editor /></App></ConfigProvider></QueryClientProvider>);
  const editor = await screen.findByRole("dialog", { name: "测试配置编辑" });
  const field = within(editor).getByLabelText("日志路径");
  await user.clear(field);
  await user.type(field, "draft.log");
  const save = within(editor).getByRole("button", { name: "保存" });
  return { client, user, editor, field, save };
}

function captureSaves(result: (request: SaveRequest) => Response) {
  const requests: SaveRequest[] = [];
  server.use(http.post("/api/v2/config/save", async ({ request }) => {
    const body = await request.json() as SaveRequest;
    requests.push(body);
    return result(body);
  }));
  return requests;
}

beforeEach(() => setMockAuthenticated(true));

it.each([true, false])("persisted=%s 关闭弹窗、提示并失效全部配置查询", async (persisted) => {
  const nextState: ConfigState = { ...changedState, disk: { status: persisted ? "in_sync" : "modified" } };
  let state = configStateFixture;
  server.use(http.get("/api/v2/config/state", () => HttpResponse.json(state)));
  const requests = captureSaves(() => {
    state = nextState;
    return HttpResponse.json({ state, persisted });
  });
  const { client, user, editor, save } = await edit();
  const otherModule = configKeys.moduleRoot("strategy");
  client.setQueryData(otherModule, { previouslyLoaded: true });
  await user.click(save);
  expect(await screen.findByText(persisted ? "配置已应用并同步到文件" : "已生效但未写入磁盘，重启后会丢失")).toBeInTheDocument();
  await waitFor(() => expect(editor).not.toBeVisible(), { timeout: 5_000 });
  expect(requests).toEqual([{ changes: [{ module: "logs", change: { ...logs, path: "draft.log" } }] }]);
  expect(client.getQueryState(configKeys.system)?.isInvalidated).toBe(true);
  expect(client.getQueryState(otherModule)?.isInvalidated).toBe(true);
  if (!persisted) expect(await screen.findByRole("button", { name: "写入磁盘" })).toBeEnabled();
});

it.each(["覆盖磁盘并保存", "重新加载后继续编辑", "取消"] as const)("磁盘冲突：%s", async (choice) => {
  let reloaded = false;
  let moduleReads = 0;
  server.use(
    http.get("/api/v2/config/state", () => HttpResponse.json(reloaded ? changedState : configStateFixture)),
    http.get("/api/v2/config/modules/logs", () => {
      moduleReads += 1;
      return HttpResponse.json(reloaded ? { ...logsConfigReadFixture, state: changedState, values: [{ module: "logs", value: { ...logs, path: "disk.log" } }] } : logsConfigReadFixture);
    }),
    http.post("/api/v2/config/reload", async ({ request }) => {
      expect(await request.text()).toBe("");
      reloaded = true;
      return HttpResponse.json({ state: changedState });
    }),
  );
  const requests = captureSaves((body) => reloaded || body.overwrite_disk
    ? HttpResponse.json({ state: changedState, persisted: true })
    : rejected(409, "CONFIG_FILE_CHANGED", { disk: { status: "modified" } }));
  const { client, user, editor, field, save } = await edit();
  await user.click(save);
  const title = await screen.findByText("磁盘配置冲突", { selector: ".ant-modal-confirm-title" });
  const conflict = within(title.closest<HTMLElement>(".ant-modal")!);
  expect(save).toBeDisabled();
  expect(requests).toHaveLength(1);
  for (const option of ["覆盖磁盘并保存", "重新加载后继续编辑", /取\s*消/]) {
    expect(conflict.getByRole("button", { name: option })).toBeInTheDocument();
  }
  await user.click(conflict.getByRole("button", { name: choice === "取消" ? /取\s*消/ : choice }));

  if (choice === "覆盖磁盘并保存") {
    expect(await screen.findByText(/磁盘上的外部修改将被丢弃/)).toBeInTheDocument();
    expect(requests).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "确认覆盖并保存" }));
    expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
    expect(requests).toEqual([requests[0], { ...requests[0], overwrite_disk: true }]);
    expect(reloaded).toBe(false);
  } else {
    await waitFor(() => expect(save).toBeEnabled());
    expect(field).toHaveValue("draft.log");
    expect(editor).toBeVisible();
    expect(requests).toHaveLength(1);
    if (choice === "重新加载后继续编辑") {
      expect(await within(editor).findByText("配置已更新，保存时将以当前内容替换本条目")).toBeInTheDocument();
      await waitFor(() => expect(moduleReads).toBe(2));
      expect(client.getQueryState(configKeys.system)?.isInvalidated).toBe(true);
      await user.click(save);
      expect(await screen.findByText("配置已应用并同步到文件")).toBeInTheDocument();
      expect(requests).toEqual([requests[0], requests[0]]);
    } else {
      expect(reloaded).toBe(false);
      expect(moduleReads).toBe(1);
    }
  }
});

it("冲突中的重新加载失败说明运行配置未更改并保留草稿", async () => {
  const requests = captureSaves(() => rejected(409, "CONFIG_FILE_CHANGED"));
  server.use(http.post("/api/v2/config/reload", () => rejected(422, "VALIDATION_FAILED", {
    diagnostic: { stage: "validate", code: "CONFIG_INVALID", message: "磁盘配置无效" },
  })));
  const { user, field, save } = await edit();
  await user.click(save);
  await user.click(await screen.findByRole("button", { name: "重新加载后继续编辑" }));
  expect(await screen.findByText(/重新加载失败：.*磁盘配置无效.*运行配置未更改/)).toBeInTheDocument();
  expect(field).toHaveValue("draft.log");
  expect(requests).toHaveLength(1);
});

it("忙时提示稍后重试且不重新提交", async () => {
  const requests = captureSaves(() => rejected(409, "OPERATION_BUSY", { retryable: true }));
  const { user, save, field } = await edit();
  await user.click(save);
  expect(await screen.findByText(/稍后重试/)).toBeInTheDocument();
  await waitFor(() => expect(save).toBeEnabled());
  expect(field).toHaveValue("draft.log");
  expect(requests).toHaveLength(1);
});

it.each([[422, "VALIDATION_FAILED", "logs.path"], [400, "INVALID_ARGUMENT", "changes[0].change.path"]] as const)("%s 字段错误内联，未匹配路径显示在顶部", async (status, code, path) => {
  captureSaves(() => rejected(status, code, { field_errors: [{ path, code }, { path: "other.path", code }] }));
  const { user, save, editor, field } = await edit();
  await user.click(save);
  expect(await within(editor).findByText(code)).toBeInTheDocument();
  expect(field).toHaveAttribute("aria-invalid", "true");
  const alert = within(editor).getByRole("alert");
  expect(alert).toHaveTextContent(`字段 other.path：${code}`);
  expect(alert).not.toHaveTextContent(`字段 ${path}：${code}`);
  expect(field).toHaveValue("draft.log");
});

it("APPLY_FAILED 在顶部显示诊断并保留草稿", async () => {
  captureSaves(() => rejected(422, "APPLY_FAILED", { diagnostic: { stage: "prepare", code: "BIND_FAILED", message: "端口无法绑定" } }));
  const { user, save, editor, field } = await edit();
  await user.click(save);
  expect(await within(editor).findByRole("alert")).toHaveTextContent(/运行状态保持不变.*端口无法绑定.*BIND_FAILED/);
  expect(field).toHaveValue("draft.log");
});

it("网络结果未确认只回读状态和模块，不重发写入", async () => {
  let stateReads = 0;
  let moduleReads = 0;
  server.use(
    http.get("/api/v2/config/state", () => { stateReads += 1; return HttpResponse.json(configStateFixture); }),
    http.get("/api/v2/config/modules/logs", () => { moduleReads += 1; return HttpResponse.json(logsConfigReadFixture); }),
  );
  const requests = captureSaves(() => HttpResponse.error());
  const { user, save, field } = await edit();
  await user.click(save);
  expect(await screen.findByText(/结果未确认/)).toBeInTheDocument();
  await waitFor(() => expect([stateReads, moduleReads]).toEqual([2, 2]));
  expect(requests).toHaveLength(1);
  expect(field).toHaveValue("draft.log");
});
