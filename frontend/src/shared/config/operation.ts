import { ApiError } from "@/shared/api/errors";
import {
  applyCandidate,
  fetchConfigOperation,
  fetchConfigState,
  restoreConfigFiles,
  retryConfigPersistence,
  type ApplyRequest,
  type ConfigModule,
  type ConfigState,
  type FileSyncRequest,
  type OperationResult,
  type ValidationResult,
} from "./api";
import type { ConfigDraft, FormPhase } from "./contract";
import { isTransientConfigRead } from "./read-retry";

export type OperationSettlement =
  | { kind: "settled"; operation: OperationResult }
  | { kind: "pending"; operation: OperationResult }
  | { kind: "unknown"; operation: OperationResult; state: ConfigState };

export interface ApplyAndSettleOptions {
  module?: ConfigModule;
  signal?: AbortSignal;
  pollIntervalMs?: number;
  maxPolls?: number;
}

/** 保存结果未决时保留原 ID；页面只能继续查询，不能把它当成可重新提交的失败。 */
export class PendingOperationError extends ApiError {
  constructor(readonly operationId: string, cause?: unknown) {
    super({ code: "CONFIG_OPERATION_PENDING", message: "配置操作结果尚未确认，请继续查询。", kind: "http", cause });
  }
}

export function createOperationId(): string {
  return crypto.randomUUID();
}

export function beginValidation(draft: ConfigDraft): FormPhase {
  return { kind: "validating", draft };
}

export function phaseAfterValidation(
  draft: ConfigDraft,
  validation: ValidationResult,
): FormPhase {
  return { kind: "confirming", draft, validation };
}

export function phaseAfterSettlement(draft: ConfigDraft, settlement: OperationSettlement): FormPhase {
  if (settlement.kind === "settled") return { kind: "settled", draft, operation: settlement.operation };
  return { kind: "result_unknown", draft, operationId: settlement.operation.operation_id };
}

/** apply 只发送一次；timeout/network 后使用同一 operation_id 回读，不重放变更。 */
export async function applyAndSettle(
  request: ApplyRequest,
  options: ApplyAndSettleOptions = {},
): Promise<OperationSettlement> {
  let operation: OperationResult;
  try {
    operation = await applyCandidate(request, { module: options.module, signal: options.signal });
  } catch (error) {
    if (!(error instanceof ApiError) || (error.kind !== "timeout" && error.kind !== "network")) throw error;
    return resumeOperation(request.operation_id, options);
  }
  assertOperationId(request.operation_id, operation);
  return settleOperation(operation, options);
}

export function restoreFilesAndSettle(
  request: FileSyncRequest,
  options: Omit<ApplyAndSettleOptions, "module"> = {},
): Promise<OperationSettlement> {
  return runFileMutationAndSettle(restoreConfigFiles, request, options);
}

export function retryPersistenceAndSettle(
  request: FileSyncRequest,
  options: Omit<ApplyAndSettleOptions, "module"> = {},
): Promise<OperationSettlement> {
  return runFileMutationAndSettle(retryConfigPersistence, request, options);
}

export async function settleOperation(
  initial: OperationResult,
  options: Omit<ApplyAndSettleOptions, "module"> = {},
): Promise<OperationSettlement> {
  const maxPolls = options.maxPolls ?? 20;
  const pollIntervalMs = options.pollIntervalMs ?? 250;
  let operation = initial;

  for (let poll = 0; isInProgress(operation) && poll < maxPolls; poll += 1) {
    await waitForPoll(pollIntervalMs, options.signal);
    try {
      const next = await fetchConfigOperation(operation.operation_id, options.signal);
      assertOperationId(operation.operation_id, next);
      operation = next;
    } catch (error) {
      // 短暂读取失败消耗原轮询预算，不能重置计数或重新提交操作。
      if (!isTransientConfigRead(error)) throw new PendingOperationError(operation.operation_id, error);
    }
  }

  if (operation.status.state === "unknown") {
    try { return { kind: "unknown", operation, state: await fetchConfigState(options.signal) }; }
    catch { throw new PendingOperationError(operation.operation_id); }
  }
  if (isInProgress(operation)) return { kind: "pending", operation };
  return { kind: "settled", operation };
}

function isInProgress(operation: OperationResult): boolean {
  return operation.status.state === "preparing"
    || operation.status.state === "applying"
    || operation.status.state === "persisting";
}

async function runFileMutationAndSettle(
  mutation: (request: FileSyncRequest, signal?: AbortSignal) => Promise<OperationResult>,
  request: FileSyncRequest,
  options: Omit<ApplyAndSettleOptions, "module">,
): Promise<OperationSettlement> {
  let operation: OperationResult;
  try {
    operation = await mutation(request, options.signal);
  } catch (error) {
    if (!(error instanceof ApiError) || (error.kind !== "timeout" && error.kind !== "network")) throw error;
    return resumeOperation(request.operation_id, options);
  }
  assertOperationId(request.operation_id, operation);
  return settleOperation(operation, options);
}

/** 恢复查询与随后轮询共用预算；没有成功快照时仍将原操作 ID 交回页面。 */
export async function resumeOperation(operationId: string, options: ApplyAndSettleOptions = {}): Promise<OperationSettlement> {
  const attempts = Math.max(0, options.maxPolls ?? 20);
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    let operation: OperationResult;
    try { operation = await fetchConfigOperation(operationId, options.signal); }
    catch (error) {
      if (!isTransientConfigRead(error)) throw new PendingOperationError(operationId, error);
      if (attempt + 1 < attempts) await waitForPoll(options.pollIntervalMs ?? 250, options.signal);
      continue;
    }
    assertOperationId(operationId, operation);
    return settleOperation(operation, { ...options, maxPolls: attempts - attempt - 1 });
  }
  throw new PendingOperationError(operationId);
}

function assertOperationId(expected: string, operation: OperationResult): void {
  if (operation.operation_id !== expected) {
    throw new ApiError({
      code: "INVALID_RESPONSE",
      message: "operation response does not match request",
      kind: "invalid-response",
    });
  }
}

function waitForPoll(delayMs: number, signal?: AbortSignal): Promise<void> {
  signal?.throwIfAborted();
  if (delayMs <= 0) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const timeout = window.setTimeout(done, delayMs);
    function done() {
      signal?.removeEventListener("abort", aborted);
      resolve();
    }
    function aborted() {
      window.clearTimeout(timeout);
      signal?.removeEventListener("abort", aborted);
      reject(signal?.reason ?? new DOMException("Aborted", "AbortError"));
    }
    signal?.addEventListener("abort", aborted, { once: true });
  });
}
