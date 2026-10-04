import { useEffect, useReducer, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, App, Button, Space, Typography } from "antd";
import { RefreshCw } from "lucide-react";
import { getSummaryPollInterval } from "@/app/query-client";
import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";
import { fetchConfigState, fetchExternalDiff, validateCandidate, type FileSyncRequest, type OperationResult } from "@/shared/config/api";
import type { ConfigChange } from "@/shared/config/contract";
import {
  externalWorkspaceReducer,
  initialExternalWorkspaceState,
  isExternalBannerVisible,
} from "@/shared/config/external-change";
import { configKeys, invalidationKeysForChanges } from "@/shared/config/query-keys";
import { confirmationLabels } from "@/shared/config/hooks";
import { assertNoPendingOperation, pendingOperationKey, trackConfigOperation } from "@/shared/config/pending-operation";
import {
  applyAndSettle, createOperationId,
  restoreFilesAndSettle,
  retryPersistenceAndSettle,
  resumeOperation,
  type OperationSettlement,
} from "@/shared/config/operation";
import { ExternalChangeBanner } from "./ExternalChangeBanner";
import { ExternalChangeDrawer } from "./ExternalChangeDrawer";

export function ConfigFileStatus() {
  const queryClient = useQueryClient();
  const { message, modal } = App.useApp();
  const [workspace, dispatch] = useReducer(externalWorkspaceReducer, initialExternalWorkspaceState);
  const [resuming, setResuming] = useState(false);
  const pendingQuery = useQuery<string | null>({
    queryKey: pendingOperationKey, queryFn: () => null, initialData: null, enabled: false, gcTime: Infinity,
  });
  const continueOperation = async () => {
    const id = pendingQuery.data;
    if (!id || resuming) return;
    setResuming(true);
    try {
      const settlement = await trackConfigOperation(queryClient, () => resumeOperation(id));
      queryClient.setQueryData(pendingOperationKey, null);
      acceptSettlement(settlement, dispatch);
      await queryClient.invalidateQueries({ queryKey: ["config-v2"] });
      const status = settlement.operation.status;
      if (status.state === "rejected" || status.state === "compensation_failed") {
        void message.error(getSafeErrorMessage(new ApiError({ code: status.error, message: status.error, kind: "http" })));
      } else {
        void message.success("已确认配置操作结果，请刷新并核对配置后继续编辑。");
      }
    } catch (error) {
      void message.warning(getSafeErrorMessage(error));
    } finally { setResuming(false); }
  };
  const stateQuery = useQuery({
    queryKey: configKeys.state(),
    queryFn: ({ signal }) => fetchConfigState(signal),
    refetchInterval: () => getSummaryPollInterval(),
  });

  useEffect(() => {
    if (stateQuery.data) dispatch({ type: "snapshot", state: stateQuery.data });
  }, [stateQuery.data]);

  useEffect(() => {
    const issue = workspace.issue;
    if (!workspace.open || workspace.phase !== "loading" || !issue) return;
    let active = true;
    void queryClient.fetchQuery({
      queryKey: configKeys.externalDiff(issue.activeRevision, issue.fileRevision),
      queryFn: ({ signal }) => fetchExternalDiff(signal),
      staleTime: 0,
    }).then(
      (diff) => { if (active) dispatch({ type: "loaded", diff }); },
      (error: unknown) => { if (active) dispatch({ type: "failure", error }); },
    );
    return () => { active = false; };
  }, [queryClient, workspace.issue, workspace.open, workspace.phase]);

  const runFileMutation = async (kind: "restore" | "retry") => {
    const issue = workspace.issue;
    if (!issue) return;
    const operationId = kind === "restore" ? createOperationId() : issue.operationId;
    if (!operationId) {
      dispatch({
        type: "failure",
        error: new ApiError({ code: "INVALID_RESPONSE", message: "missing operation id", kind: "invalid-response" }),
      });
      return;
    }

    const request: FileSyncRequest = {
      operation_id: operationId,
      expected: {
        active_revision: issue.activeRevision,
        observed_file_revision: issue.fileRevision,
      },
      discard_external_changes: kind === "restore",
    };

    dispatch({ type: "restore" });
    try {
      assertNoPendingOperation(queryClient);
      const settlement = await trackConfigOperation(queryClient, () => kind === "restore"
        ? restoreFilesAndSettle(request)
        : retryPersistenceAndSettle(request));
      acceptSettlement(settlement, dispatch);
      await queryClient.invalidateQueries({ queryKey: configKeys.externalDiffs() });
      await stateQuery.refetch();
    } catch (error) {
      dispatch({ type: "failure", error });
    }
  };

  const runAdoption = async (changes: ConfigChange[]) => {
    const diff = workspace.diff;
    if (!diff || changes.length === 0) return;
    const candidate = {
      expected: diff.expected,
      changes,
      discard_external_changes: true,
    };
    try {
      assertNoPendingOperation(queryClient);
      const validation = await validateCandidate(candidate);
      const omitted = diff.editable.length - changes.length;
      const confirmed = await new Promise<boolean>((resolve) => {
        modal.confirm({
          title: "确认组合采用外部配置？",
          content: (
            <Space orientation="vertical" size={8}>
              <Typography.Text>将一次应用 {changes.length} 项类型化变化。</Typography.Text>
              <Typography.Text type="secondary">未采用可编辑差异 {omitted} 项，受保护变化 {diff.protected_changes.length} 类；这些内容会被当前活动配置还原。</Typography.Text>
              {validation.required_confirmations.length > 0 ? <Typography.Text type="warning">{validation.required_confirmations.map((impact) => confirmationLabels[impact]).join("；")}</Typography.Text> : null}
            </Space>
          ),
          okText: "确认采用",
          cancelText: "返回差异",
          onOk: () => resolve(true),
          onCancel: () => resolve(false),
        });
      });
      if (!confirmed) return;
      dispatch({ type: "apply" });
      const settlement = await trackConfigOperation(queryClient, () => applyAndSettle({
        operation_id: createOperationId(),
        candidate,
        validation_token: validation.validation_token,
        confirmations: validation.required_confirmations,
      }));
      if (!acceptSettlement(settlement, dispatch)) return;
      await Promise.all(invalidationKeysForChanges(changes).map((queryKey) => queryClient.invalidateQueries({ queryKey })));
      await queryClient.invalidateQueries({ queryKey: configKeys.externalDiffs() });
      await stateQuery.refetch();
      if (settlement.operation.status.state === "applied_unpersisted") {
        void message.warning("组合配置已热应用，但文件尚未同步。");
      } else if (settlement.operation.status.state === "applied_synced") {
        void message.success("外部配置已组合采用并同步");
      }
    } catch (error) {
      dispatch({ type: "failure", error });
    }
  };

  const showBanner = isExternalBannerVisible(workspace);
  return (
    <>
      {pendingQuery.data ? <div className="global-config-status" aria-live="polite"><Alert type="warning" showIcon
        message="配置操作结果尚未确认"
        description="已保留原操作，请继续查询。确认结果前不能提交新的配置变更。"
        action={<Button size="small" loading={resuming} onClick={() => void continueOperation()}>继续查询</Button>}
      /></div> : null}
      {stateQuery.isError || showBanner ? (
        <div className="global-config-status" aria-live="polite">
          {stateQuery.isError ? (
            <Alert
              type="warning"
              showIcon
              message="无法刷新配置文件状态"
              description={getSafeErrorMessage(stateQuery.error)}
              action={(
                <Button
                  size="small"
                  icon={<RefreshCw size={15} aria-hidden="true" />}
                  loading={stateQuery.isFetching}
                  onClick={() => void stateQuery.refetch()}
                >
                  重试
                </Button>
              )}
            />
          ) : null}
          {showBanner && workspace.issue ? (
            <ExternalChangeBanner
              issue={workspace.issue}
              onOpen={() => dispatch({ type: "open" })}
              onDismiss={() => dispatch({ type: "dismiss" })}
            />
          ) : null}
        </div>
      ) : null}
      <ExternalChangeDrawer
        state={workspace}
        onClose={() => dispatch({ type: "close" })}
        onRestore={() => void runFileMutation("restore")}
        onRetryPersistence={workspace.issue?.operationId ? () => void runFileMutation("retry") : undefined}
        onAdopt={workspace.issue?.kind === "files_changed" ? (changes) => void runAdoption(changes) : undefined}
        onDirty={(dirty) => dispatch({ type: "dirty", value: dirty })}
      />
    </>
  );
}

function acceptSettlement(
  settlement: OperationSettlement,
  dispatch: (action: { type: "operation"; operation: OperationResult } | { type: "failure"; error: unknown }) => void,
): boolean {
  const operation = settlement.operation;
  if (operation.status.state === "rejected" || operation.status.state === "compensation_failed") {
    dispatch({
      type: "failure",
      error: new ApiError({ code: operation.status.error, message: operation.status.error, kind: "http" }),
    });
    return false;
  }
  dispatch({ type: "operation", operation });
  return true;
}
