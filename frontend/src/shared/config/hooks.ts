import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { App } from "antd";
import { ApiError } from "@/shared/api/errors";
import type { components } from "@/shared/api/generated-v2";
import { fetchConfigModule, validateCandidate, type ConfigModule, type ConfigState } from "./api";
import { invalidationKeysForChanges, configKeys } from "./query-keys";
import { applyAndSettle, createOperationId } from "./operation";

type Schemas = components["schemas"];
type ConfigChange = Schemas["ConfigChange"];
type OperationResult = Schemas["OperationResult"];

const confirmationLabels: Record<Schemas["ImpactKind"], string> = {
  rename_references: "资源改名将同步更新全部类型化引用",
  listener_rebind: "监听入口将重新绑定发生变化的 socket",
  retention_shortening: "保留范围将缩短，已清理的数据无法恢复",
  discard_external_changes: "尚未采用的外部文件变化将被覆盖",
};

export function useConfigModule(module: ConfigModule) {
  return useQuery({
    queryKey: configKeys.moduleRoot(module),
    queryFn: ({ signal }) => fetchConfigModule(module, signal),
  });
}

export function useConfigChangeMutation(module: ConfigModule) {
  const queryClient = useQueryClient();
  const { message, modal } = App.useApp();
  return useMutation({
    mutationFn: async ({ change, state }: { change: ConfigChange; state: ConfigState }): Promise<OperationResult | null> => {
      const discardExternalChanges = hasExternalFileChanges(state);
      const candidate: Schemas["Candidate"] = {
        expected: {
          active_revision: state.active_revision,
          observed_file_revision: state.observed_file_revision,
        },
        changes: [change],
        discard_external_changes: discardExternalChanges,
      };
      const validation = await validateCandidate(candidate, { module });
      if (validation.required_confirmations.length > 0) {
        const confirmed = await confirmImpacts(modal.confirm, validation.required_confirmations);
        if (!confirmed) return null;
      }
      const settlement = await applyAndSettle({
        operation_id: createOperationId(),
        candidate,
        validation_token: validation.validation_token,
        confirmations: validation.required_confirmations,
      }, { module });
      const operation = settlement.operation;
      if (settlement.kind !== "settled") {
        throw new ApiError({
          code: "INVALID_RESPONSE",
          message: "配置操作仍在进行，请根据 operation_id 查询结果。",
          kind: "invalid-response",
        });
      }
      if (operation.status.state === "rejected" || operation.status.state === "compensation_failed") {
        throw new ApiError({ code: operation.status.error, message: operation.status.error, kind: "http" });
      }
      return operation;
    },
    onSuccess: async (operation, variables) => {
      if (!operation) return;
      await Promise.all(invalidationKeysForChanges([variables.change]).map((queryKey) =>
        queryClient.invalidateQueries({ queryKey })));
      if (operation.status.state === "applied_unpersisted") {
        void message.warning("运行配置已生效，但文件尚未同步。请在全局配置提示中重试。");
      } else {
        void message.success("配置已应用并同步到文件");
      }
    },
  });
}

export function configStateEditable(state: ConfigState): boolean {
  return state.synchronization === "synced";
}

function hasExternalFileChanges(state: ConfigState): boolean {
  return state.files.source !== "unchanged"
    || (state.files.derived !== null && state.files.derived !== "unchanged");
}

function confirmImpacts(
  confirm: ReturnType<typeof App.useApp>["modal"]["confirm"],
  impacts: Schemas["ImpactKind"][],
): Promise<boolean> {
  return new Promise((resolve) => {
    confirm({
      title: "确认配置影响",
      content: impacts.map((impact) => confirmationLabels[impact]).join("；"),
      okText: "确认应用",
      cancelText: "返回编辑",
      onOk: () => resolve(true),
      onCancel: () => resolve(false),
    });
  });
}
