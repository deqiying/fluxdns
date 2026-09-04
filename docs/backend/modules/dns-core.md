# DNS Core 模块设计

> 文档状态：有效
>
> 实现状态：已实现
>
> 适用范围：canonical DNS message、请求管线、缓存交互和上游结果处理
>
> 最后核对：2026-09-04
>
> 关联实现：`backend/src/dns/*`
>
> 关联文档：[后端架构](../architecture.md) · [后端开发计划](../development-plan.md) · [Ports](ports.md) · [Policy](policy.md) · [Cache](cache.md) · [Upstream](upstream.md)

## 当前实现边界

v1 方案已完成，已实现 canonical message、固定 SERVFAIL、内联 hosts、Resource hosts index、Policy upstream path、Cache fresh/miss/single-flight/CAS、optimistic refresh、SQLite cache non-blocking write/recovery 和 resolution observation；observation 已拆分策略 target 与 actual upstream，cache entry 保存 producer upstream provenance，service 从最终逻辑响应提取有界 answer 并记录有效 client IP。配置选中的 TTL override 已在缓存写入后统一应用到 hosts、upstream 和 cache response 的全部 RR，fresh/stale cache response 已应用剩余 TTL 与 optimistic answer TTL；Runtime 已持有资源摘要并由 service 捕获同 revision core，Policy compiled resource live swap 和配置候选 reload 已接入。

## 1. 目标

DNS Core 是 transport 无关的请求编排器。输入是 canonical query 和 request context，输出是 canonical response 或明确的“无需再响应”结果。

Core 不读取配置文件，不持有 socket/HTTP client/SQLite/Moka，也不根据具体 UDP/TCP/DoH 类型分支。当前 `ConfiguredDnsCore` 在已解析配置中选择内联 hosts；未形成完整 Policy/Upstream 链路时使用确定性的 `ServFailCore` fallback。transport 差异通过 capability 和 response encoder 处理。

## 2. 内部结构

| 文件 | 职责 |
| --- | --- |
| `message.rs` | canonical query/response、DNS validation 和 response class |
| `context.rs` | request meta、client identity、deadline/cancellation |
| `handler.rs` | 请求管线、port 编排和最终事件 |

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
  → match client
  → resolve effective policy
  → evaluate listener hosts / strategy rules
  → local hosts answer OR select upstream target
  → select one cache namespace
  → cache lookup
  → exchange/group resolve on miss
  → validate and classify response
  → cache admission / CAS
  → apply client-visible TTL
  → emit stats/detail event
  → return canonical response
```

每个 stage 检查 cancellation 和剩余 deadline。deadline 不足时直接产生 timeout 结果，不启动明知无法完成的新 I/O。

## 5. Policy 结果

Policy 返回完整 `ResolutionPlan`，至少包含：

- client bucket 与生效 identity；
- strategy ID；
- matched rule/hosts/resource ID；
- local answer 或 upstream target；
- effective ECS；
- cache decision 和 namespace；
- TTL override；
- 安全的 decision trace IDs。

Core 不再次计算继承，也不把 rule 文本写入日志。

当前 `HostsCore` 同时保留旧 `HostsTable` 兼容路径，并支持不可变 `Resource::HostsIndex`；命中后直接生成 A/AAAA/CNAME 本地响应，支持 exact/wildcard 优先级，未命中时按 NXDOMAIN/NODATA 语义返回。`PolicyDnsCore` 对 upstream 请求执行 policy → cache lookup/single-flight → upstream → admission/CAS；stale 命中先返回并通过有界 finalizer refresh，`hosts[]` 本地命中绕过 response cache。`DnsCore::resolve_with_observation` 返回当前 strategy、target upstream/group、actual direct/顶层 member、matched rule/resource、answer source 和 cache status；fresh/stale/single-flight cache hit 从 entry provenance 恢复缓存生产 target/actual，不使用当前 plan 猜测来源。

## 6. Cache 交互

只有需要上游交换的计划进入 CacheFacade：

- lookup key 由 Cache 模块根据 ResolutionPlan 和 query 构造；
- fresh hit 直接使用 canonical response；
- stale hit 可以先返回，并启动捕获最新 snapshot 的 refresh；
- miss 进入 single-flight；
- waiter 取消只释放自身，不取消仍有其他 waiter 的 exchange；
- Core 把上游结果交给 CacheFacade 做准入和质量 CAS；内存 CAS 成功后仅向有界 persistence queue 非阻塞入队，SQLite I/O 不阻塞请求路径。

Core 不直接调用 Moka 或持久化 store。

## 7. ECS

ECS 处理顺序：

1. Policy 计算 effective mode；
2. `disabled` 删除已有 ECS；
3. `client` 优先使用合法请求 ECS，否则从 client address 计算；
4. `custom` 使用已验证 CIDR；
5. 对前缀长度进行 family 合法性检查；
6. canonical query 中只保留最终 ECS；
7. 普通路径的 cache key 包含最终 ECS；group member ECS 因成员在 lookup 后选择而暂时绕过缓存。

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

TTL override 在 cache admission/CAS 后应用，因此不会延长缓存 entry 的实际过期时间。当前已实现全部 RR 的配置 min/max 覆写；fresh cache response 按 `inserted_at` 后经过的整秒递减全部 RR TTL，stale response 先检查所选缓存池的 `optimistic.max_age` 并统一使用其 `answer_ttl`，再应用当前请求的 TTL override。

## 10. 事件

一条请求只生成一个最终 resolve event：

- request/trace ID 的脱敏形式；
- listener/route、配置 client bucket、有效 client IP 和 strategy；
- source：hosts、cache 或 upstream；
- target upstream/group 与实际产生结果的 direct/member；cache hit 为缓存生产来源；
- canonical qname/qtype/qclass 与最终逻辑 response 的有界 answer；
- RCODE、cache status、latency buckets；
- cancellation/failure 分类；
- runtime/resource revision 摘要。

Policy Core 提供 `strategy`、策略目标 `upstream_id`、无歧义的 `upstream_used_id`、`source`（hosts/cache/upstream）、`cache_status`、配置 client bucket，以及 matched rule/resource/version。service 在同一次完成回调中补充 canonical question、最终逻辑 response answer 和 `RequestContext.client.client_addr`。这些详情只进入启用的 bounded resolve log；rule matcher、request digest、DNS wire、header 和请求级值不会进入普通日志或 metrics。

parallel 的多个 attempt 另发 attempt event，但不重复增加 total request。

## 11. 错误语义

- 用户输入 DNS 错误：尽可能返回 FORMERR/NOTIMP/BADVERS；
- 策略未找到目标：prepare 应已阻止；运行时出现则为 internal + SERVFAIL；
- 全部 transport failure：SERVFAIL；
- deadline/cancel：按 correlation 状态返回或静默结束；
- telemetry/detail 写入失败：不改变 DNS 结果；
- cache/store 失败：按 Cache 降级语义继续上游或返回已得到响应。

## 12. 测试

- opcode、question count、EDNS version 和 canonical normalization；
- 同一 query 经 UDP/TCP/DoH 得到同一 Core 决策；
- client/strategy/rule/hosts/upstream 管线顺序；
- ECS 各层覆盖与 cache key；
- local hosts 绕过 cache 且资源更新立即生效；
- fresh/stale/miss/single-flight/cancellation；
- NXDOMAIN/NODATA/SERVFAIL/REFUSED/TC 分类；
- TTL override 不延长 cache expiry；
- 全部 upstream failure 和内部不变量破坏；
- 一请求只记录一次 total stats。

## 13. 实现检查清单

- [x] 定义 canonical query/response 与验证器；
- [x] 定义 RequestContext、deadline 与 cancellation；
- [x] 通过 Core 终态、低基数 observation 与 `ResolveEvent` 组合定义完整 resolution result，包含 failure/timeout/cancelled/dropped 分类、取消原因和命中资源 revision；
- [x] 提供可选 observation，并传播 strategy/source/cache status/client bucket；策略 target、actual upstream、cache producer provenance、matched rule/resource 及命中资源版本已拆分；
- [x] 实现固定响应的 transport 无关 handler；
- [x] 接入 Policy、Cache、Upstream ports；（基础 upstream/cache 请求路径已完成）
- [x] 实现配置驱动的 client-visible TTL min/max 覆写，且不改变 cache admission 使用的 origin TTL；
- [x] fresh cache response 应用剩余 TTL，stale response 应用当前缓存池的 optimistic answer TTL；
- [x] 实现 rule/strategy/client/global ECS 正常路径、canonical query 替换及最终 ECS cache key；
- [x] 完成显式 direct upstream/group member ECS；非法客户端 ECS 会被丢弃并按可用 client address 安全回退；
- [x] 完成 UDP/TCP/plain DoH 的 Positive/NODATA/NXDOMAIN、DNS ID、canonical response 和 UDP TC 首轮真实 loopback contract；
- [x] 完成 SERVFAIL/REFUSED 错误响应的跨 transport contract tests。

阶段证据：`dns::message::tests` 当前 14 项通过，覆盖 TTL 上下界、cache age/stale TTL、ECS 替换/删除及其他 EDNS 内容保留；`dns::policy::tests` 当前 36 项通过，覆盖 global pool 关闭时的 strategy/client cache、cache hit 剩余/stale TTL、global/client/direct upstream/group member ECS 实际 DoH wire、非法客户端 ECS 安全回退、成员 ECS group 缓存绕过、client ECS cache key 隔离、direct/cache/group 的目标与成员 observation、hosts/rule-set 的 matched rule/resource 摘要，以及跨 Policy Core 实例的 SQLite cache 恢复；Service 2 项 loopback 定向测试验证真实 UDP、DNS-over-TCP framing 和 plain DoH GET/POST 对 Positive/NODATA/NXDOMAIN/SERVFAIL/REFUSED 返回相同 canonical response 并恢复各自 DNS ID，大响应下 UDP 设置 TC 而 TCP/DoH GET/POST 保留 64 条完整 answer；既有测试继续覆盖 canonical 校验、Policy/Cache/Upstream 主链、资源 live swap、TTL override 和低基数 observation。最近一次大阶段全量测试为阶段 190 的 `555 passed、0 failed`。

阶段 142 将同一 observation 的策略目标、实际组成员和 matched rule/resource 摘要接入 `ResolveEvent` 与 SQLite schema v2；阶段 148 继续从当前 Policy snapshot 传播命中资源的 epoch/revision。Policy 35 项及存储层聚焦测试通过，未重复大阶段全量测试。

阶段 175 增加同一次解析结果的端到端契约测试，验证 SERVFAIL、deadline cancellation 分别稳定归类为 failure/timeout，并与 matched resource revision 一同进入 `ResolveEvent`；不为已有组合契约新增平行 `ResolutionResult` 抽象。

阶段 196 验证非法客户端 ECS 不会进入上游或 cache key：存在客户端地址时回退到脱敏网段，不存在时移除 ECS。阶段 197 在共享 wire 边界生成 header-only FORMERR/NOTIMP，UDP 回应后继续收包，TCP 回应后关闭当前 session；wire/UDP/TCP 定向测试分别 `7/5/12 passed`。

当前实现进度：**86%**。
