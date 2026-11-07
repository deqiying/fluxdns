/** 校验上游的 JSON 内联映射；只返回安全中文提示，不改写用户草稿或替代后端记录校验。 */
export function hostsJsonError(source: string): string | undefined {
  // 空白交给表单的必填规则，避免同时出现必填和语法两条错误。
  if (!source.trim()) return undefined;
  let value: unknown;
  try {
    value = JSON.parse(source);
  } catch (error) {
    if (!(error instanceof SyntaxError)) throw error;
    return `JSON 格式错误${errorLocation(source, error.message)}：请检查条目之间的英文逗号、双引号和括号是否配对。`;
  }
  if (value === null || Array.isArray(value) || typeof value !== "object") {
    return "JSON 顶层必须是域名映射对象，不能是数组、null 或其他值。";
  }
  return undefined;
}

/** 浏览器语法错误文案不同；只提取位置，不回显可能包含原始映射片段的错误消息。 */
function errorLocation(source: string, message: string): string {
  const lineColumn = /\bline (\d+) column (\d+)\b/i.exec(message);
  if (lineColumn && Number(lineColumn[1]) > 0 && Number(lineColumn[2]) > 0) {
    return `（第 ${lineColumn[1]} 行，第 ${lineColumn[2]} 列）`;
  }
  const position = /\bposition (\d+)\b/i.exec(message);
  const offset = position ? Number(position[1]) : /unexpected (?:end|eof)/i.test(message) ? source.length : undefined;
  if (offset === undefined || offset > source.length) return "";
  // JSON.parse 的位置按 UTF-16 字符单元计数；CRLF 视为一个换行。
  const lines = source.slice(0, offset).split(/\r\n|\r|\n/);
  return `（第 ${lines.length} 行，第 ${lines[lines.length - 1].length + 1} 列）`;
}
