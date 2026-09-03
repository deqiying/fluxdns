# Transport 模块设计

> 状态：v1 方案已完成，已实现 wire、UDP/TCP adapter、TCP 持久 session、DoH plain HTTP adapter 与 Host cardinality 边界、forwarded header、PROXY v1/v2 首轮客户端地址恢复和 TLS terminate 首轮握手；TLS material 已有启动读取上限，坏 TLS 握手的连接级隔离已有真实 loopback 证据
>
> 更新日期：2026-09-03
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
| `doh.rs` | plain HTTP route、GET/POST、HTTP/DNS 错误分层、forwarded header/PROXY 首轮恢复和 session |
| `runtime/system_socket.rs` | 系统 TCP listener、Rustls `ServerConfig` 和 TLS stream upgrade |

### 3.1 当前已实现：共享 DNS wire codec

`wire.rs` 提供 transport 共用的 DNS message 边界：

- 空报文、超过调用方限制或超过绝对 65,535 字节上限时拒绝；
- 从原始 wire message 提取 DNS ID，再生成不携带 envelope ID 的 `CanonicalQuery`；
- 编码 `CanonicalResponse` 时只修改副本并恢复请求 ID，不污染 canonical response；
- 将 decode、canonical validation、encode 和尺寸失败归入稳定的 `WireError`，不暴露底层库错误文本。

该 codec 只处理 DNS message 本身，不负责 UDP/TCP framing、策略、上游或 socket 生命周期。

### 3.2 当前已实现：UDP/TCP 入站

- UDP adapter 从 opaque socket 接收 datagram，构造统一 `RequestContext`，响应时恢复 DNS ID；完整响应超出客户端尺寸时按 RR 边界截断并设置 `TC`。
- TCP adapter 使用两字节网络序 length framing；`TcpSession` 为每个 accepted connection 固定 `ConnectionId`，连续 frame 递增 `StreamId`，连接内按读取顺序完成 Core 和 response write。
- clean EOF、半帧 EOF、零长度 frame、decode 错误和 response write 错误只关闭当前连接；listener task 继续接收其他连接。
- TCP listener 的 connection tasks 由内部 `JoinSet` 持有，未使用 detached `tokio::spawn`。
- `BindTransport` 将 DoH endpoint 与 raw TCP 明确区分；service 根据 typed binding 构造独立的 DoH listener/session，不把 HTTP 请求误交给 raw DNS/TCP adapter。

forwarded header 和 PROXY 前导解析均限定在 `doh.rs` 的 typed 边界内，不能复用未经验证的任意 header 字符串。

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

UDP 没有连接级响应确认；send error 作为 request-local 事件记录。`recv_from` 无流量 deadline 只开始下一轮接收，不触发 endpoint retry。

## 6. TCP

TCP adapter：

- accept loop 与 connection task 都由 supervisor 持有；
- 每个 DNS frame 使用两字节网络序长度；
- 长度 0、超过 65,535、半包超时或 EOF 中断都关闭连接；
- 支持同连接连续请求；
- v1 按读取顺序输出响应，避免乱序破坏简单客户端兼容性；
- connection idle timeout 和每连接 in-flight 使用固定 profile 上限；每个 TCP listener 最多持有 1,024 个 active session，达到上限时暂停 accept；
- shutdown 时停止 accept，允许已读完整 frame 在 grace deadline 内完成；无连接的 accept deadline 只推进轮询。

如果后续需要 TCP pipelining 乱序响应，应先修改 correlation 契约和测试，不在 v1 隐式开启。

## 7. DoH

当前已实现 plain HTTP、TLS terminate 首轮握手和 forwarded header 首轮客户端地址恢复。DoH endpoint 只复用底层 TCP socket，不会被当作 raw DNS/TCP 服务；service 根据 `BindTransport::Doh` 构造独立 listener/session task，并由内部 `JoinSet` 管理连接；每个 listener 最多持有 1,024 个 active session，达到上限时暂停 accept；无连接的 accept deadline 只推进轮询。

每条 route 同时支持 GET/POST：

- GET：唯一 `dns` 参数、无 padding base64url；
- POST：`application/dns-message` raw body；type/subtype 大小写不敏感，未定义参数返回 415；
- HTTP/1.x framing：拒绝重复 `Content-Length` 和全部 `Transfer-Encoding`，不接受 chunked body；
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

首轮实现边界：`tls.mode=terminate` 在 endpoint 装配阶段加载 PEM/DER 证书链和私钥，system socket 显式使用 ring provider 完成 TLS 1.2/1.3 握手；`tls.mode=external` 不读取证书材料。启用 PROXY 时，DoH session 先以单字节有界读取消费可信前导，再升级同一连接到 TLS。TLS 握手失败只关闭当前 session，listener 继续接收后续连接。`client_ip.source=forwarded_header` 已支持三个配置 header、trusted proxy CIDR、右向左链解析及 missing/invalid 的 `reject`/`use_peer` 策略；`client_ip.source=proxy_protocol` 已支持可信 peer、PROXY v1/v2、TCP4/TCP6、分片读取和长度上限，未知 v2 TLV 在长度合法时跳过；HTTP/1.x 请求按读取顺序处理并支持有界 keep-alive，尚未实现 HTTP/2、证书热加载和完整握手/故障矩阵验收。

route template 负责提取可选 `client_id`，但日志不记录实际路径参数或 query string。

## 8. TLS

`tls.mode=terminate` 在 endpoint 装配阶段：

- 有界读取证书链和私钥，文件上限分别为 1 MiB 和 64 KiB；
- 拒绝空链、无匹配 key、加密但无法解密的 key 和不支持算法；
- 显式安装 Rustls crypto provider；
- 将脱敏的 DER 材料交给 system socket，由其构造 `ServerConfig` 并在连接 session 内完成 stream upgrade。

`tls.mode=external` 不读取证书材料。v1 不实现证书热加载；证书变化需要新 candidate/rebind。

TLS handshake 受 endpoint request timeout 和 cancellation 约束，失败不进入 HTTP router且不影响 listener；真实 loopback 已验证坏握手后同一 listener 仍可完成下一条 TLS DoH 请求。v1 暂不实现独立 TLS timeout 或证书热加载。

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

## 12. 测试

- UDP/TCP canonical equivalence、ID 恢复和 UDP TC；
- TCP length 分片、半包、EOF、idle timeout 和顺序响应；
- DoH GET/POST、request-line、Host authority/cardinality、媒体类型、严格十进制 `Content-Length`、body framing、header 数量/字节、方法、65,535/87,380 边界、最大 request-target 独立计费和 HTTP/DNS 分层；
- TLS PEM/DER 证书/key 组合、handshake timeout 与坏连接隔离；
- forwarded header 信任链、伪造 header、missing/invalid policy；
- PROXY v1/v2 分片、未知 TLV、非法长度、不可信 peer；
- admission control、client disconnect、shutdown cancellation；
- UDP/TCP/DoH 多轮无流量 deadline 后继续服务且不消耗 endpoint retry；
- wire codec 的 DNS ID 分离/恢复、canonicalization、输入输出尺寸上限和安全错误分类；
- 所有 adapter 通过 Ports contract suite。

## 13. 实现检查清单

- [x] 实现 v1 固定 profile/capabilities 映射，并由 transport 模块统一提供；
- [x] 实现 UDP、TCP adapter；
- [x] 实现 DoH plain HTTP GET/POST；
- [x] 按 HTTP/1.1 契约拒绝缺失、重复或非法 authority 的 `Host`，并保留 HTTP/1.0 兼容路径；
- [x] 分离 DoH request-target、header fields 和 POST body 上限，确保完整 GET wire 不被 header 预算提前拒绝；
- [x] 拒绝重复 `Content-Length`、`Transfer-Encoding`、超过 64 个 header 和超过 16 KiB header fields；
- [x] 严格校验 request-line，并按 HTTP 媒体类型规则接受 `application/dns-message` 大小写变体；
- [x] 实现 TLS terminate 首轮证书加载和握手；
- [x] 实现 forwarded header 首轮 client IP 恢复；
- [x] 实现 PROXY v1/v2 首轮 client IP 恢复；
- [x] 实现 UDP/TCP response correlation/encoder；
- [x] 建立共享 DNS wire decode/encode boundary 和尺寸/错误分类测试；
- [x] 完成 UDP/TCP framing、尺寸、EOF、取消和顺序响应测试；
- [x] 完成 UDP/TCP/plain DoH 的真实 loopback response contract，统一验证 Positive/NODATA/NXDOMAIN/SERVFAIL/REFUSED、DNS ID 和 canonical response，并验证大响应下 UDP TC、TCP/DoH 完整响应；
- [x] 验证 transport task 的三次 transient retry 上限和 `FatalEndpoint` 耗尽升级；
- [x] 同一逻辑 listener 的单 endpoint 耗尽只降级，保留 sibling endpoint 继续服务；
- [x] 限制每个 TCP/DoH listener 的 active session 数，达到边界时暂停 accept；
- [x] 验证坏 TLS 握手只终止当前 DoH session，同一 listener 可继续服务后续正常连接；
- [ ] 完成 DoH/TLS 资源限制、安全和协议测试。

阶段证据：DoH codec/session、request-line/media type、Host authority/cardinality、request-target/header/body 独立上限、forwarded trust chain、PROXY v1/v2、TLS 材料加载、PROXY 前导后升级和客户端地址恢复定向测试通过；system socket Rustls loopback 验证成功握手、peer 保留、deadline 和 cancellation；阶段 164 新增真实 TLS loopback，验证坏握手后同一 listener 仍可接受正常 TLS DoH 请求；Service loopback 定向测试验证真实 UDP、DNS-over-TCP framing 和 plain DoH GET/POST 返回一致的 Positive/NODATA/NXDOMAIN/SERVFAIL/REFUSED canonical response 和各自 DNS ID，并验证 64 条 A 记录下 UDP 截断而 TCP/DoH GET/POST 保持完整；capability 定向测试验证 Datagram/Stream/Multiplexed 由 transport 模块稳定映射到 v1 cache compatibility；阶段 163–168 验证 transient retry 上限、endpoint 故障隔离、空闲 listener、session 上限和 framing 资源边界；阶段 190 后端全量 `555 passed、0 failed`。阶段 182 新增 Host authority 与严格十进制 `Content-Length` 校验；阶段 185 补齐 DER 证书/私钥直接加载证据；阶段 191 为证书链与私钥读取增加独立硬上限，DoH 24 项定向测试通过。真实 plain HTTP smoke 已验证 GET/POST、DNS ID/RCODE 和 SIGINT 停机。未测试 nginx、特权端口或 HTTP/2。

当前实现进度：**90%**。
