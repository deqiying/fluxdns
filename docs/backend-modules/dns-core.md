# DNS Core 模块设计

> 状态：v1 方案已完成，代码未实现
>
> 更新日期：2026-08-30
>
> 目标代码：`src/dns/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [开发计划](../backend-development-plan.md)
>
> 相关方案：[Ports](ports.md) · [Policy](policy.md) · [Cache](cache.md) · [Upstream](upstream.md)

## 1. 目标

DNS Core 是 transport 无关的请求编排器。输入是 canonical query 和 request context，输出是 canonical response 或明确的“无需再响应”结果。

Core 不读取配置文件，不持有 socket/HTTP client/SQLite/Moka，也不根据具体 UDP/TCP/DoH 类型分支。transport 差异通过 capability 和 response encoder 处理。

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

本地 hosts 已经是内存编译 snapshot，命中后直接生成响应并绕过 response cache；这样资源更新在下一请求立即可见，也避免重复缓存静态索引。仍应用客户端可见 TTL override，并记录 `source=hosts`。

## 6. Cache 交互

只有需要上游交换的计划进入 CacheFacade：

- lookup key 由 Cache 模块根据 ResolutionPlan 和 query 构造；
- fresh hit 直接使用 canonical response；
- stale hit 可以先返回，并启动捕获最新 snapshot 的 refresh；
- miss 进入 single-flight；
- waiter 取消只释放自身，不取消仍有其他 waiter 的 exchange；
- Core 把上游结果交给 CacheFacade 做准入和质量 CAS。

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

TTL override 不延长缓存 entry 的实际过期时间。返回 stale/optimistic entry 时使用 `answer_ttl`，且不超过该 entry 允许的 stale 窗口。

## 10. 事件

一条请求只生成一个最终 resolve event：

- request/trace ID 的脱敏形式；
- listener/route/client bucket/strategy；
- source：hosts、cache 或 upstream；
- final upstream/group ID；
- RCODE、cache status、latency buckets；
- cancellation/failure 分类；
- runtime/resource revision 摘要。

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

- [ ] 定义 canonical query/response 与验证器；
- [ ] 定义 RequestContext 和 resolution result；
- [ ] 实现 transport 无关 handler；
- [ ] 接入 Policy、Cache、Upstream ports；
- [ ] 实现 ECS、TTL 和错误映射；
- [ ] 完成跨 transport contract tests。

当前实现进度：**0%**。
