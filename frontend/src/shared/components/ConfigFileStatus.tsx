import { useEffect, useReducer } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, Button } from "antd";
import { RefreshCw } from "lucide-react";
import { getSummaryPollInterval } from "@/app/query-client";
import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";
import { fetchConfigState, fetchExternalDiff, type FileSyncRequest, type OperationResult } from "@/shared/config/api";
import {
  externalWorkspaceReducer,
  initialExternalWorkspaceState,
  isExternalBannerVisible,
} from "@/shared/config/external-change";
import { configKeys } from "@/shared/config/query-keys";
import {
  createOperationId,
  restoreFilesAndSettle,
  retryPersistenceAndSettle,
  type OperationSettlement,
} from "@/shared/config/operation";
import { ExternalChangeBanner } from "./ExternalChangeBanner";
import { ExternalChangeDrawer } from "./ExternalChangeDrawer";

export function ConfigFileStatus() {
  const queryClient = useQueryClient();
  const [workspace, dispatch] = useReducer(externalWorkspaceReducer, initialExternalWorkspaceState);
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
      const settlement = kind === "restore"
        ? await restoreFilesAndSettle(request)
        : await retryPersistenceAndSettle(request);
      acceptSettlement(settlement, dispatch);
      await queryClient.invalidateQueries({ queryKey: configKeys.externalDiffs() });
      await stateQuery.refetch();
    } catch (error) {
      dispatch({ type: "failure", error });
    }
  };

  const showBanner = isExternalBannerVisible(workspace);
  return (
    <>
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
      />
    </>
  );
}

function acceptSettlement(
  settlement: OperationSettlement,
  dispatch: (action: { type: "operation"; operation: OperationResult } | { type: "failure"; error: unknown }) => void,
) {
  const operation = settlement.operation;
  if (operation.status.state === "rejected" || operation.status.state === "compensation_failed") {
    dispatch({
      type: "failure",
      error: new ApiError({ code: operation.status.error, message: operation.status.error, kind: "http" }),
    });
    return;
  }
  dispatch({ type: "operation", operation });
}
