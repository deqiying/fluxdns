# DNS Core 模块设计

> 状态：v1 方案已完成，已实现 canonical message、固定 SERVFAIL、内联 hosts、Resource hosts index、Policy upstream path、基础 Cache fresh/miss/single-flight/CAS 接线、当前 snapshot-local optimistic refresh、SQLite cache non-blocking write/recovery 和低基数 resolution observation；Policy observation 已补充 client bucket，并在 group 路径记录实际选中的成员；配置选中的 TTL override 已在缓存写入后统一应用到 hosts、upstream 和 cache response 的全部 RR，fresh/stale cache response 已应用剩余 TTL 与 optimistic answer TTL；Runtime 已持有资源摘要并由 service 捕获同 revision core，Policy compiled resource live swap 和配置候选 reload 已接入
>
> 更新日期：2026-09-03
>
> 目标代码：`backend/src/dns/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [开发计划](../backend-development-plan.md)
>
> 相关方案：[Ports](ports.md) · [Policy](policy.md) · [Cache](cache.md) · [Upstream](upstream.md)

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

非法 query 在能够安全形成 DNS 响应时返回 FORMERR/NOTIMP/BADVERS；无法获得可靠 ID/question 时由 Transport 丢弃或返回协议层错误。

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

当前 `HostsCore` 同时保留旧 `HostsTable` 兼容路径，并支持不可变 `Resource::HostsIndex`；命中后直接生成 A/AAAA/CNAME 本地响应，支持 exact/wildcard 优先级，未命中时按 NXDOMAIN/NODATA 语义返回。`PolicyDnsCore` 现在对 upstream 请求执行 policy → cache lookup/single-flight → upstream → admission/CAS 的基础路径，stale 命中时可在当前 immutable core 内先返回并通过有界 finalizer 进行 optimistic refresh，`hosts[]` 本地命中仍绕过 response cache；上游请求会按 plan 应用 `disabled`/`client`/`custom` ECS，保留其他 EDNS option，并以最终 ECS 隔离 cache key；响应离开 Policy Core 前按当前 plan 对 answer/authority/additional RR 应用 TTL 上下界，缓存仍保存未覆写的 canonical response；`DnsCore::resolve_with_observation` 沿同一请求路径返回生效 strategy、selected upstream、answer source 和 cache status，供 service 的 stats/detail 使用。

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
7. cache key 包含最终 ECS。

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
- listener/route/client bucket/strategy；
- source：hosts、cache 或 upstream；
- final upstream/group ID；
- RCODE、cache status、latency buckets；
- cancellation/failure 分类；
- runtime/resource revision 摘要。

Policy Core 已提供首轮 `strategy`、selected `upstream`、`source`（hosts/cache/upstream）、`cache_status` 和配置 client bucket 元数据；group member、matched resource/rule 等完整 resolution result 字段仍待后续切片。client bucket/upstream 仅使用已验证配置 ID，未知匹配保持缺省，不记录原始 client ID/IP。

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
- [ ] 定义完整 resolution result；
- [x] 提供可选低基数 observation，并传播 strategy/source/cache status/client bucket/selected upstream；group 路径已记录实际成员；
- [x] 实现固定响应的 transport 无关 handler；
- [x] 接入 Policy、Cache、Upstream ports；（基础 upstream/cache 请求路径已完成）
- [x] 实现配置驱动的 client-visible TTL min/max 覆写，且不改变 cache admission 使用的 origin TTL；
- [x] fresh cache response 应用剩余 TTL，stale response 应用当前缓存池的 optimistic answer TTL；
- [x] 实现 rule/strategy/client/global ECS 正常路径、canonical query 替换及最终 ECS cache key；
- [ ] 完成显式 upstream/group member ECS 和完整错误映射；
- [x] 完成 UDP/TCP/plain DoH 同策略 answer 的首轮真实 loopback contract；
- [ ] 完成跨 transport contract tests。

阶段证据：`dns::message::tests` 当前 14 项通过，覆盖 TTL 上下界、cache age/stale TTL、ECS 替换/删除及其他 EDNS 内容保留；`dns::policy::tests` 当前 32 项通过，覆盖 global pool 关闭时的 strategy/client cache、cache hit 剩余/stale TTL、global/client/direct upstream ECS 实际 DoH wire、client ECS cache key 隔离、普通及嵌套 group 实际成员 observation，以及跨 Policy Core 实例的 SQLite cache 恢复；Service loopback 测试验证同一 hosts 策略经真实 UDP、DNS-over-TCP framing 和 plain DoH POST 返回一致 answer；既有测试继续覆盖 canonical 校验、Policy/Cache/Upstream 主链、资源 live swap、TTL override 和低基数 observation。最近一次大阶段全量测试为 515 passed、0 failed。

当前实现进度：**75%**。
