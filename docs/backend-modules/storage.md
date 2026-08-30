# Storage 模块设计

> 状态：v1 方案已完成，代码未实现
>
> 更新日期：2026-08-30
>
> 目标代码：`backend/src/storage/*`、`backend/migrations/*`
>
> 上位设计：[后端架构](../backend-architecture.md) · [配置字段参考](../configuration-reference.md)
>
> 相关方案：[Ports](ports.md) · [Observability](observability.md) · [Cache](cache.md)

## 1. 职责与边界

Storage 模块实现业务 SQLite：

- schema migration；
- 默认开启的聚合统计；
- 可选解析详情；
- writer 健康状态、flush 和 shutdown。

它不存储 DNS response cache。Cache persistence 使用配置中的独立文件和独立 `PersistentCacheStore`，不能复用本模块 pool、表或 writer。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `sqlite.rs` | pool、PRAGMA、migration、transaction、health |
| `statistics.rs` | sharded counters、checkpoint、batch ledger |
| `resolve_log.rs` | 有界详情队列、批量写入和淘汰 |

## 2. SQLite 初始化

prepare 阶段：

1. 创建数据库父目录；
2. 以读写/创建模式打开文件；
3. 设置 `foreign_keys=ON`、WAL 和有界 busy timeout；
4. 使用 `synchronous=NORMAL` 作为吞吐与崩溃恢复折中；
5. 运行嵌入 binary 的 SQLx migrations；
6. 执行小型写入/回滚探针；
7. 建立 stats/detail 独立 writer channel；
8. 返回 `StorageServices`。

数据库打不开、migration 失败、无写权限或基本写探针失败属于启动 fatal。

## 3. Schema 职责

具体 SQL 放入 migration，逻辑表至少包括：

### `storage_meta`

- schema version；
- instance/database ID；
- 创建与最近 migration 时间。

### `stats_daily_total`

- `day_utc`；
- `total_requests`；
- primary key 为 `day_utc`。

### `stats_daily_dimension`

- `day_utc`；
- `dimension_kind`；
- `dimension_value`；
- `count`；
- 复合 primary key。

`dimension_kind` 只允许 client bucket、transport class、strategy、source/upstream、RCODE、cache status 和有限 attempt outcome。`dimension_value` 必须来自配置/枚举 ID，不接受任意域名、完整 client ID 或原始 IP。

### `stats_batch_ledger`

- monotonic `batch_id`；
- `max_event_seq`；
- counter epoch；
- commit time；
- payload/hash 摘要。

用于幂等重试，不能与详情日志共享。

### `resolve_log`

- event time 与 request duration；
- request ID digest；
- listener、route、client bucket、strategy；
- canonical qname、qtype、qclass；
- matched resource/rule ID；
- ECS mode/prefix length，不保存完整敏感 client ECS；
- source/upstream、RCODE、cache status；
- failure/cancellation 分类；
- runtime/resource revision 摘要。

解析详情本身是敏感数据；数据库文件使用工作目录权限保护，不把详情复制到服务日志。

## 4. 聚合统计热路径

请求线程只更新内存 sharded counters：

- 单请求分配 monotonic event sequence；
- `day_utc` 在事件发生时确定；
- 一次请求只增加一次 total；
- 维度 key 由 typed enum/ID 构造；
- parallel attempt 只进入 attempt counter，不重复 total。

更新不得 await、锁 SQLite 或向有界详情队列发送。

## 5. Stats checkpoint

writer 周期性执行：

1. 原子切换 counter epoch；
2. 冻结旧 epoch snapshot；
3. 分配 monotonic batch ID；
4. 在单事务中 upsert daily total/dimensions；
5. 写入 batch ledger；
6. commit 成功后 ack 并释放 snapshot；
7. commit 失败保留同一 batch 重试。

重试前先查 ledger；已提交 batch 不重复累加。新请求始终写下一 epoch，不等待旧批次。

进程在计数尚未进入 checkpoint 前崩溃会产生 in-memory persistence gap；系统必须报告，不承诺绝对无损。

## 6. Resolve log writer

详情开启时，DNS Core 通过 `ResolveEventSink` 尝试写入独立有界 channel：

- send 使用 non-blocking try-send；
- 队列满时丢弃当前详情并增加 `dropped_detail_records`；
- writer 按数量或时间批量提交；
- 提交失败有界重试，不阻塞 stats writer；
- `enable=false` 时 sink 为 no-op，但 stats 不受影响。

详情事件在进入队列前完成 redaction 和字段长度限制。

## 7. 淘汰和硬上限

每次详情 batch commit 在同一维护循环检查：

1. 删除早于 `max_record_age` 的记录；
2. 数量达到 `eviction_threshold_records` 时按时间/id 删除最旧记录；
3. 目标降到软阈值以下；
4. 插入前计算本事务后的数量；
5. 如果仍会超过 `max_records`，丢弃本批次中最晚到达的详情并计数。

硬上限判断和插入在同一 writer 串行路径中完成，避免并发突破。聚合表和 ledger 不受详情上限影响。

## 8. Connection 与事务

- stats 和 detail 使用同一业务数据库，但独立逻辑 writer；
- pool 保持小规模，避免 SQLite 写锁竞争；
- 两个 writer 的事务短且不互相 await；
- 只读查询未来通过独立 read connection；
- 所有 SQL 使用 bind 参数；
- migration 只在 prepare 执行。

## 9. 运行期故障

SQLite busy、磁盘满、I/O error：

- stats：保留未 ack batch，继续内存计数，进入 degraded；
- detail：允许丢弃并计数；
- DNS 请求继续服务；
- 恢复后 stats 按 batch ledger 幂等补写；
- 记录 degraded 首发、最近重试、积压 batch、persistence gap 风险；
- pending batch/补偿计数达到 v1 固定内存保护上限时，stats 不能静默丢弃；升级为明确 fatal 或受控进程退出，由 supervisor 处理。

`StorageBackend` 自身 panic、schema corruption 或无法保证 ledger 正确性时升级 fatal，不继续写可能重复的统计。

## 10. Flush 与 shutdown

shutdown：

1. 停止接收新 detail event；
2. 冻结最后一个 stats epoch；
3. 在 deadline 内提交 pending stats batch；
4. 尝试提交 detail batch；
5. WAL checkpoint；
6. 关闭 pool；
7. 返回已提交、丢弃和可能 gap 的摘要。

stats 优先级高于 detail。deadline 不足时先保证 ledger 一致性。

## 11. Migration

- migration 文件只前进，不在运行时自动 down；
- 每个 migration 在空库和上一版本库测试；
- 破坏性表重建使用新表 → copy/validate → rename；
- schema version 与配置/cache/resource version 独立；
- migration 失败保留原库并阻止启动；
- backup/rollback CLI 属于后续独立契约。

## 12. 测试

- 新库、旧版本库、重复启动 migration；
- stats total/dimension upsert；
- batch commit 后崩溃与 retry 去重；
- event 跨午夜、late write；
- parallel/hosts/cache source 计数；
- detail enable/disable、队列满、软阈值、硬上限和 age；
- stats/detail 并发互不阻塞；
- busy、disk full、permission、corruption；
- shutdown deadline 和 gap summary；
- 业务 DB 与 cache DB 完全隔离。

## 13. 实现检查清单

- [ ] 建立 SQLx pool/migration；
- [ ] 实现 stats schema/counters/checkpoint/ledger；
- [ ] 实现独立 resolve-log writer；
- [ ] 实现淘汰与硬上限；
- [ ] 实现 degraded/recovery/flush；
- [ ] 完成 migration、压力和故障测试。

当前实现进度：**0%**。
