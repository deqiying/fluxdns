# DNS 请求管线实现

> 文档状态：有效
>
> 适用范围：正式 transport、Policy、Cache、Upstream 与单次完成事件链路
>
> 最后核对：2026-09-05（parallel 响应择优、late-result 保留及缓存增量持久化；其他链路沿用既有核对）
>
> 核对基线：`19c3c81e4fdbea9424d522620ad81462c6d22eb1` 加本次后端契约实施工作树

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
