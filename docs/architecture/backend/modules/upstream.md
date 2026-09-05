# Upstream 模块设计

> 文档状态：有效
>
> 适用范围：upstream connector、bootstrap、outbound、group、并发选择和故障回退
>
> 最后评审：待核对（本次仅分类与边界复核，不等同完整契约重审）
>
> 关联实现：`backend/src/upstream/*`
>
> 关联文档：[后端架构](../overview.md) · [配置字段参考](../../../implementation/configuration.md) · [Ports](ports.md) · [Policy](policy.md) · [Cache](cache.md)

## 1. 职责

Upstream 模块把 typed upstream 配置编译为 `UpstreamRegistry` 和可复用 `UpstreamConnector`，并实现单上游交换、组选择、bootstrap 与 outbound。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `doh.rs` | DoH connector、HTTP/TLS/DNS response validation |
| `http.rs` | plain HTTP/1.1 与独立 SOCKS5/SOCKS5H adapter |
| `reqwest_http.rs` | direct/proxy HTTP/HTTPS、TLS、地址覆盖和有界 client pool |
| `group.rs` | parallel、round-robin、load-balance、failover、fallback |
| `bootstrap.rs` | 上游主机名解析与地址 override |
| `outbound.rs` | direct、SOCKS5、SOCKS5H profile 和 SecretRef |

`upstreams[type=hosts]` 作为内存 connector 实现同一 `DnsExchange`，主要用于 bootstrap，也可被 strategy 直接引用。

## 2. Registry 与 connector

prepare 为每个 upstream 生成 typed ID 和 connector handle，组合以下职责：

- hosts connector；
- DoH connector；
- 地址解析 port；
- group selection policy；
- outbound profile；
- bootstrap dependency；
- 安全的观测标签。

Registry 在构造时拒绝缺失/重复 connector、非法 outbound、循环和不兼容组合，返回稳定 typed error；不能等到每次请求才解释配置。Group 可递归组合，但必须保留 cycle guard 和嵌套 late-result sink，不在 GroupExchange 边界丢弃晚到候选。具体生产与测试构造器的支持范围见[DNS 管线实现](../../../implementation/backend/dns-pipeline.md)。

connector 构建 key 至少包含 upstream、outbound、bootstrap/connect_ip 和 TLS/HTTP profile。相同 key 复用 client 和连接池。

DNS Core 只持有 typed connector/group handle，不读取 URL 或 proxy 配置。

## 3. DoH connector

HTTP adapter 负责有界 header/body、连接、写入、读取与 deadline/cancellation。地址解析通过 port 注入；配置了 bootstrap 却缺少有效 resolver 时 fail-closed，不偷偷回退 system resolver。显式 connect_ip 只替换连接目标，不能触发目标 hostname resolver。plain HTTP 与 HTTP/HTTPS adapter 可以共存，但具体支持范围必须由 Registry 显式选择，不隐式接管不支持的协议。

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

`OutboundProfile` 在 prepare 边界解析 SecretRef，保存 scheme、代理端点和受保护凭据；userinfo 百分号解码后必须有长度限制，Debug 只显示长度。`OutboundTarget` 固化 host/port、connect_ip、bootstrap 与解析模式。proxy resolver、target resolver、dialer、SOCKS codec 和 HTTP exchange 分工，沿同一 deadline/cancellation 组合，避免各 adapter 自行解释冲突的代理语义。

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
- `perform_handshake` 按 response address type 读取固定或可变长度 frame，区分 credentials 缺失、proxy reply、clean EOF 和底层 port 错误；
- `Socks5Connector` 只接受已解析的 proxy `SocketAddr`，成功路径为 dial → method/auth → CONNECT；dialer 将 Tokio I/O 错误转换为不含原始地址和凭据的 `PortError`；
- `connect_profile` 只传递已 prepare 的 `OutboundCredentials`，非法百分号、空 userinfo 和超限字段在 profile 边界拒绝；
- `connect_profile_with_resolver` 对 proxy hostname 只接受 bounded `SocketAddr` 候选，并在无地址时返回稳定的 proxy resolve failure；
- codec 不创建连接，也不记录 credential 或完整 domain 内容。

## 6. Group 总体语义

group deadline 为请求剩余 deadline 与 `timeout` 的较小值。任何合法 DNS response 都是 terminal response；只有全部 attempt 都没有 terminal response 时才进入 fallback。

fallback 使用独立 `fallback_timeout`，但不能超过请求总 deadline。进入 fallback 后不再回到主组。

主成员和 fallback 成员的 `weight` 都可省略，并在配置输入边界归一化为 `1`。`round-robin` 与 `load-balance` 消费权重；`parallel` 与 `failover` 不消费权重，显式非 `1` 值在配置校验阶段拒绝。

一次 attempt 结果：

- terminal response：立即按模式规则结束或进入 parallel late window；
- transport failure：该成员失败，可继续其他成员；
- cancelled：依据原因终止本组或单 attempt。

## 7. 选择算法

### parallel

- 同时发起全部成员，weight 省略或为 1；
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
- weight 省略或为 1，避免暗示不存在的流量权重；
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

实际 adapter 和正式接线见[DNS 管线实现](../../../implementation/backend/dns-pipeline.md)；真实 TLS、proxy、bootstrap 与跨 adapter 故障证据按[契约核对计划](../../../plans/backend-contract-gaps.md)补齐，不由本设计推断。

## 10. 契约验证要求

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
