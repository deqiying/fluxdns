import { describe, expect, it, vi } from "vitest";
import { hostsJsonError } from "./hosts-json";

const missingComma = '{\n  "first.example": {"A": ["192.0.2.1"]}\n  "second.example": {"A": ["192.0.2.2"]}\n}';

describe("上游 JSON 格式提示", () => {
  it.each(["", " \r\n\t", "{}", '{"example.test":{"A":["192.0.2.1"],"AAAA":"2001:db8::1"}}'])("保留空白必填分工及合法对象：%s", (source) => {
    expect(hostsJsonError(source)).toBeUndefined();
  });

  it("缺逗号时提示发生位置，不回显域名和地址", () => {
    const message = hostsJsonError(missingComma);
    expect(message).toContain("JSON 格式错误（第 3 行，第 3 列）");
    expect(message).toContain("英文逗号");
    expect(message).not.toContain("first.example");
    expect(message).not.toContain("192.0.2.1");
  });

  it.each(['{"example.test": {},}', "{'example.test': {}}", '{/* comment */"example.test": {}}', '{"example.test": {'])("拦截尾逗号、单引号、注释和未闭合括号：%s", (source) => {
    expect(hostsJsonError(source)).toContain("JSON 格式错误");
  });

  it.each(["[]", "null", '"text"', "42", "true"])("拒绝非对象顶层：%s", (source) => {
    expect(hostsJsonError(source)).toBe("JSON 顶层必须是域名映射对象，不能是数组、null 或其他值。");
  });

  it("兼容只提供字符位置的错误，正确处理 CRLF 和 UTF-16 字符", () => {
    const source = '{"😀": {},\r\n  "second.example": }';
    vi.spyOn(JSON, "parse").mockImplementationOnce(() => { throw new SyntaxError(`Unexpected token at position ${source.lastIndexOf("}")}`); });
    expect(hostsJsonError(source)).toContain("（第 2 行，第 21 列）");
  });

  it("兼容 Firefox 的行列位置文案", () => {
    vi.spyOn(JSON, "parse").mockImplementationOnce(() => { throw new SyntaxError("JSON.parse: expected ',' or '}' at line 3 column 3 of the JSON data"); });
    expect(hostsJsonError(missingComma)).toContain("（第 3 行，第 3 列）");
  });

  it("未结束的 JSON 将位置指向输入末尾", () => {
    vi.spyOn(JSON, "parse").mockImplementationOnce(() => { throw new SyntaxError("Unexpected end of JSON input"); });
    expect(hostsJsonError("{\n  ")).toContain("（第 2 行，第 3 列）");
  });

  it("浏览器未提供位置时只显示通用提示，不伪造行列或回显原文", () => {
    vi.spyOn(JSON, "parse").mockImplementationOnce(() => { throw new SyntaxError("JSON Parse error: Unexpected identifier 'private-value'"); });
    expect(hostsJsonError("{bad}")).toBe("JSON 格式错误：请检查条目之间的英文逗号、双引号和括号是否配对。");
  });

  it("不把非语法异常伪装成用户输入错误", () => {
    vi.spyOn(JSON, "parse").mockImplementationOnce(() => { throw new Error("unexpected failure"); });
    expect(() => hostsJsonError("{}")).toThrow("unexpected failure");
  });
});
