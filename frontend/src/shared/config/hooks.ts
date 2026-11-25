import { createElement } from "react";
import { useMutation, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { App, Button, Space } from "antd";
import { getSummaryPollInterval } from "@/app/query-client";
import { ApiError, getSafeErrorMessage, parseDiagnostic } from "@/shared/api/errors";
import { fetchConfigModule, fetchConfigState, persistConfig, reloadConfig, saveConfig, type ConfigModule, type ConfigState, type SaveRequest } from "./api";
import { configInvalidationKeys, configKeys } from "./query-keys";

type Confirm = ReturnType<typeof App.useApp>["modal"]["confirm"];

export function useConfigModule(module: ConfigModule) {
  return useQuery({
    queryKey: configKeys.moduleRoot(module),
    queryFn: ({ signal }) => fetchConfigModule(module, signal),
  });
}

/** 标题与全局提示共用同一份磁盘状态。 */
export function useConfigState() {
  return useQuery({
    queryKey: configKeys.state(),
    queryFn: ({ signal }) => fetchConfigState(signal),
    refetchInterval: () => getSummaryPollInterval(),
    refetchOnWindowFocus: "always",
  });
}

function invalidateConfig(queryClient: QueryClient, state?: ConfigState) {
  if (state) queryClient.setQueryData(configKeys.state(), state);
  return Promise.all(configInvalidationKeys.map((queryKey) => queryClient.invalidateQueries({ queryKey })));
}

function confirmAction(confirm: Confirm, title: string, content: string, okText: string, danger = false) {
  return new Promise<boolean>((resolve) => {
    confirm({ title, content, okText, cancelText: "取消", okButtonProps: { danger },
      onOk: () => resolve(true), onCancel: () => resolve(false), afterClose: () => resolve(false) });
  });
}

function confirmOverwrite(confirm: Confirm, okText: string) {
  return confirmAction(confirm, "确认覆盖磁盘配置？", "磁盘上的外部修改将被丢弃，无法恢复。", okText, true);
}

function chooseDiskConflict(confirm: Confirm, disk?: ConfigState["disk"]) {
  return new Promise<"overwrite" | "reload" | "cancel">((resolve) => {
    const content = disk?.status === "missing" ? "配置文件不存在。"
      : disk?.status === "unreadable" ? "配置文件无法读取。" : "磁盘配置与当前运行配置不一致。";
    const dialog = confirm({
      title: "磁盘配置冲突",
      content: `${content}当前草稿已保留。`,
      okText: "重新加载后继续编辑",
      cancelText: "取消",
      onOk: () => resolve("reload"),
      onCancel: () => resolve("cancel"),
      afterClose: () => resolve("cancel"),
      footer: (_, { CancelBtn, OkBtn }) => createElement(Space, { wrap: true },
        createElement(CancelBtn),
        createElement(Button, { danger: true, onClick: () => { resolve("overwrite"); dialog.destroy(); } }, "覆盖磁盘并保存"),
        createElement(OkBtn)),
    });
  });
}

function isUncertain(error: unknown) {
  return error instanceof ApiError && (error.kind === "network" || error.kind === "timeout" || error.kind === "invalid-response");
}

function useConfigWriteFeedback() {
  const queryClient = useQueryClient();
  const { message, modal } = App.useApp();

  const showUncertain = () => {
    void message.warning("结果未确认，请核对重新读取的配置后再操作。");
    // HTTP 断开不终止后端工作；只回读，不重发写入。
    void invalidateConfig(queryClient);
  };

  const runDiskAction = async (action: "reload" | "persist") => {
    try {
      const { state } = await (action === "reload" ? reloadConfig() : persistConfig());
      void invalidateConfig(queryClient, state);
      void message.success(action === "reload" ? "已重新加载磁盘配置" : "已写入磁盘");
    } catch (error) {
      if (isUncertain(error)) showUncertain();
      else if (error instanceof ApiError && error.code === "OPERATION_BUSY") {
        void message.warning(getSafeErrorMessage(error));
      } else {
        const detail = getSafeErrorMessage(error);
        const unchanged = error instanceof ApiError && ["CONFIG_FILE_UNREADABLE", "VALIDATION_FAILED", "APPLY_FAILED"].includes(error.code);
        void message.error(action === "reload" ? `重新加载失败：${detail}${unchanged ? "；运行配置未更改。" : ""}` : detail);
      }
    }
  };

  return { queryClient, message, modal, showUncertain, runDiskAction };
}

/** 全局磁盘操作使用同一刷新与错误处理，覆盖写入必须经用户确认。 */
export function useConfigDiskAction() {
  const { modal, runDiskAction } = useConfigWriteFeedback();
  return useMutation({
    retry: false,
    networkMode: "always",
    mutationFn: async (action: "reload" | "persist") => {
      if (action === "persist" && !(await confirmOverwrite(modal.confirm, "确认写入磁盘"))) return;
      await runDiskAction(action);
    },
  });
}

/** 保存只在服务端明确要求且用户确认后重发，其他失败均保留草稿。 */
export function useConfigSave() {
  const { queryClient, message, modal, showUncertain, runDiskAction } = useConfigWriteFeedback();
  const { mutate, isPending, error, reset } = useMutation({
    retry: false,
    // 离线时立即尝试并报告结果，不让请求在恢复连接后自动提交。
    networkMode: "always",
    mutationFn: async (changes: SaveRequest["changes"]) => {
      let request: SaveRequest = { changes };
      for (;;) {
        try {
          return await saveConfig(request);
        } catch (error) {
          if (isUncertain(error)) {
            showUncertain();
            return;
          }
          if (!(error instanceof ApiError)) throw error;
          if (error.code === "CONFIG_FILE_CHANGED" && !request.overwrite_disk) {
            const choice = await chooseDiskConflict(modal.confirm, error.disk);
            if (choice === "reload") await runDiskAction("reload");
            if (choice !== "overwrite" || !(await confirmOverwrite(modal.confirm, "确认覆盖并保存"))) return;
            request = { ...request, overwrite_disk: true };
          } else if (error.code === "CONFIRMATION_REQUIRED" && !request.confirm_retention_shortening) {
            const confirmed = await confirmAction(modal.confirm, "确认缩短统计保留期？",
              "保留范围将缩短，已清理的数据无法恢复。", "确认缩短");
            if (!confirmed) return;
            request = { ...request, confirm_retention_shortening: true };
          } else if (error.code === "OPERATION_BUSY") {
            void message.warning(getSafeErrorMessage(error));
            return;
          } else {
            throw error;
          }
        }
      }
    },
    onSuccess: (result) => {
      if (!result) return;
      void invalidateConfig(queryClient, result.state);
      if (result.persisted) void message.success("配置已应用并同步到文件");
      else {
        const diagnostic = parseDiagnostic(result.diagnostic);
        void message.warning(`已生效但未写入磁盘，重启后会丢失${diagnostic ? `；${diagnostic.message}` : ""}`);
      }
    },
  });

  return { isPending, error, reset, save: (change: SaveRequest["changes"][number], onSaved: () => void) => {
    if (!isPending) mutate([change], { onSuccess: (result) => { if (result) onSaved(); } });
  } };
}
