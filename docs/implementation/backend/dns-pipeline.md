# DNS 请求管线实现

> 文档状态：有效
>
> 适用范围：正式 transport、Policy、Cache、Upstream 与单次完成事件链路
>
> 最后核对：2026-09-05（UTC；late-window 组合、本地 adapter 与显式会话容量验证）
>
> 核对基线：`f65fb3f8bd68e1a40ca041d9a380859b44a3da0c` 加本次契约验证工作树

## 入口与调用链

[`service.rs`](../../../backend/src/service.rs) 根据 typed binding 启动 UDP、TCP 和 DoH task；`run_adapter_loop` 把 adapter 产出的请求交给 instrumented core，再编码写回。TCP/DoH listener 用内部 `JoinSet` 管理 session，不把 DoH 当成 raw DNS/TCP。

```text
SystemSocketFactory / typed binding
 -> transport adapter -> DnsRequest + RequestContext
 -> instrumented DnsCore::resolve_with_completion
 -> PolicyDnsCore -> PolicyContext -> fast cache
 -> miss: RouteDecision -> hosts / resolved cache / UpstreamRegistry
 -> canonical response + completion
 -> ResolutionEventSink::try_publish -> transport response encoding
```

[`transport/wire.rs`](../../../backend/src/transport/wire.rs) 分离请求 DNS ID 与 canonical message；response encoder 恢复本次 ID、TCP framing 或 DoH envelope，UDP 按 EDNS 尺寸截断副本，不污染共享缓存。

![DNS 请求主链与后台消费流程](dns-query-pipeline.svg)

图中三个消费分支不表示三条额外下游队列；实际队列是 resolution ingress、cache commit 和详情投影，stats 由 dispatcher 直接累加，详见[后台服务](background-services.md)。

## Policy 与实际 core

正式 async prepare 在 [`dns/policy.rs`](../../../backend/src/dns/policy.rs) 构造 `PolicyDnsCore::from_config_with_resource_snapshots`；不另设仅处理 hosts 的配置装配层。`PolicyContext` 先选 client/strategy/cache namespace，Core 再结合 PolicyState 计算 ECS、eligibility 和 fingerprint；fast miss 后再求 `RouteDecision`，调用 [`policy`](../../../backend/src/policy) 的 client/strategy/route 逻辑。

配置 route 由 [`config/doh_route.rs`](../../../backend/src/config/doh_route.rs) 共享编译，DoH adapter 匹配真实路径后传 typed route ID，Policy 不重新匹配 URL。资源-only publish 更新 core 内的资源 snapshot，后续请求使用新 hash；不依靠全局 cache clear。

`dns/policy.rs` 同时含具体 adapter 的构造代码，包括 `UpstreamRegistry`、Moka 和 SQLite cache；解析方法通过 port 使用它们。不能把设计中的“公共接口不泄漏 adapter 类型”扩大为“整个 dns 源目录不 import adapter”。

`MemoryCacheStore`、`InMemoryStorageBackend` 和 `HostsCore`/`ServFailCore` 不在正式请求装配中。前两者用于与 Moka/SQLite 共用的 adapter 契约测试；后两者用于简化解析、dispatch/Transport 测试。它们不是查询性能优化，也不应为了清理名称相似的代码而删除生产 `MokaCacheStore`、`SqliteStorageBackend`、`PolicyDnsCore` 或 hosts upstream 使用的 `HostsTable`。

client CIDR 按前缀长度降序扫描；hosts 使用 BTreeMap，rule exact/suffix 使用 BTreeSet，随后依次匹配 keyword/受限 regex。当前没有 CIDR/suffix trie。PolicyState 的 matcher/version/hash 一起发布，Runtime metadata 随后更新，不是跨两个 ArcSwap 的事务。

## Cache 与 TTL

`build_cache_facade` 的生产默认是 [`MokaCacheStore::with_max_weight`](../../../backend/src/cache/moka.rs)，不是测试用 [`MemoryCacheStore`](../../../backend/src/cache/memory.rs)。共享 store 通过 namespace 形成逻辑池；`cache/key.rs` 的 v2 编码隔离 Fast/Resolved。当前 fingerprint 包含 PolicyState 中全部 hosts/rule-set hash，不仅限于本请求依赖的资源；纯观测或整个 runtime revision 不直接作为全局失效维度。

- listener/strategy hosts 本地回答绕过 response cache；upstream hosts 按普通上游响应处理。
- group member ECS 在选择前不确定且无上层覆盖时，绕过 lookup、single-flight 和写入，避免只按 group ID 混用响应。
- 缓存候选使用 canonical upstream response 与 origin TTL；返回时递减 TTL 或应用 effective override。stale 返回受当前 pool 的 optimistic max age 与 answer TTL 限制。
- optimistic refresh 通过有界 finalizer 重新使用 core 的最新资源/策略决策，不复用 entry 保存的旧 connector。配置切换期间的候选和 owner 行为见 lifecycle/background 文档；不能把“最新资源”推断为所有 late-window 跨 revision 场景均已验收。
- Core 将持有 single-flight lease 的 `CacheCommitCandidate` 随完成事件交出；[`resolution.rs`](../../../backend/src/resolution.rs) 的 cache worker 在独立 deadline 内 CAS。candidate drop 必须唤醒 waiter，响应不等待写回。

持久化启用、恢复、队列和失败见[后台服务](background-services.md)；准入与终态约束见 [Cache 设计](../../architecture/backend/modules/cache.md)。

## Upstream 与出站

[`UpstreamRegistry::from_resolved_with_outbounds`](../../../backend/src/upstream/registry.rs) 在 core 构造时装配真实 connector；HTTPS 使用 [`ReqwestDohHttpTransport`](../../../backend/src/upstream/reqwest_http.rs)。[`tokio_outbound.rs`](../../../backend/src/upstream/tokio_outbound.rs)、[`socks5.rs`](../../../backend/src/upstream/socks5.rs)、[`bootstrap.rs`](../../../backend/src/upstream/bootstrap.rs) 提供系统 TCP、代理协商和 bootstrap 解析，Host/SNI 与连接目标保持各自语义。不是只有选择器或 fake、没有真实 I/O 的状态。

[`group.rs`](../../../backend/src/upstream/group.rs) / [`executor.rs`](../../../backend/src/upstream/executor.rs) 分离成员选择、响应择优、fallback 与 late cache candidate。正常回答（包括 NXDOMAIN）阻止 fallback；SERVFAIL/TC 与可重试失败沿用既有继续规则。parallel 在已启动的主成员中等待更好的 Positive，到阶段结束/超时后使用已收到的正常回答，不为了择优启动 fallback。late result 不改变已返回响应；主动健康检查和持久健康分数没有接入。

`from_resolved` 和正式 `from_resolved_with_outbounds` 都为每个 DoH connector 创建 [`TokioDohAddressResolver::for_upstream`](../../../backend/src/upstream/http.rs)，绑定 host/port/bootstrap 与 upstream ID。resolver 内通过 `AddressResolutionState` 查填最多一项地址答案，TTL 内不重复查询 A/AAAA；它不是 Moka response cache，也不接 Storage。

[`BootstrapResolver`](../../../backend/src/upstream/bootstrap.rs) 在每族合法回答收到时记录单调时间到期点，汇总及填状态不延长较早答案。生产 TTL 采用 0..3,600 秒，精确到期失效；零 TTL/汇总已过期的答案只供当前请求。异步 Semaphore 串行查填，等待者取得许可后复查状态；取消、超时或 drop 不泄漏许可。所有等待与查询沿用原调用者预算，命中也检查预算。错误不隐式回退 system resolver，不服务过期地址。

connector clone 共享同一 resolver；重新 prepare 创建新状态，即使配置 ID 相同也不会接收旧请求写回。Reqwest 按新地址列表选择 client pool key，Tokio direct/SOCKS5 则将新 IP 交给连接层，HTTP Host/TLS SNI 不变。connect_ip、SOCKS5H、无 bootstrap 维持原分支；未绑定配置的基础 resolver 仍不缓存。详细不变量见 [Upstream](../../architecture/backend/modules/upstream.md)。

parallel 的上述择优不依赖 sink 是否存在。Positive 提前返回时，有 late sink 就把剩余任务移交 drain，保留生产 late-result 收集；无 sink 则 abort 剩余任务。不可重试的某个 parallel 成员失败不能遮蔽其他成员有效响应，顺序模式的终止契约不变。load-balance 继续统计 primary lease，失败后按配置顺序重试，不等同逐 attempt least-in-flight。

## 能力与证据

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| UDP/TCP/DoH | `transport/udp.rs`、`tcp.rs`、`doh.rs` | service 的 typed binding 与 session loop | 本次完整测试包含跨 UDP/TCP/DoH GET/POST 用例 | 本地 loopback，不是远程矩阵；DoH 入站非 HTTP/2 |
| TLS / 客户端地址 | system socket TLS、DoH forwarded/PROXY parser | DoH accept 后先可信 PROXY、再 TLS、再 HTTP | 本轮核对生产分支 | 真实代理、证书和故障组合仍需环境验收 |
| Moka / SQLite cache | `build_cache_facade`、`initialize_cache_persistence`、增量 SQLite writer | async prepare 默认构造 | schema v1 升级、增量写触发器、失败回滚与已有 adapter 契约测试 | 保留插入时间淘汰；真实 disk-full 与组合 late-window 的后续证据见[契约验证开发计划](../../plans/backend-contract-validation.md) |
| Policy -> 出站 | core -> registry -> protocol-independent connector | 正式配置构造支持真实 HTTP/代理路径 | 本轮静态，无远程请求 | 不等同所有 SOCKS/Host/SNI 组合已实测 |
| 单次完成事件 | service instrumented core、resolution publisher | core 返回后、编码前无等待移交 | 本轮核对调用位置 | ingress 满会出现可观测 gap，不能承诺零丢失 |
| bootstrap 地址缓存 | 配置绑定 resolver、绝对到期点、查填许可 | 两个配置工厂均装配，direct/HTTPS/SOCKS5 共用 | [address_cache_tests.rs](../../../backend/src/upstream/address_cache_tests.rs)；registry 的正式 hosts bootstrap/代理测试 | 单 connector 单项；不缓存 system lookup、负答案或过期地址 |

2026-09-05 的完整测试及命令记录统一见[后台服务验证](background-services.md#本次验证)。地址缓存用可控 Clock 验证 TTL 0/1 秒/上限、A/AAAA 时间差、并发查填、各方取消/超时/drop、配置代际隔离；真实 loopback HTTP/HTTPS 检查地址更新与 Host/SNI，SOCKS5 检查 CONNECT 中的新目标 IP。没有执行真实远程 DoH、代理服务或 bootstrap 长期性能压测。

## 契约验证补充

本节对应 `f65fb3f8bd68e1a40ca041d9a380859b44a3da0c` 之后工作树；运行命令、基线指纹和实际结果统一见[契约验证运行入口](background-services.md#契约验证运行入口)。测试编号维护在对应测试注释中，不新增生产 conformance 注册层。

### Late-window 与 owner

| 用例 | 生产入口与组合 | 预期终态 |
| --- | --- | --- |
| V2-L01 | `dns::policy::tests::contract_v2_nested_positive_reload_and_shutdown_keep_response_immutable`；真实 Policy/nested parallel group/late sink，primary 提交/丢弃 × 同代/换代 × late 前停机，共八种组合 | 两个 follower 中取消一个不影响另一个；primary 完成/丢弃即结束 single-flight，不延长到 late drain。换代后的 late 写入新 cache；同代已有同质量 Positive 时不覆盖；先停机则无晚写入，已响应内容始终不变 |
| V2-L02 | `upstream::executor::tests::contract_v2_cancel_after_negative_stops_pending_primary_without_fallback`；NoData/NXDOMAIN 先到，另一 primary 用 gate 保持在途，再交叉客户端断开/停机取消 | 返回原取消原因，fallback 调用数为零，primary selector 的 in-flight 归零 |
| V2-Q01 | `resolution::tests::contract_v2_rejected_envelope_releases_followers_and_accounts_queue_gap`；真实 publisher 与 Moka，分别使用 revision 1/2 的 producer | full/closed/stopped 均丢弃 candidate 并唤醒两个 waiter，可重新取得 leader；full 明确增加 ingress gap 和 cache commit dropped，closed 返回错误，stopped 返回 Disabled |
| V2-O02 | `cache::service::tests::contract_v2_finalizer_terminal_matrix_releases_moka_waiters` | 容量拒收、shutdown 拒收、任务 panic 和 drop 唤醒 Abandoned；成功为 Ready、准入拒绝为 Miss；回收后 active 为零。内部 panic 用例只证明回收，不证明自动恢复或升级 |

当前/历史 owner 在首次 poll 前 abort 的回归及生产修复见[生命周期](lifecycle.md#契约验证补充)。原有 `parallel_waits_for_positive_after_terminal_negative_with_or_without_sink`、`parallel_negative_blocks_fallback_even_when_other_primary_times_out`、`nested_parallel_group_propagates_late_attempt_to_outer_sink` 和 single-flight adapter suite 继续复用。V2-L01 的 primary 来源按现有契约保存顶层成员 `inner`，late attempt 保存实际 connector `remote`，没有扩展为逐 attempt trace；八种定向交互不宣称穷举所有因素排列。

### Adapter 支持矩阵

| 组合 | 可重复证据 | 边界 |
| --- | --- | --- |
| 入站 UDP / TCP / plain DoH GET、POST | `service::tests::udp_tcp_and_plain_doh_follow_the_same_dns_contract` 与 error response suite | 真 loopback；同时验证 DNS ID、RCODE、Positive/NoData/NXDOMAIN、UDP 截断与其他 transport 一致性 |
| direct HTTP + `connect_ip` | `reqwest_http::tests::posts_with_connect_ip_and_preserves_http_envelope` | 真 HTTP 请求 Host 与 body，不仅检查配置 |
| direct HTTPS + `connect_ip` | `performs_live_https_tls_handshake_with_verified_host`、V3-H02 | 真 TLS；自签测试根受信、未受信与域名不匹配分别验证，不依赖外部信任链 |
| HTTP / HTTPS + SOCKS5 + bootstrap | V3-H01 `contract_v3_proxy_target_host_sni_and_tls_order_matrix` | 注入地址 resolver 返回 loopback IP；真实 SOCKS CONNECT 为 IPv4，HTTP Host 与 HTTPS SNI 保持原域名，TLS 发生在 CONNECT 之后 |
| HTTP / HTTPS + SOCKS5H 域名 | 同 V3-H01 | CONNECT 为 domain，不调用本地目标 resolver；真实 TLS 校验原域名 |
| HTTP / HTTPS + SOCKS5H + `connect_ip` | 同 V3-H01 | CONNECT 为显式 IPv4，Host/SNI 不变；不误用代理侧域名解析 |
| bootstrap 地址缓存及更换目标地址 | [`address_cache_tests.rs`](../../../backend/src/upstream/address_cache_tests.rs) 与 registry 既有 suite | FakeClock/解析夹具与真实 HTTP/HTTPS/SOCKS 出站分层验证，不能称为真实远程 DNS 解析验收 |
| 无 TLS 能力的 Tokio HTTP adapter 收到 HTTPS；无效 outbound/不支持组合 | `https_requires_a_tls_adapter_and_cancellation_is_observed`、`config_aware_registry_rejects_missing_or_invalid_outbound_profiles` | 保留拒绝分支，不增加协议或 adapter |
| 出站 TCP stream | V3-O01/O02，位于 [`tokio_outbound.rs`](../../../backend/src/upstream/tokio_outbound.rs) | 真 dialer/read/write、读取上限、EOF、已 poll 的 read 取消、已过期拨号预算和非法 resolver 输入 |
| HTTP body 读取 | V3-H03 `contract_v3_partial_body_cancel_and_eof_do_not_report_success` | 真部分 body；进入 read Pending 后取消保留原因，或耗尽从建 client/send 开始共享的原 deadline；EOF 不返回成功 |
| TLS / 响应头等待 | V3-D01 `contract_v3_tls_and_headers_wait_share_original_deadline` | 服务端实际收到 ClientHello 或完整 HTTP 请求后暂停；取消原因与原预算超时分别验证，不用预取消冒充在途取消 |
| SOCKS 分段预算 | V3-D02 `socks5::tests::contract_v3_socks_stages_preserve_dial_budget_and_cancellation` | 真实 dialer/stream，依次在 method、userpass、CONNECT 卡住响应；每次 read/write 核对与拨号同一绝对 deadline，已完成阶段不重置预算，取消与超时各覆盖三个位置 |

V3-H03 的截断 body 是**现状分类断言**：锁定 Reqwest 的 `Response::chunk` 将断流包装为 decode error，当前 `map_reqwest_error` 映射 `Internal`，DoH connector 将其视为不可重试；本轮没有改成 `Unavailable` 或扩大重试。若要将此类输入细分为协议错误或可重试读取失败，须先确认分类/策略，不把本表作为该调整已验收的依据。

入站 TLS/PROXY 顺序、坏握手隔离以及 forwarded 信任链继续复用 [`transport/doh.rs`](../../../backend/src/transport/doh.rs) 的真实/合成夹具；SOCKS codec 的坏 reply、EOF 与凭据边界继续复用既有 suite。真实挂起拨号的在途取消、不同 OS 的网络终态和外部 TLS/代理仍未完成对应环境实测；不能用成功拨号、预过期预算或本地 handshake 结果替代这些证据。

### 真实会话边界

显式入口为 `pwsh -File script/test-backend-contracts.ps1 -Suite Connections`。V6-C01 位于 [`service.rs`](../../../backend/src/service.rs)，默认 `ignore`，只有显式选择才运行。使用真实 TCP/plain DoH listener 和受控 core；每条请求进入 core 时交出独立 oneshot，区分“TCP connect 已完成”与“已 accept 并进入请求处理”。

用例分别确认 1,023 和 1,024 个在途会话，超额请求保持等待；释放指定会话后，超额请求进入 core 并返回 HTTP/DNS 正确结果。停机后 request guard、Supervisor task 归零，并释放引用后重绑原端口。测试保留正式 `DEFAULT_REQUEST_TIMEOUT`，外层 60 秒 watchdog 与业务预算独立。

本机三次独立容量/恢复运行不等于长期压力或完整 V6 验收；plain DoH 只借用 external endpoint 的 HTTP 层，未部署外部 TLS 终止代理。满载/reload/重连混合周期、OS 句柄趋势、慢握手/慢 body 组合和真实 external 信任链仍须按[活动计划](../../plans/backend-contract-validation.md)验收。
