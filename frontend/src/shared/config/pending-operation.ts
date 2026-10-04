import type { QueryClient } from "@tanstack/react-query";
import { PendingOperationError, type OperationSettlement } from "./operation";

// 与服务端配置快照分开保存，后台刷新和页面切换都不能清除未确认的操作。
export const pendingOperationKey = ["config-pending-operation"] as const;

export function assertNoPendingOperation(client: QueryClient): void {
  const id = client.getQueryData<string | null>(pendingOperationKey);
  if (id) throw new PendingOperationError(id);
}

/** 所有写入口共用未决操作记录；继续查询只使用原 ID。 */
export async function trackConfigOperation(
  client: QueryClient,
  run: () => Promise<OperationSettlement>,
): Promise<OperationSettlement> {
  try {
    const settlement = await run();
    if (settlement.kind !== "settled") throw new PendingOperationError(settlement.operation.operation_id);
    return settlement;
  } catch (error) {
    if (error instanceof PendingOperationError) client.setQueryData(pendingOperationKey, error.operationId);
    throw error;
  }
}
