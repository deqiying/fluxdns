# DNS Core 模块设计

> 文档状态：有效
>
> 适用范围：canonical DNS message、请求管线、缓存交互和上游结果处理
>
> 最后评审：2026-09-05（模块边界与关键契约静态核对，基线见[模块索引](README.md)；不含运行验收）
>
> 2026-09-20 局部评审：仅核对 group ECS、Resolved key、stale refresh 与 late-result 编排；不代表整篇重审或运行验收
>
> 局部评审基线：`fcbb12831c66e81e02108ccd030ac3c2a1e08f56` 加本次工作树；仅用于上述范围
>
> 2026-11-09 局部评审：基线 `08a774724e2304f72dd7b8d0a9d0299983f9b0e5` 加工作树，仅核对完整决策、v3 AnswerKey 与 ECS scope 编排；未新增测试或运行验收。
>
> 关联实现：[policy.rs](../../../../backend/src/dns/policy.rs)、[handler.rs](../../../../backend/src/dns/handler.rs)、[service.rs](../../../../backend/src/service.rs)、[resolution.rs](../../../../backend/src/resolution.rs)
>
> 关联文档：[后端架构](../overview.md) · [Ports](ports.md) · [Policy](policy.md) · [Cache](cache.md) · [Upstream](upstream.md)

## 1. 目标

DNS Core 是 transport 无关的请求编排器。输入是 canonical query 和 request context，输出是 canonical response 或明确的“无需再响应”结果。

公开 `DnsCore` 契约不暴露 socket/HTTP/SQLite/Moka，不根据具体 UDP/TCP/DoH 类型分支；transport 差异通过 capability 和 response encoder 处理。正式启动由 async prepare 构造 `PolicyDnsCore`，Policy、资源、Cache、Upstream 和后台完成事件链路已经接通。

`dns/policy.rs` 同时承担部分组合根职责：构造具体 Upstream registry、Moka facade，并向进程级 cache snapshot owner 暴露受限 source；不能把“公共接口隔离”写成整个文件不依赖 adapter。它不再初始化 SQLite cache persistence。`HostsCore`/`ServFailCore` 保留为简化解析与契约测试实现；配置装配统一使用 `PolicyDnsCore`，不维护另一套仅处理 hosts 的配置装配层。

## 2. 内部结构

| 文件 | 职责 |
| --- | --- |
| `message.rs` | canonical query/response、DNS validation 和 response class |
| `context.rs` | request meta、client identity、deadline/cancellation |
| `handler.rs` | `DnsCore`、completion/observation、简化 Core 与 `dispatch_inbound` |
| `policy.rs` | 正式策略、资源状态、v3 答案缓存、上游、刷新与 adapter 装配 |
| `configured.rs` / `hosts.rs` | 简化配置 Core 与兼容 hosts 数据结构 |
| `service.rs` / `resolution.rs`（模块外） | completion 计时/发布和后台 stats/cache/detail 分发 |

可将纯函数拆成私有子模块，但不为每个 stage 创建空抽象层。

## 3. Canonical message

`CanonicalQuery`：

- 不包含客户端 DNS ID；
- QNAME 使用 DNS 规范化表示，比较时大小写不敏感并统一根点；
- v1 只接受 opcode QUERY 和恰好一个 question；
- 保留会影响答案的 RD、CD、DO 等标志；
- EDNS options 经过边界校验，ECS 在 policy 阶段重新计算；
- 不保留 transport envelope。

`CanonicalResponse`：

- 不包含客户端 DNS ID；
- 保留 question、answer/authority/additional、RCODE、AA/RA/AD/CD/TC；
- 附带解析后的 TTL metadata 和 response class；
- 只有通过问题段匹配和 wire 完整性验证的响应才能构造。

非法 query 在无需伪造 question 且 header 可靠时由 UDP/TCP Transport 返回 FORMERR/NOTIMP；短 header、`QR=1` 或需要合法 OPT 的 BADVERS 不猜测响应。DoH 非法 DNS wire 继续按 HTTP 400 分层。

## 4. 请求管线

```text
capture one RuntimeSnapshot
  → validate canonical query
  → prepare PolicyContext（client / strategy / namespace / TTL）
  → evaluate RouteDecision（rules / hosts / target）
  → local hosts answer OR prepare_query（最终 member ECS）
  → v3 AnswerKey scope/exact lookup → miss: single-flight / exchange
  → validate and classify response
  → create optional CacheCommitCandidate
  → apply client-visible TTL
  → return DnsCoreCompletion
  → service freezes timing and try_publish one ResolutionEnvelope
  → transport encodes canonical response
  → background: stats / cache commit / optional detail projection
```

Core 入口、cache/exchange 与 response correlation 按各自边界检查 cancellation/deadline；并非每个纯内存 stage 都有独立检查。下游 exchange 不能延长请求预算，后台 cache commit/optimistic refresh 使用明确的独立窗口。

## 5. Policy 结果

Policy 将原有完整决策拆为两个稳定阶段：

- `PolicyContext`：client bucket/identity、生效 strategy、cache decision/namespace、TTL override；Core 组合请求级 ECS；
- `RouteDecision`：matched rule/hosts/resource、upstream target、生效 ECS；Core 再准备最终 member ECS 和安全 observation。

Core 不再次计算继承，也不把 rule 文本写入日志。

当前 `HostsCore` 同时保留旧 `HostsTable` 兼容路径，并支持不可变 `Resource::HostsIndex`；命中后直接生成 A/AAAA/CNAME 本地响应，支持 exact/wildcard 优先级，未命中时按 NXDOMAIN/NODATA 语义返回。`PolicyDnsCore` 每次执行 `prepare_context → evaluate_route`，本地 hosts 命中绕过 response cache；其余完成 `prepare_query` 后才查答案缓存，不存在 Fast 分支或决策 memo。`DnsCore::resolve_with_completion` 返回响应、当前决策 observation 和可选 commit candidate；fresh/stale/single-flight hit 从 entry provenance 恢复缓存生产 target/actual，不用当前 route 猜测来源。

Upstream runtime 预记录 group 全部 direct leaves（primary/fallback/nested/隐式继承成员）。rule/strategy/client 显式 ECS 统一覆盖成员，否则逐 leaf 计算最终 ECS。uniform ECS 可按应答 scope 共享；异构成员保留 per-member query 并以排序组合摘要精确缓存，正常参与 single-flight 与 commit，不再绕过。

## 6. Cache 交互

只有需要上游交换的路径进入 CacheFacade：

- 完整决策后用 namespace、规范化 question、RD/CD/DO、保留 EDNS options、transport compatibility、target 子树 fingerprint 与最终 ECS 构造 `AnswerKey`；入口、policy/资源 hash 不入 key，EDNS 存在本身不入 key；
- fresh hit 直接使用 canonical response；
- stale hit 可以先返回，并调度 `schedule_optimistic_refresh`；
- uniform lookup 按 covering 前缀从长到短、最后 exact 探测；miss 按查询 ECS 精确值 single-flight，存储 key 待应答 scope 确定；无 ECS 桶和异构组合只精确匹配；
- waiter 取消只释放自身，不取消仍有其他 waiter 的 exchange；
- Core 把上游结果与 single-flight lease 封装为 `CacheCommitCandidate`，由统一解析事件移交后台 worker；worker 用独立 100ms deadline 完成准入、质量 CAS 和 follower completion。进程快照不在请求完成链逐条 enqueue。
- optimistic refresh 切到 latest core 重新准备；目标 fresh 则跳过，回源前记录目标 store 版本水位，写前复核 AnswerKey。新 scope key 写入成功后在 store 锁内确认新版本有效，再清理被覆盖且不晚于水位的旧条目，不删除刷新期间更新的条目；新条目失效则不清理，失败不延长旧 stale；
- late-result 保存 AnswerKey 而非 policy semantics，跨 runtime 重新决策与写前均要求答案身份相等；policy 变更但最终答案身份不变仍可接纳，不能仅凭 group ID 接纳。

Core 不直接调用 Moka 或持久化 store。

## 7. ECS

ECS 处理顺序：

1. Policy 计算 effective mode；
2. `disabled` 删除已有 ECS；
3. `client` 优先使用合法请求 ECS，source 截断到 IPv4 /24、IPv6 /56（较短值保留）、清除 host bits；无合法请求 ECS 时从 client address 推导同样前缀；
4. `custom` 使用已验证 CIDR，不按 client 上限截断；
5. 所有查询 ECS scope 置 0，并校验 family/prefix；
6. uniform 最终 ECS 进入 scope 缓存，异构组合进入精确缓存；无 ECS 使用独立桶，source 0 产生 exact /0，不能与应答 scope 0 covering /0 混用；应答缺失或不一致 ECS 按查询 source 存储，不一致应答仍返回。完整矩阵见 [Cache](cache.md#4-lookup-与-single-flight)。

客户端地址不可用且 mode=client 时，不伪造 `0.0.0.0/0`；按“无 ECS”继续并记录低基数原因。

## 8. 上游结果处理

`Response(CanonicalResponse)` 作为 terminal DNS response 进入：

- DNS ID/question/response flag 二次防御校验；
- response class 分类；
- TTL/negative TTL 计算；
- cache admission；
- client-visible TTL override。

全部 upstream attempt 都是 `TransportFailure` 时生成 SERVFAIL。client disconnect 或 shutdown cancellation 时不再尝试响应；request deadline 到期且连接仍有效时可以返回 SERVFAIL，但需由 Transport correlation 判断是否仍可写。

NXDOMAIN、REFUSED、SERVFAIL、TC 都是有效 DNS response，不转换成 adapter error。

## 9. TTL 处理

区分三种 TTL：

- origin TTL：上游或本地资源提供；
- cache expiry TTL：用于 entry 生命周期；
- client-visible TTL：按当前 override 和剩余 TTL 生成。

TTL override 作用于输出副本，不能污染 origin cache candidate 或延长 entry expiry。fresh response 按 `inserted_at` 后经过的整秒递减 RR TTL；stale response 先满足所选池的 max_age 并使用其 answer TTL，再应用当前请求的 min/max override。异步 CAS 与客户端输出之间没有必须等待写回的时序要求。

## 10. 事件

一条请求在 transport 编码前至多生成并无等待发布一个 `ResolutionEnvelope`。其中 `ResolutionEvent` 只保存冻结的终态与低基数/typed 数据：

- 进程内 request ID、listener/route、配置 client bucket 和 strategy；
- source：hosts、cache 或 upstream；
- target upstream/group 与实际产生结果的 direct/member；cache hit 为缓存生产来源；
- 仅在 `resolve_log` 开启时附带的 typed canonical question、有效 client IP 和共享 `Arc<CanonicalResponse>`；
- 终态、RCODE、cache status，以及在 core 返回时冻结的服务端总耗时和 DNS 主链耗时；
- cancellation/failure 分类；
- runtime/resource revision 摘要。

Policy Core 提供 `strategy`、策略目标 `upstream_id`、无歧义的 `upstream_used_id`、`source`（hosts/cache/upstream）、lookup `cache_status`、配置 client bucket，以及 matched rule/resource/version。service 补充 typed canonical question、共享逻辑 response 和 `RequestContext.client.client_addr`，但不在请求任务中生成 request digest、qname/answer 字符串或 JSON。后台 dispatcher 更新 stats、提交 cache candidate，并仅在启用时把事件交给 detail projector；rule matcher、request digest、DNS wire、header 和请求级值不会进入普通日志或 metrics。

service 紧贴 `DnsCore::resolve_with_completion` 调用前后记录主链耗时，并以 transport 提供的 `received_at` 计算截至同一完成时刻的服务端总耗时。两个值作为数值跨越异步 observation 边界，不传递 `Instant`，因此 dispatcher 排队、detail projector 和 SQLite 写入不会污染请求耗时。

resolution ingress 满时 DNS 响应照常编码，但整个 envelope 被丢弃并累计 `dropped` 与首次 gap 时间；cache candidate drop 通过 RAII 释放 single-flight follower。detail 与 cache 下游队列失败只影响各自消费者。cache lookup 状态只表示响应完成前已知的 hit/miss/stale，异步 commit 结果以独立 counter 记录。

当前 dispatcher 从这条完成事件生成一次 total 和各统计维度；名为 `attempt_outcome` 的维度也使用请求终态，不是逐 upstream attempt 计数。executor 的 attempt 列表和 late candidate 尚未形成独立的生产 attempt 事件流，不能用字段名称推断已有完整 attempt telemetry。

## 11. 错误语义

- 用户输入 DNS 错误：尽可能返回 FORMERR/NOTIMP/BADVERS；
- 策略未找到目标：prepare 应已阻止；运行时出现则为 internal + SERVFAIL；
- 全部 transport failure：SERVFAIL；
- deadline/cancel：按 correlation 状态返回或静默结束；
- resolution ingress、telemetry 或 detail 写入失败：不改变 DNS 结果，并以独立 gap/counter 暴露；
- cache/store 失败：按 Cache 降级语义继续上游或返回已得到响应。

## 12. 契约验证要求

- opcode、question count、EDNS version 和 canonical normalization；
- 同一 query 经 UDP/TCP/DoH 得到同一 Core 决策；
- client/strategy/rule/hosts/upstream 管线顺序；
- ECS 各层覆盖、全部 direct leaf 的最终 query 一致性与 cache key；
- v3 AnswerKey 跨入口共享、target 子树定向失效、RD/CD/DO/EDNS 规范化、scope/exact 及无 ECS 桶隔离；
- local hosts 绕过 cache 且资源更新立即生效；
- fresh/stale/miss/single-flight/cancellation，以及最新 core 下重新决策的统一刷新流程；
- 异步 commit 成功唤醒 follower，candidate drop 释放 lease；
- NXDOMAIN/NODATA/SERVFAIL/REFUSED/TC 分类；
- TTL override 不延长 cache expiry；
- 全部 upstream failure 和内部不变量破坏；
- 一请求只记录一次 total stats。
- 一请求只发布一次 typed completion，响应对象与详情 source 共享同一 `Arc`；
- resolution ingress/detail/cache commit 有界队列的丢弃与独立计数。
- refresh/late-result 在 latest core 重新准备并按 AnswerKey 接纳；刷新水位清理保护并发更新，新条目失效则不清理，失败不延长 stale。
