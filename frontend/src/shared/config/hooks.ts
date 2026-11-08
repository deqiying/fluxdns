import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { App } from "antd";
import { getSummaryPollInterval } from "@/app/query-client";
import { ApiError } from "@/shared/api/errors";
import type { components } from "@/shared/api/generated-v2";
import { fetchConfigModule, fetchConfigState, type ConfigModule, type ConfigState } from "./api";
import { invalidationKeysForChanges, configKeys } from "./query-keys";
import { applyWithConfirmation, createOperationId, getOperationMessage, operationError } from "./operation";
import { assertNoPendingOperation, trackConfigOperation } from "./pending-operation";

type Schemas = components["schemas"];
type ConfigChange = Schemas["ConfigChange"];
type OperationResult = Schemas["OperationResult"];

export const confirmationLabels: Record<Schemas["ImpactKind"], string> = {
  retention_shortening: "保留范围将缩短，已清理的数据无法恢复",
  discard_external_changes: "尚未采用的外部文件变化将被覆盖",
};

export function useConfigModule(module: ConfigModule) {
  return useQuery({
    queryKey: configKeys.moduleRoot(module),
    queryFn: ({ signal }) => fetchConfigModule(module, signal),
  });
}

/** 标题区只消费全局配置同步状态；与全局提示共用 30 秒轮询和同一 query key，避免同一页出现互相矛盾的同步结论。 */
export function useConfigState() {
  return useQuery({
    queryKey: configKeys.state(),
    queryFn: ({ signal }) => fetchConfigState(signal),
    refetchInterval: () => getSummaryPollInterval(),
  });
}

export function useConfigChangeMutation(module: ConfigModule) {
  const queryClient = useQueryClient();
  const { message, modal } = App.useApp();
  return useMutation({
    mutationFn: async ({ change, confirmationDetails }: { change: ConfigChange; confirmationDetails?: Partial<Record<Schemas["ImpactKind"], string>> }): Promise<OperationResult | null> => {
      assertNoPendingOperation(queryClient);
      const readError = queryClient.getQueryState(configKeys.moduleRoot(module))?.error
        ?? queryClient.getQueryState(configKeys.state())?.error;
      if (readError) throw new ApiError({ code: "CONFIG_READ_FAILED", message: "配置读取失败，请刷新后保存。", kind: "http", cause: readError,
        ...(readError instanceof ApiError ? { requestId: readError.requestId, diagnostic: readError.diagnostic, serverMessage: readError.serverMessage, fieldErrors: readError.fieldErrors } : {}) });
      const settlement = await trackConfigOperation(queryClient, () => applyWithConfirmation({
        operation_id: createOperationId(),
        changes: [change],
      }, (validation) => confirmImpacts(modal.confirm, validation.required_confirmations, confirmationDetails), { module }));
      if (!settlement) return null;
      const operation = settlement.operation;
      if (operation.status.state === "rejected" || operation.status.state === "compensation_failed") {
        throw operationError(operation);
      }
      return operation;
    },
    onSuccess: async (operation, variables) => {
      if (!operation) return;
      await Promise.all(invalidationKeysForChanges([variables.change]).map((queryKey) =>
        queryClient.invalidateQueries({ queryKey })));
      if (operation.status.state === "applied_unpersisted") {
        void message.warning(getOperationMessage(operation));
      } else {
        void message.success("配置已应用并同步到文件");
      }
    },
  });
}

export function configStateEditable(state: ConfigState): boolean {
  return state.synchronization === "synced";
}

export function confirmImpacts(
  confirm: ReturnType<typeof App.useApp>["modal"]["confirm"],
  impacts: Schemas["ImpactKind"][],
  details?: Partial<Record<Schemas["ImpactKind"], string>>,
): Promise<boolean> {
  return new Promise((resolve) => {
    confirm({
      title: "确认配置影响",
      content: impacts.map((impact) => details?.[impact] ?? confirmationLabels[impact]).join("；"),
      okText: "确认应用",
      cancelText: "返回编辑",
      onOk: () => resolve(true),
      onCancel: () => resolve(false),
    });
  });
}
