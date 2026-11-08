import { describe, expect, it } from "vitest";
import { ApiError, getSafeErrorMessage, parseDiagnostic, safeText } from "./errors";

const diagnostic = { stage: "prepare", code: "RESOURCE_PARSE_FAILED", message: "规则文件无法解析", resource: "guard" };

describe("统一安全错误文案", () => {
  it("保留中文运行态语义、细分原因、字段错误及两个关联 ID", () => {
    const error = new ApiError({ code: "APPLY_FAILED", kind: "http", message: "not for display", diagnostic,
      requestId: "request-1", operationId: "operation-1", fieldErrors: [{ path: "/changes/0/change/rule", code: "INVALID_ARGUMENT" }] });
    const text = getSafeErrorMessage(error);
    for (const value of ["运行状态保持不变", "阶段：prepare", "资源：guard", "规则文件无法解析", "原因码：RESOURCE_PARSE_FAILED", "request-1", "operation-1", "/changes/0/change/rule", "INVALID_ARGUMENT"]) expect(text).toContain(value);
    expect(text).not.toContain("not for display");
  });

  it("旧响应使用已约定安全 message，固定英文说明不覆盖更好的中文 fallback", () => {
    expect(getSafeErrorMessage(new ApiError({ code: "APPLY_FAILED", kind: "http", message: "internal", serverMessage: "资源名与已有条目冲突" })))
      .toContain("资源名与已有条目冲突");
    const error = new ApiError({ code: "APPLY_FAILED", kind: "http", message: "internal", serverMessage: "request could not be completed" });
    expect(getSafeErrorMessage(error)).toBe("配置未能应用，运行状态保持不变。");
    expect(getSafeErrorMessage(error, "自定义中文说明")).toBe("自定义中文说明");
  });

  it("不读取任意 Error/cause 内的凭据或 HTML", () => {
    const secret = "https://user:password@example.com/?token=secret";
    expect(getSafeErrorMessage(new Error(secret))).not.toContain(secret);
    expect(getSafeErrorMessage(new ApiError({ code: "NETWORK_ERROR", kind: "network", message: secret, cause: new Error(secret) }))).not.toContain(secret);
  });

  it("按 UTF-8 字节限制诊断，控制字符不影响显示方向，字段列表有界", () => {
    const value = parseDiagnostic({ stage: "s".repeat(100), code: "C".repeat(100), message: `\u0000${"中".repeat(500)}\u202e`, resource: "😀".repeat(100) });
    expect(value?.stage).toHaveLength(64);
    expect(value?.code).toHaveLength(64);
    expect(new TextEncoder().encode(value?.message).length).toBeLessThanOrEqual(1024);
    expect(new TextEncoder().encode(value?.resource).length).toBeLessThanOrEqual(128);
    expect(value?.message).not.toMatch(/[\u0000\u202e]/);
    expect(safeText("😀中", 4)).toBe("😀");
    const error = new ApiError({ code: "VALIDATION_FAILED", kind: "http", message: "", requestId: "bad\nrequest", operationId: "https://user:secret@example.com", fieldErrors: Array.from({ length: 200 }, () => ({ path: "a".repeat(600), code: "INVALID_ARGUMENT" })) });
    expect(error.requestId).toBeUndefined();
    expect(error.operationId).toBeUndefined();
    expect(error.fieldErrors).toHaveLength(128);
    expect(error.fieldErrors[0].path).toHaveLength(512);
  });

  it.each([null, {}, { stage: "prepare", code: "CODE" }, { stage: "bad\nvalue", code: "CODE", message: "detail" }])("非法 diagnostic %j 回落原错误", (value) => {
    expect(parseDiagnostic(value)).toBeUndefined();
    expect(getSafeErrorMessage(new ApiError({ code: "APPLY_FAILED", message: "", kind: "http", diagnostic: value }))).toBe("配置未能应用，运行状态保持不变。");
  });
});
