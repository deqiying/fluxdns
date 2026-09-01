# Upstream 模块设计

> 状态：v1 方案已完成，已实现内联 hosts exchange、可注入 DoH exchange、plain HTTP DoH transport、可注入地址解析 port、bootstrap 引用元数据透传、bootstrap 响应地址提取、注入 connector 的 bootstrap A/AAAA 查询、默认 DoH transport/Registry bootstrap 接线、hosts/plain HTTP DoH registry、Outbound profile/target 规划、协议无关 SOCKS5/SOCKS5H codec、PolicyCore direct request path、纯 group member selection 和 outcome/fallback 判定；自定义 transport 的 bootstrap resolver、SOCKS5/SOCKS5H 实际 stream handshake 与 outbound 仍未实现
>
> 更新日期：2026-09-01
>
> 目标代码：`backend/src/upstream/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [配置字段参考](../configuration-reference.md)
>
> 相关方案：[Ports](ports.md) · [Policy](policy.md) · [Cache](cache.md)

## 1. 职责

Upstream 模块把 typed upstream 配置编译为 `UpstreamRegistry` 和可复用 `UpstreamConnector`，并实现单上游交换、组选择、bootstrap 与 outbound。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `doh.rs` | DoH connector、HTTP/TLS/DNS response validation |
| `http.rs` | Tokio plain HTTP/1.1 DoH request/response adapter；TLS、proxy 和 connection pool 待后续接入 |
| `group.rs` | parallel、round-robin、load-balance、failover、fallback |
| `bootstrap.rs` | 上游主机名解析与地址 override |
| `outbound.rs` | direct、SOCKS5、SOCKS5H profile 和 SecretRef |

`upstreams[type=hosts]` 作为内存 connector 实现同一 `DnsExchange`，主要用于 bootstrap，也可被 strategy 直接引用。

## 2. Registry 与 connector

prepare 阶段为每个 upstream 生成 typed ID 和 connector handle。当前 registry 可构造已验证的 hosts connector，以及不含 bootstrap、proxy 和启用 ECS 覆盖的 plain HTTP DoH connector；DoH transport 通过可注入 port 提供：

- hosts connector；
- DoH connector；
- 地址解析 port；
- group selection policy；
- outbound profile；
- bootstrap dependency；
- 安全的观测标签。

`UpstreamRegistry::from_resolved` 默认创建共享 bootstrap connector registry，并使用带该 registry 的 `TokioDohHttpTransport` 构造 `http://` DoH；hosts/DoH direct connector 会在构造阶段登记，运行时由 `TokioDohAddressResolver` 按引用查找。HTTPS、proxy、启用的 ECS 覆盖和 Group 在构建边界返回带稳定 kind 的 `UnsupportedUpstream`，不会静默丢弃配置；bootstrap 不再被 registry 静默拒绝，但自定义 DoH transport 的 resolver 能力仍由调用方显式提供。归一化后的 `EcsMode::Disabled` 不会阻止 connector 构造。`ConfigId` 到 `ConnectorId` 的不兼容字符也在构建边界返回稳定错误。

connector 构建 key 至少包含 upstream、outbound、bootstrap/connect_ip 和 TLS/HTTP profile。相同 key 复用 client 和连接池。

DNS Core 只持有 typed connector/group handle，不读取 URL 或 proxy 配置。

## 3. DoH connector

当前已实现 `TokioDohHttpTransport` 的 plain HTTP/1.1 一次交换：使用 URL host 生成 Host header，使用显式 `connect_ip` 只替换 TCP 连接目标，固定 POST `application/dns-message`，要求 bounded header、`Content-Length` 和 DNS wire body，并将 deadline/cancellation 传递到解析、连接、写入和读取。地址解析通过 `DohAddressResolver` port 注入；默认实现无 bootstrap 时使用 Tokio `lookup_host`，配置 bootstrap 时通过共享 registry 调用 `BootstrapResolver`，显式 `connect_ip` 已验证不会触发 resolver。DoH request envelope 会保留可选的 bootstrap 引用，未配置或未登记 bootstrap adapter 时 fail-closed，不偷偷回退 system resolver。该 adapter 不支持 `https://`，HTTPS/TLS、proxy、HTTP/2 和连接池仍由后续 adapter 负责。

Reqwest client：

- `default-features = false`；
- 显式启用 Rustls、HTTP/2 和 SOCKS；
- 显式 `no_proxy()`，不继承系统代理；
- URL host 始终用于 HTTP Host 和 TLS SNI；
- 连接池与 TLS session 在 connector 生命周期内复用；
- request deadline 由 RequestContext 和上游组 deadline 的较小值决定。

请求默认使用 POST `application/dns-message`，避免 URL 暴露 query；未来是否支持上游 GET 不影响 port。

响应必须验证：

- HTTP 2xx；
- Content-Type 为 `application/dns-message`，允许合法参数但不接受其他媒体类型；
- body 不超过 65,535 字节；
- DNS QR、ID、question 和 wire 完整性；
- response 到达时请求尚未被取消，或结果仅允许进入 late cache finalizer。

HTTP、TLS、解析和协议错误统一为 `TransportFailure`，不能伪造 SERVFAIL response。

## 4. Bootstrap 与 connect_ip

目标地址顺序：

1. 显式 `connect_ip`；
2. 已配置 `bootstrap` connector；
3. 系统 resolver；
4. `socks5h://` 且无 connect_ip 时由代理解析。

`connect_ip` 只替换网络连接目标，不改变 URL host、Host header 或 SNI。

bootstrap：

- 通过引用 connector 查询 A/AAAA；
- `BootstrapResolver` 通过注入的 `DnsExchange` 顺序执行 A、AAAA 查询，合并合法地址并取最低 TTL；
- 默认 `UpstreamRegistry::from_resolved` 将 hosts/DoH connector 登记到共享 bootstrap registry，`TokioDohAddressResolver` 把地址转换为请求端口并交给 HTTP adapter；
- 只接受完整、合法地址答案；
- `bootstrap_answer_from_response` 只提取与 question owner 匹配的 A/AAAA，并按地址记录的最低 TTL 建立答案；
- 地址按 DNS TTL 缓存，并设置实现级最小/最大 refresh 边界；
- 刷新失败时可在未过期窗口内继续使用旧地址并标记 degraded；
- 无可用地址时本次 exchange 失败；
- dependency cycle 在 Config 阶段拒绝。

系统 resolver 只在未配置 bootstrap/connect_ip/proxy remote resolve 时使用，不作为任何失败路径的隐式 fallback。

## 5. Outbound

`OutboundProfile` 在显式 prepare 边界解析 SecretRef 的代理 URL，保存 scheme、代理端点和受保护的 credential material；`OutboundTarget` 固化目标 host/port、connect_ip、bootstrap 和本地/远程/旁路 hostname resolution 模式。`socks5` codec 只构造和解析 method negotiation、username/password、CONNECT 及 reply 帧，不执行 DNS、stream I/O 或 socket dial。

`socks5://`：

- 目标主机在本地按 connect_ip → bootstrap → system resolver 解析；
- 代理只看到目标 IP；
- credential 从 SecretRef 读取，不能进入 Debug/日志。

`socks5h://`：

- 无 connect_ip 时把 URL host 交给代理解析；
- 禁止同时配置 bootstrap；
- 有 connect_ip 时代理连接该 IP，Host/SNI 仍是 URL host。

Secret 变化在 v1 需要构建新 candidate，不对现有 client 原地修改 credential。

协议 codec：

- method negotiation 根据是否存在 credential 提供 `NO AUTH` 与 `USERNAME/PASSWORD` 方法列表；
- username/password 和 CONNECT 帧均限制单字段不超过 255 字节，拒绝空 credential、非法端口和超界 domain；
- CONNECT 地址支持 IPv4、IPv6 和远程 domain；`socks5` 本地解析必须先提供已解析 IP，`socks5h` 无 `connect_ip` 时保留 domain；
- response parser 验证 version、reserved、address type、完整长度并保留 reply/bound address；
- codec 不创建连接，也不记录 credential 或完整 domain 内容。

## 6. Group 总体语义

group deadline 为请求剩余 deadline 与 `timeout` 的较小值。任何合法 DNS response 都是 terminal response；只有全部 attempt 都没有 terminal response 时才进入 fallback。

fallback 使用独立 `fallback_timeout`，但不能超过请求总 deadline。进入 fallback 后不再回到主组。

一次 attempt 结果：

- terminal response：立即按模式规则结束或进入 parallel late window；
- transport failure：该成员失败，可继续其他成员；
- cancelled：依据原因终止本组或单 attempt。

## 7. 选择算法

### parallel

- 同时发起全部成员，weight 必须为 1；
- 第一个 terminal response 立即返回；
- 首响应为完整 NOERROR/TC=0 时取消其他成员；
- 首响应为 NXDOMAIN/REFUSED/SERVFAIL/TC 时，其余已发请求继续到完成或 timeout，只用于确定 cache candidate；
- late window 按配置顺序选择完整 NOERROR，再选择其他可缓存终态；
- 主组完全没有 terminal response 才进入 fallback。

### round-robin

- 使用 per-group 原子游标和 smooth weighted round-robin；
- weight 决定长期被选为 primary 的频率；
- primary transport failure 后，按本次计算的确定性候选顺序尝试尚未使用成员；
- 遇到第一个 terminal response 即结束；
- 无 terminal response 才进入 fallback。

### load-balance

- 选择 `in_flight / weight` 最小的成员；
- 相同比值时使用轮转游标打破平局，避免永久偏向首项；
- 只统计该 connector 当前受监督的活动 exchange，不建立主动健康状态；
- primary transport failure 后选择下一个未尝试成员；
- terminal response 与 fallback 规则同上。

### failover

- 严格按配置顺序串行尝试；
- weight 在该模式必须为 1，避免暗示不存在的流量权重；
- transport failure 才尝试下一成员；
- 任意 terminal response 都停止，不因 SERVFAIL/NXDOMAIN 继续切换；
- 全部成员失败后进入 fallback。

## 8. 并发与取消

每个 attempt 都是 supervisor 可追踪的子任务。Group aggregator 持有：

- group deadline；
- child cancellation tokens；
- responder 是否已完成；
- late cache finalizer；
- attempt outcome 列表。

客户端取消时：

- 没有 cache finalizer 价值的 attempt 立即取消；
- parallel 已返回非完整终态且仍可能得到可缓存完整答案时，可在 group deadline 内继续；
- shutdown 取消所有 late finalizer；
- late result 不能修改已返回客户端的 response。

## 9. Failure 与观测

TransportFailure 分类至少包括 connect、DNS bootstrap、proxy、TLS、HTTP status、media type、body limit、wire、question mismatch 和 timeout。

观测字段使用 upstream/group ID、outbound kind、attempt ordinal、outcome 和 latency bucket，不记录完整 URL、credential、query domain 或目标原始 IP。

v1 不实现主动健康检查、熔断器或持久健康分数。load-balance 只使用实时 in-flight，不应在文档或指标中称为 health。

当前已实现：`DohExchange` 固定 POST `application/dns-message` 请求，自动分配内部 DNS ID，保留 URL host 作为 Host/SNI，并将显式 `connect_ip`、bootstrap 引用、deadline、cancellation 和 HTTP/协议错误映射到 `UpstreamOutcome`；`TokioDohHttpTransport` 提供 plain HTTP/1.1 loopback-capable adapter，并在默认 Registry 路径接入共享 bootstrap resolver；`BootstrapResolver` 通过注入 connector 执行 A/AAAA 查询并合并地址；`OutboundProfile`/`OutboundTarget` 固化 SecretRef 脱敏、代理 scheme 和目标解析模式；`GroupSelector` 只负责无网络副作用的成员选择，提供 failover/parallel 配置顺序、smooth weighted round-robin、weighted least-in-flight、平局轮转和 `SelectionLease` 生命周期；`outcome` 提供按 attempt index 的 terminal/retryable/cancelled 聚合和 fallback 判定。自定义 transport 的 bootstrap resolver 注入、SOCKS5/SOCKS5H 握手与 socket dial、HTTPS/TLS、late cache finalizer 和 Runtime/DNS Core 接线尚未接入。

## 10. 测试

- DoH Host/SNI、connect_ip、bootstrap 和 system resolver；
- SOCKS5/SOCKS5H 本地/远程解析与 secret 脱敏；
- HTTP status/media/body/wire/question validation；
- parallel 快速 SERVFAIL/TC/REFUSED 与慢速完整回答；
- smooth weighted round-robin 长期分布和并发游标；
- weighted least-in-flight 的选择和平局；
- failover 只在 transport failure 切换；
- fallback 只在主组无 terminal response 时进入；
- request/group/fallback deadline 与 cancellation；
- late result 只影响 cache/observability。

## 11. 实现检查清单

- [x] 实现 hosts connector 与首轮 Registry factory；
- [x] 实现 DoH connector 的协议无关 exchange 与响应校验边界；
- [x] 实现 plain HTTP/1.1 DoH transport adapter；
- [x] 将 plain HTTP DoH connector 接入 Registry，并提供可注入 transport 构造入口；
- [x] 通过注入式 Registry 验证 PolicyCore direct DoH request path；
- [x] 抽出 DoH 地址解析 port，并验证 resolver 注入与 `connect_ip` 旁路；
- [x] 在 DoH request envelope 中透传 bootstrap 引用，并对未配置 bootstrap resolver 的默认路径 fail-closed；
- [x] 从已校验 DNS response 提取 bootstrap A/AAAA 与最低 TTL，并拒绝非正向响应；
- [x] 通过注入式 `DnsExchange` 执行 bootstrap A/AAAA 查询、地址合并和 transport/cancel/no-address 分类；
- [x] 将 bootstrap resolver 接入默认 DoH address resolver、Registry 和 plain HTTP 实际路径；
- [x] 解析 outbound SecretRef，固化 socks5/socks5h profile 与本地/远程目标规划；
- [x] 实现无网络副作用的 SOCKS5/SOCKS5H method、认证、CONNECT codec 与 reply parser；
- [ ] 为自定义 transport、HTTPS/TLS 和 outbound 提供完整 bootstrap 执行；
- [ ] 实现 SOCKS5/SOCKS5H stream 握手、认证 I/O 和 socket dial；
- [x] 固化四种 group 模式的纯 member selection；
- [x] 实现 outcome/fallback 判定边界；
- [ ] 接入 group exchange、fallback 执行与 late cache finalizer；
- [ ] 实现 late cache finalizer；
- [ ] 完成代理、TLS、算法和并发测试。

阶段证据：hosts/group/outcome 定向测试 19 项通过，`upstream::registry` 5 项通过，`upstream::doh` 7 项通过，`upstream::bootstrap::tests` 14 项通过，`upstream::http::tests` 7 项通过，`upstream::outbound::tests` 4 项通过，`upstream::socks5::tests` 6 项通过，PolicyCore focused tests 11 项通过；覆盖 Registry 的 plain HTTP DoH 构造、默认 Registry bootstrap loopback 路径与不支持能力拒绝、注入式 PolicyCore DoH request path、DoH request envelope、Host/SNI/connect_ip、resolver 注入与 `connect_ip` 旁路、bootstrap 引用透传与默认路径 fail-closed、bootstrap response 的 owner/TTL 提取、注入 connector 的 A/AAAA 查询与地址合并、SecretRef 解析和 credential Debug 脱敏、socks5/socks5h 目标解析模式、SOCKS5 method/auth/CONNECT codec 与 reply parser、plain HTTP/1.1 headers/body、chunked 拒绝、HTTPS 未接入和 cancellation。当前只验证了默认 Registry 的 plain HTTP bootstrap 路径、无真实网络的 PolicyCore direct path 与注入式 bootstrap connector path；SOCKS5/SOCKS5H 实际 stream handshake、socket dial、自定义 transport resolver、RuntimeSnapshot、真实 outbound 和 TLS/proxy 仍未实现。

当前实现进度：**79%**。
