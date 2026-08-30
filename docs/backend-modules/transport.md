# Transport 模块设计

> 状态：v1 方案已完成，代码未实现
>
> 更新日期：2026-08-30
>
> 目标代码：`backend/src/transport/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [配置字段参考](../configuration-reference.md)
>
> 相关契约：[Ports 模块](ports.md) · [DNS Core 模块](dns-core.md)

## 1. 职责与边界

Transport 模块实现 UDP、TCP、DoH、TLS 和客户端身份恢复 adapter。它把网络输入转换成统一 `DnsRequest`，并把 `CanonicalResponse` 编码回对应 transport。

它负责 framing、协议限制、client IP trust boundary、连接生命周期和 response envelope；不负责策略、缓存、上游或 SQLite。

`ports/inbound.rs` 定义契约，`transport/*` 实现契约。两者不能互相包含第三方框架类型。

## 2. 内部结构

| 文件 | 职责 |
| --- | --- |
| `udp.rs` | datagram receive/send、EDNS 尺寸和截断 |
| `tcp.rs` | accept、两字节 length framing、连接内请求 |
| `doh.rs` | Axum route、GET/POST、HTTP/DNS 错误分层 |
| `tls.rs` | Rustls 配置加载和 TLS accept |
| `proxy_protocol.rs` | PROXY v1/v2 分片解析、长度与 trust 检查 |

forwarded header 解析可放在 `doh.rs` 的独立子模块，不能复用未经验证的任意 header 字符串。

## 3. TransportProfile

Config/Runtime 为每个 endpoint 编译 immutable `TransportProfile`：

- protocol class：datagram、stream 或 multiplexed；
- listener/route ID；
- framing 和固定上限；
- TLS mode 与已加载的 server config；
- client IP source 与 trusted proxy matcher；
- response encoder；
- admission-control handle；
- opaque cache compatibility key。

DNS Core 只看到 `TransportCapabilities`，不看到 address、certificate、HTTP method 或 proxy header。

## 4. 统一入站流程

```text
receive / accept
  → admission control
  → recover peer/client identity
  → framing and size validation
  → parse DNS wire
  → create RequestMeta + response correlation
  → DnsCore
  → ResponseEncoder
```

每个入口都有有界连接/请求预算。达到预算时：

- UDP 可丢弃并增加 `admission_dropped`；
- TCP/DoH 在形成请求前拒绝或关闭连接；
- 已进入 Core 的请求不因新请求拥塞被抢占；
- 预算值在 v1 作为有测试的实现常量，不新增未定稿配置字段。

## 5. UDP

UDP adapter：

- 每个 bind socket 一个受监督 receive loop；
- 使用可复用 buffer pool，但每次解析只暴露本请求 slice；
- datagram 大于 65,535 字节或无法解析时丢弃并计数；
- `peer_addr` 同时作为默认 `client_addr`；
- query DNS ID 从 canonical query 分离，响应时恢复；
- 输出按客户端 EDNS advertised UDP size 重新编码；
- 完整响应超出限制时按 RR 边界截断并设置 TC；
- 本地输出截断不覆盖缓存中的完整 canonical response。

UDP 没有连接级响应确认；send error 作为 request-local 事件记录。

## 6. TCP

TCP adapter：

- accept loop 与 connection task 都由 supervisor 持有；
- 每个 DNS frame 使用两字节网络序长度；
- 长度 0、超过 65,535、半包超时或 EOF 中断都关闭连接；
- 支持同连接连续请求；
- v1 按读取顺序输出响应，避免乱序破坏简单客户端兼容性；
- connection idle timeout、每连接 in-flight 和全局连接数使用固定 profile 上限；
- shutdown 时停止 accept，允许已读完整 frame 在 grace deadline 内完成。

如果后续需要 TCP pipelining 乱序响应，应先修改 correlation 契约和测试，不在 v1 隐式开启。

## 7. DoH

每条 route 同时支持 GET/POST：

- GET：唯一 `dns` 参数、无 padding base64url；
- POST：`application/dns-message` raw body；
- 解码后 DNS wire 和 POST body最大 65,535 字节；
- GET `dns` 最多 87,380 字符；
- method 错误返回 405 和 `Allow: GET, POST`；
- 媒体类型错误返回 415；
- request target/body 超限返回 414/413；
- base64 或 DNS wire 非法返回 400；
- 已形成 DNS transaction 后，NXDOMAIN/REFUSED/SERVFAIL 等使用 HTTP 2xx；
- 成功响应 Content-Type 为 `application/dns-message`，Cache-Control 固定 `no-store`。

route template 负责提取可选 `client_id`，但日志不记录实际路径参数或 query string。

## 8. TLS

`tls.mode=terminate` 在 prepare 阶段：

- 读取证书链和私钥；
- 拒绝空链、无匹配 key、加密但无法解密的 key 和不支持算法；
- 显式安装 Rustls crypto provider；
- 构造只读 `Arc<ServerConfig>`。

`tls.mode=external` 不读取证书材料。v1 不实现证书热加载；证书变化需要新 candidate/rebind。

TLS handshake 受独立 timeout 和 cancellation 约束，失败不进入 HTTP router。

## 9. Client IP 恢复

### peer

直接使用 socket peer address。

### forwarded_header

只有 peer 命中 `trusted_proxies` 才解析 header。链从右向左：

1. 跳过连续的可信代理地址；
2. 第一个非可信地址视为客户端；
3. 全链均可信时取最左地址；
4. 非 IP token、非法 quoting 或多义格式按 `on_invalid` 处理。

反向代理必须清理客户端自带 header；FluxDNS 不能仅凭 header 内容建立信任。

### proxy_protocol

顺序为：

```text
TCP accept → peer trust check → required PROXY header → optional TLS → HTTP
```

- v1 在前 107 字节内完成 PROXY v1；
- v2 总前导长度不超过 536 字节；
- 支持分片读取；
- 未知但长度合法的 v2 TLV 跳过；
- 不可信 peer、缺失 header、未知版本、非法长度或无 TCP4/TCP6 源地址时拒绝；
- 不回退到 peer 模式。

## 10. ResponseEncoder

encoder 由 request correlation 持有并只能调用一次：

- UDP：恢复 ID、按本次尺寸编码和截断；
- TCP：恢复 ID、添加 length prefix；
- DoH：恢复 ID、构造 HTTP response；
- client gone/cancelled 时返回分类结果，不重试到其他 transport；
- encode 失败不会重新进入 DNS Core。

## 11. 安全与观测

不记录：

- raw DNS wire；
- DoH query string；
- 完整 `client_id`；
- forwarded/proxy 原始 header；
- TLS private key；
- ECS 原始值。

允许的低基数字段包括 listener、endpoint、route template、transport class、method、结果分类和 wire size bucket。

## 12. 测试

- UDP/TCP canonical equivalence、ID 恢复和 UDP TC；
- TCP length 分片、半包、EOF、idle timeout 和顺序响应；
- DoH GET/POST、媒体类型、方法、65,535/87,380 边界和 HTTP/DNS 分层；
- TLS 证书/key 组合与 handshake timeout；
- forwarded header 信任链、伪造 header、missing/invalid policy；
- PROXY v1/v2 分片、未知 TLV、非法长度、不可信 peer；
- admission control、client disconnect、shutdown cancellation；
- 所有 adapter 通过 Ports contract suite。

## 13. 实现检查清单

- [ ] 实现 profile/capabilities 映射；
- [ ] 实现 UDP、TCP adapter；
- [ ] 实现 DoH GET/POST；
- [ ] 实现 TLS 与 client IP 恢复；
- [ ] 实现 response correlation/encoder；
- [ ] 完成资源限制、安全和协议测试。

当前实现进度：**0%**。
