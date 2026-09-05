# Transport 模块设计

> 文档状态：有效
>
> 适用范围：DNS wire、UDP、TCP、DoH、TLS、client IP 恢复和响应编码
>
> 最后评审：2026-09-05（模块边界与关键契约静态核对，基线见[模块索引](README.md)；不含运行验收）
>
> 关联实现：[udp.rs](../../../../backend/src/transport/udp.rs)、[tcp.rs](../../../../backend/src/transport/tcp.rs)、[doh.rs](../../../../backend/src/transport/doh.rs)、[service.rs](../../../../backend/src/service.rs)、[system_socket.rs](../../../../backend/src/runtime/system_socket.rs)
>
> 关联文档：[后端架构](../overview.md) · [配置字段参考](../../../implementation/configuration.md) · [Ports 模块](ports.md) · [DNS Core 模块](dns-core.md)

## 1. 职责与边界

Transport 模块实现 UDP、TCP、DoH、TLS 和客户端身份恢复 adapter。它把网络输入转换成统一 `DnsRequest`，并把 `CanonicalResponse` 编码回对应 transport。

它负责 framing、协议限制、client IP trust boundary、连接生命周期和 response envelope；不负责策略、缓存、上游或 SQLite。

`ports/inbound.rs` 定义契约，`transport/*` 实现契约。两者不能互相包含第三方框架类型。

## 2. 内部结构

| 文件 | 职责 |
| --- | --- |
| `udp.rs` | datagram receive/send、EDNS 尺寸和截断 |
| `tcp.rs` | accept、两字节 length framing、连接内请求 |
| `doh.rs` | HTTP route、GET/POST、HTTP/DNS 错误分层、客户端身份恢复和 session |
| `runtime/system_socket.rs` | 系统 TCP listener、Rustls `ServerConfig` 和 TLS stream upgrade |

### 2.1 共享 DNS wire codec

`wire.rs` 提供 transport 共用的 DNS message 边界：

- 空报文、超过调用方限制或超过绝对 65,535 字节上限时拒绝；
- 从原始 wire message 提取 DNS ID，再生成不携带 envelope ID 的 `CanonicalQuery`；
- 编码 `CanonicalResponse` 时只修改副本并恢复请求 ID，不污染 canonical response；
- 将 decode、canonical validation、encode 和尺寸失败归入稳定的 `WireError`，不暴露底层库错误文本。

该 codec 只处理 DNS message 本身，不负责 UDP/TCP framing、策略、上游或 socket 生命周期。

### 2.2 UDP/TCP 入站

- UDP adapter 从 opaque socket 接收 datagram，构造统一 `RequestContext`，响应时恢复 DNS ID；完整响应超出客户端尺寸时按 RR 边界截断并设置 `TC`。
- TCP adapter 使用两字节网络序 length framing；`TcpSession` 为每个 accepted connection 固定 `ConnectionId`，连续 frame 递增 `StreamId`，连接内按读取顺序完成 Core 和 response write。
- clean EOF、半帧 EOF、零长度 frame、decode 错误和 response write 错误只关闭当前连接；listener task 继续接收其他连接。
- TCP listener 的 connection tasks 由内部 `JoinSet` 持有，未使用 detached `tokio::spawn`。
- `BindTransport` 将 DoH endpoint 与 raw TCP 明确区分；service 根据 typed binding 构造独立的 DoH listener/session，不把 HTTP 请求误交给 raw DNS/TCP adapter。

forwarded header 和 PROXY 前导解析均限定在 `doh.rs` 的 typed 边界内，不能复用未经验证的任意 header 字符串。

## 3. Endpoint 装配

当前没有统一 `TransportProfile` 类型。Config 的 `BindEntry` 表达 socket/transport/DoH endpoint 引用；`DnsService::prepare_transport_plans` 把它与 resolved 配置组合为 UDP/TCP/DoH adapter 和 `TransportTaskPlan`。TLS/client IP/route 参数由各 adapter 持有，request correlation 持有 encoder。

请求将 protocol class、UDP payload 能力和 opaque cache compatibility 等表示为 `TransportCapabilities`；Core 不读取 certificate、HTTP method 或 proxy header。有效 client address 则通过 `RequestContext.client` 提供给策略和 ECS，不等同 socket/HTTP 对象泄漏。

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

实际并发边界是：

- 每个 UDP socket 的 service loop 顺序 receive → dispatch → encode，不为每个 datagram 新建 task；
- 每个 TCP/DoH listener 最多 1,024 个 session，满时暂停 accept；每个 session 顺序处理请求；
- `ActiveRuntime::try_acquire` 用于 drain 计数，`Capacity` 只是 `usize::MAX` 溢出防护，不是可配置的全局请求限流器；
- 当前没有统一 admission-control handle 或 UDP `admission_dropped` 专用 counter，不能把设计中的通用资源预算写成已接线限流。

## 5. UDP

UDP adapter：

- 每个 bind socket 一个受监督 receive loop；
- system socket 每次 receive 分配有界 `Vec<u8>`，尚无 buffer pool；
- adapter 以 65,535 字节上限接收；非法 wire 在 header 可靠时尝试安全错误响应，否则忽略，不承诺每类丢弃都有独立 counter；
- `peer_addr` 同时作为默认 `client_addr`；
- query DNS ID 从 canonical query 分离，响应时恢复；
- 输出按客户端 EDNS advertised UDP size 重新编码；
- 完整响应超出限制时按 RR 边界截断并设置 TC；
- 本地输出截断不覆盖缓存中的完整 canonical response。

UDP 没有连接级响应确认；send error 作为 request-local 事件记录。`recv_from` 无流量 deadline 只开始下一轮接收，不触发 endpoint retry。

## 6. TCP

TCP adapter：

- Supervisor 持有 accept task，其内部 JoinSet 持有 connection task；
- 每个 DNS frame 使用两字节网络序长度；
- 长度 0、超过 65,535、半包超时或 EOF 中断都关闭连接；
- 支持同连接连续请求；
- v1 按读取顺序输出响应，避免乱序破坏简单客户端兼容性；
- frame read 使用 adapter request timeout，单连接一次只处理一个请求；每个 TCP listener 最多持有 1,024 个 active session，达到上限时暂停 accept；
- shutdown 时取消 accept 及 session，正在进行的 dispatch 可被取消并关闭连接，不保证已读完整 frame 最终响应；无连接的 accept deadline 只推进轮询。

如果后续需要 TCP pipelining 乱序响应，应先修改 correlation 契约和测试，不在 v1 隐式开启。

## 7. DoH

DoH endpoint 只复用底层 TCP socket，不能被当作 raw DNS/TCP 服务。typed binding 必须创建独立 listener/session task，并由 owner 持有连接；每个 listener 最多 1,024 个 active session，达到上限时暂停 accept；无连接的 accept deadline 只推进轮询。

每条 route 同时支持 GET/POST：

- GET：唯一 `dns` 参数、无 padding base64url；
- POST：`application/dns-message` raw body；type/subtype 大小写不敏感，未定义参数返回 415；
- HTTP/1.x framing：拒绝重复 `Content-Length` 和全部 `Transfer-Encoding`，不接受 chunked body；
- 不支持 `Expect`/`100-continue` interim response，收到时立即返回 417 并关闭连接；
- 解码后 DNS wire 和 POST body最大 65,535 字节；
- GET `dns` 最多 87,380 字符；
- request-target、header fields 和 POST body 独立计费；request-target 上限为 131,072 字节，header fields 上限为 16 KiB，session 总 buffer 仍为三者之和形成的固定上限；
- method 错误返回 405 和 `Allow: GET, POST`；
- 媒体类型错误返回 415；
- request target/body 超限返回 414/413；
- base64 或 DNS wire 非法返回 400；
- method 必须符合 token 语法，request-target 只接受可见 ASCII；畸形 request-line 返回 400，合法但不支持的方法返回 405；
- HTTP/1.1 缺少 `Host`，或任意 HTTP/1.x 的 `Host` 重复、包含非法 authority 时返回 400 并关闭连接；
- 已形成 DNS transaction 后，NXDOMAIN/REFUSED/SERVFAIL 等使用 HTTP 2xx；
- 成功响应 Content-Type 为 `application/dns-message`，Cache-Control 固定 `no-store`。

TLS、forwarded header 与 PROXY 的约束见下节；HTTP/1.x 请求按读取顺序处理并支持有界 keep-alive。本设计不提供入站 HTTP/2 或证书热加载契约，实际支持和握手/故障验证边界见[DNS 管线实现](../../../implementation/backend/dns-pipeline.md)。

route template 由 Config 提供的共享 compiler 校验和匹配。末尾 `/{client_id}` 同时接受去掉该段后的裸路径与带一个非空 client ID 的路径：裸路径不产生 `client_id`，尾斜杠和额外路径段不匹配；非末尾占位符仍要求对应段存在。Transport 对真实 HTTP path 只匹配一次，将配置模板写入稳定 `route_id` 并把可选值写入客户端身份；Policy 只按 route ID 查表，不重建或二次匹配路径。日志不记录实际路径参数或 query string。

## 8. TLS

`tls.mode=terminate` 在 endpoint 装配阶段：

- 有界读取证书链和私钥，文件上限分别为 1 MiB 和 64 KiB；
- 拒绝空链、无匹配 key、加密但无法解密的 key 和不支持算法；
- 显式安装 Rustls crypto provider；
- 将脱敏的 DER 材料交给 system socket，由其构造 `ServerConfig` 并在连接 session 内完成 stream upgrade。

`tls.mode=external` 不读取证书材料。v1 没有证书文件 watcher；需要触发配置 candidate/endpoint adapter 重建才重新读证书，单独修改 PEM 文件并不保证触发 reload。底层 listener 是否 rebind 由 BindPlan 复用判定，不等同 TLS 材料是否重建。

TLS handshake 受 endpoint request timeout 和 cancellation 约束，失败不进入 HTTP router 且只关闭当前 session；必须证明同一 listener 仍能服务后续连接。v1 不新增独立 TLS timeout 或证书热加载。

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
TCP accept → peer trust check → required PROXY header → optional TLS upgrade → HTTP
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

## 12. 契约验证要求

- UDP/TCP canonical equivalence、ID 恢复和 UDP TC；
- TCP length 分片、半包、EOF、idle timeout 和顺序响应；
- DoH GET/POST、request-line、Host authority/cardinality、媒体类型、严格十进制 `Content-Length`、`Expect` 拒绝、body framing、header 数量/字节、方法、65,535/87,380 边界、最大 request-target 独立计费和 HTTP/DNS 分层；
- DoH 精确 route、尾部 client ID route 的裸路径/带 ID/尾斜杠/多段边界，以及 canonical route ID；
- TLS PEM/DER 证书/key 组合、handshake timeout 与坏连接隔离；
- forwarded header 信任链、伪造 header、missing/invalid policy；
- PROXY v1/v2 分片、未知 TLV、非法长度、不可信 peer；
- admission control、client disconnect、shutdown cancellation；
- UDP/TCP/DoH 多轮无流量 deadline 后继续服务且不消耗 endpoint retry；
- wire codec 的 DNS ID 分离/恢复、canonicalization、输入输出尺寸上限和安全错误分类；
- UDP/TCP 在 header 可靠时对非法 question/解码返回 FORMERR、对非 QUERY opcode 返回 NOTIMP；短 header 和需要 OPT 的 BADVERS 不猜测响应；
- 共享 fake 与各 adapter 本地用例覆盖相关契约；完整 TLS/proxy/取消/资源上限矩阵仍见[差距计划](../../../plans/backend-contract-gaps.md)，不宣称已通过统一 conformance 门禁。
