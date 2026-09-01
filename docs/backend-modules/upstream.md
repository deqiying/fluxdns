# Upstream 模块设计

> 状态：v1 方案已完成，已实现内联 hosts exchange、可注入 DoH exchange、hosts registry、纯 group member selection 和 outcome/fallback 判定；真实 HTTP/TLS adapter、bootstrap/outbound 尚未实现
>
> 更新日期：2026-08-31
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
| `group.rs` | parallel、round-robin、load-balance、failover、fallback |
| `bootstrap.rs` | 上游主机名解析与地址 override |
| `outbound.rs` | direct、SOCKS5、SOCKS5H profile 和 SecretRef |

`upstreams[type=hosts]` 作为内存 connector 实现同一 `DnsExchange`，主要用于 bootstrap，也可被 strategy 直接引用。

## 2. Registry 与 connector

prepare 阶段为每个 upstream 生成 typed ID 和 connector handle。当前 registry 首轮只构造已验证的 hosts connector；DoH connector 已独立提供可注入 transport 边界，尚未接入 registry：

- hosts connector；
- DoH connector；
- group selection policy；
- outbound profile；
- bootstrap dependency；
- 安全的观测标签。

`UpstreamRegistry::from_resolved` 对尚未实现的 DoH/Group 返回显式 `UnsupportedUpstream`，不会静默丢弃配置；`ConfigId` 到 `ConnectorId` 的不兼容字符也在构建边界返回稳定错误。

connector 构建 key 至少包含 upstream、outbound、bootstrap/connect_ip 和 TLS/HTTP profile。相同 key 复用 client 和连接池。

DNS Core 只持有 typed connector/group handle，不读取 URL 或 proxy 配置。

## 3. DoH connector

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
- 只接受完整、合法地址答案；
- 地址按 DNS TTL 缓存，并设置实现级最小/最大 refresh 边界；
- 刷新失败时可在未过期窗口内继续使用旧地址并标记 degraded；
- 无可用地址时本次 exchange 失败；
- dependency cycle 在 Config 阶段拒绝。

系统 resolver 只在未配置 bootstrap/connect_ip/proxy remote resolve 时使用，不作为任何失败路径的隐式 fallback。

## 5. Outbound

`socks5://`：

- 目标主机在本地按 connect_ip → bootstrap → system resolver 解析；
- 代理只看到目标 IP；
- credential 从 SecretRef 读取，不能进入 Debug/日志。

`socks5h://`：

- 无 connect_ip 时把 URL host 交给代理解析；
- 禁止同时配置 bootstrap；
- 有 connect_ip 时代理连接该 IP，Host/SNI 仍是 URL host。

Secret 变化在 v1 需要构建新 candidate，不对现有 client 原地修改 credential。

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

当前已实现：`DohExchange` 固定 POST `application/dns-message` 请求，自动分配内部 DNS ID，保留 URL host 作为 Host/SNI，并将显式 `connect_ip`、deadline、cancellation 和 HTTP/协议错误映射到 `UpstreamOutcome`；`GroupSelector` 只负责无网络副作用的成员选择，提供 failover/parallel 配置顺序、smooth weighted round-robin、weighted least-in-flight、平局轮转和 `SelectionLease` 生命周期；`outcome` 提供按 attempt index 的 terminal/retryable/cancelled 聚合和 fallback 判定。真实 HTTP/TLS/socket adapter、bootstrap、late cache finalizer 和 Runtime/DNS Core 接线尚未接入。

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
- [ ] 实现 bootstrap/connect_ip/outbound；
- [x] 固化四种 group 模式的纯 member selection；
- [x] 实现 outcome/fallback 判定边界；
- [ ] 接入 group exchange、fallback 执行与 late cache finalizer；
- [ ] 实现 late cache finalizer；
- [ ] 完成代理、TLS、算法和并发测试。

阶段证据：hosts/group/outcome 定向测试 19 项通过，另有 `upstream::doh` 7 项通过，覆盖 DoH request envelope、Host/SNI/connect_ip、HTTP/协议响应校验、timeout 映射和 URL 安全边界。当前实现未接入 Runtime/DNS Core，也未执行真实出站网络 I/O。

当前实现进度：**50%**。
