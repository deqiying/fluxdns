# DNS Core 模块设计

> 文档状态：有效
>
> 适用范围：canonical DNS message、请求管线、缓存交互和上游结果处理
>
> 最后评审：待核对（本次仅分类与边界复核，不等同完整契约重审）
>
> 关联实现：`backend/src/dns/*`
>
> 关联文档：[后端架构](../overview.md) · [Ports](ports.md) · [Policy](policy.md) · [Cache](cache.md) · [Upstream](upstream.md)

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
  → prepare PolicyContext（client / strategy / namespace / fingerprints）
  → fast cache key v2 lookup
  → on miss: evaluate RouteDecision（rules / hosts / target / member ECS）
  → local hosts answer OR resolved key lookup / exchange
  → validate and classify response
  → create optional CacheCommitCandidate
  → apply client-visible TTL
  → try_publish one ResolutionEnvelope
  → return canonical response
  → background: stats / cache commit / optional detail projection
```

每个 stage 检查 cancellation 和剩余 deadline。deadline 不足时直接产生 timeout 结果，不启动明知无法完成的新 I/O。

## 5. Policy 结果

Policy 将原有完整决策拆为两个稳定阶段：

- `PolicyContext`：client bucket/identity、生效 strategy、cache decision/namespace、TTL override、可提前确定的 ECS，以及 policy/request fingerprint；
- `RouteDecision`：matched rule/hosts/resource、local answer 或 upstream target、最终 target/member ECS 和安全 decision trace IDs。

Core 不再次计算继承，也不把 rule 文本写入日志。

当前 `HostsCore` 同时保留旧 `HostsTable` 兼容路径，并支持不可变 `Resource::HostsIndex`；命中后直接生成 A/AAAA/CNAME 本地响应，支持 exact/wildcard 优先级，未命中时按 NXDOMAIN/NODATA 语义返回。`PolicyDnsCore` 对 eligible 请求先执行 `prepare_context → fast lookup`，miss 后再执行 `evaluate_route → resolved lookup/upstream`；stale 命中先返回并在后台按最新 Policy/资源重新刷新，`hosts[]` 本地命中绕过 response cache。`DnsCore::resolve_with_completion` 返回响应、当前 strategy/target/actual/matched resource/cache lookup observation、取消原因和可选 commit candidate；fresh/stale/single-flight cache hit 从 entry provenance 恢复缓存生产 target/actual，不使用当前 route 猜测来源。

## 6. Cache 交互

只有需要上游交换的路径进入 CacheFacade：

- lookup key 由 Cache 模块根据 `PolicyContext`、可选 `RouteDecision` 和 query 构造；`Fast`/`Resolved` 模式显式隔离；
- fresh hit 直接使用 canonical response；
- stale hit 可以先返回，并启动捕获最新 snapshot 的 refresh；
- miss 进入 single-flight；
- waiter 取消只释放自身，不取消仍有其他 waiter 的 exchange；
- Core 把上游结果与 single-flight lease 封装为 `CacheCommitCandidate`，由统一解析事件移交后台 worker；worker 用独立 100ms deadline 完成准入、质量 CAS、follower completion 和 persistence enqueue。

Core 不直接调用 Moka 或持久化 store。

## 7. ECS

ECS 处理顺序：

1. Policy 计算 effective mode；
2. `disabled` 删除已有 ECS；
3. `client` 优先使用合法请求 ECS，否则从 client address 计算；
4. `custom` 使用已验证 CIDR；
5. 对前缀长度进行 family 合法性检查；
6. canonical query 中只保留最终 ECS；
7. fast key 包含提前可确定的 ECS/request fingerprint；group member ECS 因成员在 lookup 后选择而不使用 fast key，并继续绕过 response cache。

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

- request/trace ID 的脱敏形式；
- listener/route、配置 client bucket、有效 client IP 和 strategy；
- source：hosts、cache 或 upstream；
- target upstream/group 与实际产生结果的 direct/member；cache hit 为缓存生产来源；
- typed canonical question，以及仅在 `resolve_log` 开启时附带的共享 `Arc<CanonicalResponse>`；
- RCODE、cache status、latency buckets，以及在 core 返回时冻结的服务端总耗时和 DNS 主链耗时；
- cancellation/failure 分类；
- runtime/resource revision 摘要。

Policy Core 提供 `strategy`、策略目标 `upstream_id`、无歧义的 `upstream_used_id`、`source`（hosts/cache/upstream）、lookup `cache_status`、配置 client bucket，以及 matched rule/resource/version。service 补充 typed canonical question、共享逻辑 response 和 `RequestContext.client.client_addr`，但不在请求任务中生成 request digest、qname/answer 字符串或 JSON。后台 dispatcher 更新 stats、提交 cache candidate，并仅在启用时把事件交给 detail projector；rule matcher、request digest、DNS wire、header 和请求级值不会进入普通日志或 metrics。

service 紧贴 `DnsCore::resolve_with_completion` 调用前后记录主链耗时，并以 transport 提供的 `received_at` 计算截至同一完成时刻的服务端总耗时。两个值作为数值跨越异步 observation 边界，不传递 `Instant`，因此 dispatcher 排队、detail projector 和 SQLite 写入不会污染请求耗时。

resolution ingress 满时 DNS 响应照常编码，但整个 envelope 被丢弃并累计 `dropped` 与首次 gap 时间；cache candidate drop 通过 RAII 释放 single-flight follower。detail 与 cache 下游队列失败只影响各自消费者。cache lookup 状态只表示响应完成前已知的 hit/miss/stale，异步 commit 结果以独立 counter 记录。

parallel 的多个 attempt 另发 attempt event，但不重复增加 total request。

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
- ECS 各层覆盖与 cache key；
- fast/resolved key v2 隔离、fingerprint 语义变化与纯观测配置排除；
- local hosts 绕过 cache 且资源更新立即生效；
- fresh/stale/miss/single-flight/cancellation；
- 异步 commit 成功唤醒 follower，candidate drop 释放 lease；
- NXDOMAIN/NODATA/SERVFAIL/REFUSED/TC 分类；
- TTL override 不延长 cache expiry；
- 全部 upstream failure 和内部不变量破坏；
- 一请求只记录一次 total stats。
- 一请求只发布一次 typed completion，响应对象与详情 source 共享同一 `Arc`；
- resolution ingress/detail/cache commit 有界队列的丢弃与独立计数。
