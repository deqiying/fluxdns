# Cache 模块设计

> 状态：v1 方案已完成，已实现内存 CacheStore 首轮切片；Facade、TTL、Moka/SQLite persistence 尚未实现
>
> 更新日期：2026-08-31
>
> 目标代码：`backend/src/cache/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [配置字段参考](../configuration-reference.md)
>
> 相关方案：[Ports](ports.md) · [DNS Core](dns-core.md) · [Storage](storage.md)

## 1. 职责

Cache 模块实现逻辑缓存池、entry 生命周期、single-flight、optimistic refresh、内存存储和独立持久化存储。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `key.rs` | namespace、query/strategy/ECS/transport compatibility key |
| `memory.rs` | 当前为无外部依赖的 HashMap/Mutex `CacheStore` adapter；Moka 接入待后续切片 |
| `persistence.rs` | 独立 SQLite `PersistentCacheStore` |
| `service.rs` | `CacheFacade`、single-flight、TTL、CAS、invalidations |

缓存 SQLite 与业务统计 SQLite 是不同文件、不同 schema、不同 writer 和不同故障边界。

## 2. Namespace

每个请求最多选择一个：

- `global`；
- `strategy:<strategy-id>`；
- `client:<identity-digest>:strategy:<strategy-id>`；
- disabled。

namespace 使用稳定 typed components 编码，不直接拼接可伪造原始 client ID。持久化格式包含 namespace format version。

## 3. Cache key

key 至少包含：

- namespace；
- canonical QNAME/QTYPE/QCLASS；
- 会改变答案的 DNS flags；
- effective ECS；
- strategy/route 产生的 target identity；
- opaque transport compatibility；
- cache key format version。

不包含：

- 客户端 DNS ID；
- RuntimeSnapshot revision；
- 全局 resource generation；
- HTTP header 或 URL；
- 原始 client address。

资源变更不会因 key generation 改变而清空全局缓存。

## 4. Entry

entry 包含：

- canonical response；
- inserted/expiry/stale-until 时间；
- 原始 RR TTL metadata；
- response class；
- producer runtime revision；
- 相关 resource fingerprints；
- quality rank；
- checksum 和 format version；
- 上游 TC 时的 transport compatibility。

resource fingerprint 用于诊断和显式清理，不是普通 lookup 的自动失效条件。

## 5. TTL 与准入

- 正常 NOERROR：按可用 RR TTL 计算 entry 生命周期，返回时逐 RR 扣减；
- NODATA/NXDOMAIN：优先使用 SOA TTL 与 MINIMUM 的较小值，无可用值时使用 `failure_ttl`；
- SERVFAIL 和上游 TC：使用 `failure_ttl`；
- REFUSED：不缓存；
- malformed、question mismatch、连接/TLS/HTTP failure、timeout：不缓存。

TTL override 只影响 client-visible TTL，不延长 entry expiry。

quality 从高到低：

1. 完整 NOERROR/TC=0；
2. NODATA/NXDOMAIN；
3. SERVFAIL/TC。

高质量可替换低质量；低质量不能覆盖仍 fresh 的高质量。同质量默认保留先到值到 expiry，避免竞态抖动。

## 6. Lookup 状态

`CacheFacade` 返回：

- `Disabled`；
- `Miss`；
- `Fresh(entry)`；
- `Stale(entry, refresh-permit)`；
- `StoreUnavailable`。

store unavailable 时继续上游解析；已有可安全读取的内存 entry 仍可返回。

## 7. Single-flight

single-flight key 与 cache key 相同：

- 首个 miss 创建 producer；
- 后续 waiter 订阅 producer 结果；
- 单 waiter 取消只移除自身；
- 无 waiter 且无 late cache value 时可以取消 producer；
- optimistic refresh 即使没有当前 waiter，也在 refresh deadline 内保留 producer；
- producer 完成后以 CAS 写入并广播；
- panic/取消必须释放占位，不能永久阻塞 key。

为防止高基数攻击，占位表使用容量和空闲超时上限；超过上限时允许请求独立解析并计数，而不是全局阻塞。

## 8. Optimistic refresh

stale entry 仅在：

- optimistic enabled；
- 当前时间不超过 stale-until；
- entry 仍满足 transport compatibility；
- response class 允许 stale；
- refresh admission 有容量

时先返回。

refresh 捕获启动时最新 RuntimeSnapshot，完整重跑 client/policy/resource/upstream 流程。它不能复用旧 entry 保存的 connector 或 rule pointer。

写回以 key + quality + producer revision 做 CAS；旧 producer 不覆盖新完整答案。

## 9. Memory store

目标 Moka store：

- 全部 namespace 共享一个容量预算；
- weigher 计入 key、canonical wire、TTL metadata 和索引开销；
- 计费值用于 eviction，不宣称等于 RSS；
- per-entry expiry；
- eviction listener 只发送低成本统计，不执行阻塞 I/O。

Moka adapter 不向 DNS Core 暴露具体 entry guard 或 future 类型。

当前已实现的首轮内存 adapter 使用 `HashMap + Mutex` 保持无外部依赖和确定性：

- lookup 区分 fresh、stale 和已过期 entry；已过期且超出 stale 窗口的 entry 会被移除；
- CAS 检查 `Absent/Version` 条件，并拒绝较低质量覆盖仍 fresh 的高质量 entry；
- 支持 exact、namespace、typed predicate 和 all 失效；
- single-flight 保证每个 key 只有一个 leader，follower 可独立取消，leader abandon/drop 会广播稳定失败；
- shutdown 会清理记录、唤醒 waiter，并拒绝后续读写；
- `weighted_size` 仅作为内部计费统计，不宣称等于进程 RSS；当前尚未实现容量淘汰和 eviction listener。

## 10. Persistence

独立 SQLite cache DB 至少包含：

- metadata：schema/cache format/key format；
- cache entry；
- namespace/key 索引；
- checksum、expiry 和 last-access bucket。

写入经有界队列批量提交。超过 page budget 时优先删除 expired，再按近似 LRU/last-access bucket 淘汰。WAL/SHM 短时空间不计入主文件预算，但必须可观测。

启动恢复：

1. 检查 schema/format；
2. 验证 checksum；
3. 丢弃 expired/corrupt/incompatible entry；
4. 分批注入 memory store；
5. 任一步失败只禁用 persistence 并标记 degraded，不阻止 DNS 启动。

## 11. 显式失效

提供：

- exact key；
- namespace；
- typed predicate；
- 全部缓存。

普通 resource refresh 不调用这些接口。未来 WebUI 清除缓存必须通过同一 facade 并产生审计事件。

## 12. 故障语义

- memory store 内部错误：绕过缓存继续解析；
- persistence queue 满/DB busy/disk full：停止或丢弃持久化写，保留内存；
- recovery corrupt entry：隔离并计数，不加载；
- single-flight producer panic：唤醒 waiter 为 failure，清理占位；
- stale refresh failure：保留原 entry 到 stale-until，不延长窗口。

## 13. 测试

- namespace 选择和 key 稳定性；
- 正向/负向/failure TTL；
- REFUSED 和 transport failure 不准入；
- quality CAS 和并发乱序；
- single-flight 多 waiter 与取消；
- optimistic 使用最新 snapshot；
- resource update 不全局失效；
- Moka weight/expiry；
- persistence format/checksum/recovery/page budget；
- DB busy/disk full 降级不影响 DNS；
- explicit invalidation 范围准确。

## 14. 实现检查清单

- [ ] 定义 namespace/key/entry format；（基础 typed contract 已在 `ports/cache.rs`，生产 key builder 待实现）
- [ ] 实现 CacheFacade 和准入/TTL；
- [x] 实现 single-flight/CAS/显式失效的内存 adapter 首轮切片；
- [ ] 实现 Moka adapter；
- [ ] 实现独立 SQLite persistence；
- [x] 完成内存 adapter 的 fresh/stale/expiry、质量 CAS、失效、取消、abandon 和 shutdown 测试；
- [ ] 完成跨 adapter 一致性、恢复和故障测试。

当前实现进度：**20%**（内存 adapter 首轮切片；容量淘汰、Facade/TTL、optimistic refresh 和 SQLite persistence 未实现）。
