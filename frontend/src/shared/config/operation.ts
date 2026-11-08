import { ApiError, getSafeErrorMessage } from "@/shared/api/errors";
import { generateUuidV4 } from "@/shared/uuid";
import {
  applyCandidate,
  fetchConfigOperation,
  fetchConfigState,
  restoreConfigFiles,
  retryConfigPersistence,
  validateCandidate,
  type ApplyRequest,
  type ConfigModule,
  type ConfigState,
  type FileSyncRequest,
  type OperationResult,
  type ValidationResult,
} from "./api";
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
  declare readonly operationId: string;
  constructor(operationId: string, cause?: unknown) {
    const detail = cause instanceof ApiError ? cause : undefined;
    super({ code: "CONFIG_OPERATION_PENDING", message: "配置操作结果尚未确认，请继续查询。", kind: "http", cause,
      operationId, requestId: detail?.requestId, diagnostic: detail?.diagnostic, serverMessage: detail?.serverMessage, fieldErrors: detail?.fieldErrors });
  }
}

/** 终态转换保留 operation ID 和安全诊断；不把 unknown 或部分成功当作 rejected。 */
export function operationError(operation: OperationResult): ApiError {
  const status = operation.status;
  return new ApiError({
    code: "error" in status ? status.error : "CONFIG_OPERATION_PENDING",
    message: "配置操作结果",
    kind: "http",
    operationId: operation.operation_id,
    diagnostic: operation.diagnostic,
  });
}

export function getOperationMessage(operation: OperationResult): string {
  const status = operation.status;
  if (status.state === "applied_synced") return "配置已应用并同步到文件";
  return getSafeErrorMessage(operationError(operation), status.state === "applied_unpersisted"
    ? "运行配置已生效，但文件尚未同步。请在全局配置提示中重试。"
    : undefined);
}

/** 每次保存仅生成一次，确认重试与结果回读继续沿用同一 ID。 */
export function createOperationId(): string {
  return generateUuidV4();
}

/** apply 只发送一次；传输/响应丢失后使用同一 operation_id 回读，不重放变更。 */
export async function applyAndSettle(
  request: ApplyRequest,
  options: ApplyAndSettleOptions = {},
): Promise<OperationSettlement> {
  let operation: OperationResult;
  try {
    operation = await applyCandidate(request, { module: options.module, signal: options.signal });
  } catch (error) {
    if (!(error instanceof ApiError)) throw new PendingOperationError(request.operation_id, error);
    if (error.kind === "http") throw new ApiError({ ...error, message: error.message, operationId: request.operation_id, cause: error });
    return resumeOperation(request.operation_id, options);
  }
  try {
    assertOperationId(request.operation_id, operation);
    return await settleOperation(operation, options);
  } catch (error) { throw error instanceof PendingOperationError ? error : new PendingOperationError(request.operation_id, error); }
}

/** 仅首次受理前的确认拒绝可补充确认重试；不确定结果仍交给 applyAndSettle 只查询。
 * 外部采用传入的文件基准不可被后续 validate 悄悄替换，二次冲突直接交还草稿。
 */
export async function applyWithConfirmation(
  request: ApplyRequest,
  confirm: (validation: ValidationResult) => Promise<boolean>,
  options: ApplyAndSettleOptions = {},
): Promise<OperationSettlement | null> {
  try {
    return await applyAndSettle(request, options);
  } catch (error) {
    if (!(error instanceof ApiError) || error.status !== 409
      || (error.code !== "CONFIRMATION_REQUIRED" && error.code !== "EXTERNAL_CHANGES_REQUIRE_CONFIRMATION")) throw error;
    const validation = await validateCandidate({ changes: request.changes }, options);
    if (request.observed_file_revision !== undefined
      && validation.observed_file_revision !== request.observed_file_revision) {
      throw new ApiError({ code: "FILE_REVISION_CONFLICT", message: "配置文件已再次变化，请重新读取差异。", status: 409, kind: "http" });
    }
    if (!await confirm(validation)) return null;
    const confirmations = [...new Set([...(request.confirmations ?? []), ...validation.required_confirmations])];
    return applyAndSettle({
      ...request,
      confirmations,
      ...(confirmations.includes("discard_external_changes")
        ? { observed_file_revision: request.observed_file_revision ?? validation.observed_file_revision }
        : {}),
    }, options);
  }
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
    try { await waitForPoll(pollIntervalMs, options.signal); }
    catch (error) { throw new PendingOperationError(operation.operation_id, error); }
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
    catch (error) { throw new PendingOperationError(operation.operation_id, error); }
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
    if (!(error instanceof ApiError)) throw new PendingOperationError(request.operation_id, error);
    if (error.kind === "http") throw new ApiError({ ...error, message: error.message, operationId: request.operation_id, cause: error });
    return resumeOperation(request.operation_id, options);
  }
  try {
    assertOperationId(request.operation_id, operation);
    return await settleOperation(operation, options);
  } catch (error) { throw error instanceof PendingOperationError ? error : new PendingOperationError(request.operation_id, error); }
}

/** 恢复查询与随后轮询共用预算；没有成功快照时仍将原操作 ID 交回页面。 */
export async function resumeOperation(operationId: string, options: ApplyAndSettleOptions = {}): Promise<OperationSettlement> {
  const attempts = Math.max(0, options.maxPolls ?? 20);
  let lastReadError: unknown;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    let operation: OperationResult;
    try { operation = await fetchConfigOperation(operationId, options.signal); }
    catch (error) {
      lastReadError = error;
      if (!isTransientConfigRead(error)) throw new PendingOperationError(operationId, error);
      if (attempt + 1 < attempts) {
        try { await waitForPoll(options.pollIntervalMs ?? 250, options.signal); }
        catch (cause) { throw new PendingOperationError(operationId, cause); }
      }
      continue;
    }
    try {
      assertOperationId(operationId, operation);
      return await settleOperation(operation, { ...options, maxPolls: attempts - attempt - 1 });
    } catch (error) { throw error instanceof PendingOperationError ? error : new PendingOperationError(operationId, error); }
  }
  throw new PendingOperationError(operationId, lastReadError);
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
