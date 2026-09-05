# Upstream 模块设计

> 文档状态：有效
>
> 适用范围：upstream connector、bootstrap、outbound、group、并发选择和故障回退
>
> 最后评审：2026-09-05（parallel 择优、正常响应阻止 fallback、保留 late-result 与 primary lease；基线见[模块索引](README.md)，证据见[DNS 管线](../../../implementation/backend/dns-pipeline.md)）
>
> 关联实现：[registry.rs](../../../../backend/src/upstream/registry.rs)、[executor.rs](../../../../backend/src/upstream/executor.rs)、[group.rs](../../../../backend/src/upstream/group.rs)、[http.rs](../../../../backend/src/upstream/http.rs)、[reqwest_http.rs](../../../../backend/src/upstream/reqwest_http.rs)
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
| `group.rs` | weighted round-robin、primary load-balance 和确定性候选顺序 |
| `executor.rs` / `outcome.rs` | group/fallback 执行、terminal 分类、parallel task 与 late drain |
| `registry.rs` / `hosts.rs` | direct connector 注册与内存 hosts exchange |
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

目标地址选择是互斥分支，不是失败后逐项 fallback：

1. 显式 `connect_ip` 直接使用该 IP；
2. `socks5h://` 且无 connect_ip 时交给代理解析，同时禁止 bootstrap；
3. 其他模式有 bootstrap 时调用指定 connector；
4. 没有以上选择时才使用 system resolver。

`connect_ip` 只替换网络连接目标，不改变 URL host、Host header 或 SNI。

bootstrap：

- 通过引用 connector 查询 A/AAAA；
- `BootstrapResolver` 通过注入的 `DnsExchange` 顺序执行 A、AAAA 查询，合并合法地址并取最低 TTL；
- Registry 将 hosts/DoH connector 登记到仅持 Weak 引用的共享 bootstrap registry，并为每个 DoH connector 创建配置绑定的 `TokioDohAddressResolver`；
- 只接受完整、合法地址答案；
- `bootstrap_answer_from_response` 只提取与 question owner 匹配的 A/AAAA，并按地址记录的最低 TTL 建立答案；
- resolver 复用 `AddressResolutionState` / `AddressCachePolicy` 保存最多一项地址答案，命中时直接转换为请求端口，不再查询 A/AAAA；
- 无可用地址时本次 exchange 失败；
- dependency cycle 在 Config 阶段拒绝。

生产 TTL 下限为 0，上限为 3,600 秒，不沿用纯状态原语默认的 5 秒下限。答案记录单调时钟到期点，A 等待 AAAA 和写入状态的时间不能续长 A 的权威 TTL；零 TTL 或汇总时已过期的成功答案只供本次使用。`now >= expires_at` 即过期，不提供过期地址、负缓存或 system fallback。

每个 resolver 固定 upstream ID、host/port/bootstrap，拒绝运行时身份错配。并发 miss 通过一个异步许可串行查填，持有者完成后等待者复查缓存；Mutex 只保护短状态操作，不跨网络 await。等待和查询共用调用者原 deadline/cancellation，命中也不能掩盖超时/取消；持有者取消、超时或 drop 会释放许可，但后续请求可能重新查询。失败分支仅允许复用仍有效的旧地址，不延期。

transport clone 共用绑定 resolver，其他 connector 与重新 prepare 的候选均使用独立空缓存；旧请求只能更新旧状态。资源-only publish 复用原 connector 时保留其缓存。HTTP client pool 是另一层复用，其 key 包含目标地址列表；TTL 到期取得新地址后选用对应 client，而 Host/SNI 始终取 endpoint。显式 `connect_ip`、无 bootstrap 和 SOCKS5H 分支不缓存目标解析；无配置绑定的测试 resolver 维持无缓存语义。

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

group deadline 为请求剩余 deadline 与 `timeout` 的较小值。正常 DNS response（包括 NXDOMAIN/NODATA、REFUSED 和其他非 SERVFAIL/TC 回答）是 terminal response，阻止进入 fallback；SERVFAIL、TC 与可重试 transport failure 仍沿用下一成员/fallback 规则。

fallback 使用独立 `fallback_timeout`，但不能超过请求总 deadline。进入 fallback 后不再回到主组。

主成员和 fallback 成员的 `weight` 都可省略，并在配置输入边界归一化为 `1`。`round-robin` 与 `load-balance` 消费权重；`parallel` 与 `failover` 不消费权重，显式非 `1` 值在配置校验阶段拒绝。

一次 attempt 结果：

- terminal response：按模式规则立即返回或等待已启动的同阶段成员，不为等待更好答案另启 fallback；
- transport failure：可重试时继续候选；不可重试时顺序模式终止，parallel 中不屏蔽其他已启动成员的有效响应；
- cancelled：依据原因终止本组或单 attempt。

## 7. 选择算法

### parallel

- 同时发起全部成员，weight 省略或为 1；
- 无论是否带 `LateResultSink`，正常负答案都等待已启动的同阶段成员完成或阶段 deadline，允许更好的 Positive 胜出；已经收到正常回答时不启动 fallback；
- Positive 可以提前返回：带 sink 时移交剩余 JoinSet 继续收集，保留生产 late-result 能力；无 sink 的调用仍 abort 剩余任务；
- 成员不可重试失败不能遮蔽其他 parallel 成员已经取得的有效响应；不改变顺序模式的失败终止契约；
- late window 按配置顺序选择完整 NOERROR，再选择其他可缓存终态；
- 主组完全没有 terminal response 才进入 fallback。

### round-robin

- 使用 per-group `Mutex<SmoothState>` 实现 smooth weighted round-robin，不是原子游标算法；
- weight 决定长期被选为 primary 的频率；
- primary transport failure 后，按本次计算的确定性候选顺序尝试尚未使用成员；
- 遇到第一个 terminal response 即结束；
- 无 terminal response 才进入 fallback。

### load-balance

- 选择 `in_flight / weight` 最小的成员；
- 相同比值时使用轮转游标打破平局，避免永久偏向首项；
- executor 用 `SelectionLease` 给本次 primary 加一，lease 覆盖整段 ordered execution；重试成员不另加自己的 in-flight，因此这是 primary 占用估计，不是每个 connector 的真实活动 exchange 数；
- primary transport failure 后按配置顺序尝试其余成员，不在每次失败后重新计算 least-in-flight；
- terminal response 与 fallback 规则同上。

### failover

- 严格按配置顺序串行尝试；
- weight 省略或为 1，避免暗示不存在的流量权重；
- 可重试 transport failure、SERVFAIL 或 TC 才尝试下一成员；
- 正常 terminal response 都停止，包括 NXDOMAIN；不可重试 failure 也停止；
- 全部成员失败后进入 fallback。

## 8. 并发与取消

顺序模式直接 await exchange；parallel 模式由 executor 的 `JoinSet` 持有 attempt task，已移交的 drain 由 `LateCacheFinalizer` owner 管理，不逐项注册 Runtime Supervisor。Group aggregator 持有：

- group deadline；
- child cancellation tokens；
- responder 是否已完成；
- late cache finalizer；
- attempt outcome 列表。

客户端取消时：

- 没有 cache finalizer 价值的 attempt 立即取消；
- 已移交的 parallel late drain 可在既有预算内继续收集，Positive 首响应也保留该能力；不是为负答案额外延长 group deadline；
- shutdown 取消所有 late finalizer；
- late result 不能修改已返回客户端的 response。

## 9. Failure 与观测

TransportFailure 分类至少包括 connect、DNS bootstrap、proxy、TLS、HTTP status、media type、body limit、wire、question mismatch 和 timeout。

观测字段使用 upstream/group ID、outbound kind、attempt ordinal、outcome 和 latency bucket，不记录完整 URL、credential、query domain 或目标原始 IP。

v1 不实现主动健康检查、熔断器或持久健康分数。load-balance 只使用 primary lease 计数，不应在文档或指标中称为 health。当前也没有完整的独立 attempt telemetry 生产链路，见 [DNS Core](dns-core.md)。

实际 adapter 和正式接线见[DNS 管线实现](../../../implementation/backend/dns-pipeline.md)；真实 TLS、proxy、bootstrap 与跨 adapter 故障证据按[契约核对计划](../../../plans/backend-contract-gaps.md)补齐，不由本设计推断。

## 10. 契约验证要求

- DoH Host/SNI、connect_ip、bootstrap 和 system resolver；
- SOCKS5/SOCKS5H 本地/远程解析与 secret 脱敏；
- HTTP status/media/body/wire/question validation；
- parallel 快速 SERVFAIL/TC/REFUSED 与慢速完整回答；
- smooth weighted round-robin 长期分布和并发游标；
- weighted primary lease 的选择和平局、失败后确定性顺序，以及与真实逐 attempt in-flight 的区别；
- failover 只在 transport failure 切换；
- fallback 只在主组无 terminal response 时进入；
- request/group/fallback deadline 与 cancellation；
- late result 只影响 cache/observability。
