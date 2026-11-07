/**
 * 生成小写 UUID v4，供配置操作和客户端 ID 共用。
 * 局域网 HTTP 属于非安全上下文，可能没有 crypto.randomUUID；
 * getRandomValues 不受该限制，直接组装可保留密码学随机性。
 */
export function generateUuidV4(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
