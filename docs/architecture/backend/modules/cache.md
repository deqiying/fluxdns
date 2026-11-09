# Cache 模块设计

> 文档状态：有效
>
> 适用范围：缓存 key、TTL、single-flight、memory adapter 和 snapshot 生命周期
>
> 最后评审：2026-09-08（内存权威与独立完整快照；其余基线见[模块索引](README.md)，实际接线见[后台服务](../../../implementation/backend/background-services.md#cache-persistence)）
>
> 2026-09-20 局部评审：仅核对 group ECS、Resolved fingerprint、stale refresh 与 late-result 约束；不代表整篇重审或运行验收
>
> 局部评审基线：`fcbb12831c66e81e02108ccd030ac3c2a1e08f56` 加本次工作树；仅用于上述范围
>
> 2026-09-21 局部评审：仅核对条目保留期、`Expired` lookup 状态、过期条目的写回 CAS 与快照恢复口径；不代表整篇重审或运行验收
>
> 2026-10-04 局部评审：仅核对负缓存 TTL 上限、按条目质量区分的 stale 窗口与刷新结果观测；不代表整篇重审，本地运行验收见[负缓存新鲜度计划](../../../plans/negative-cache-freshness.md)
>
> 2026-11-08 局部评审：REFUSED/空 Answer 固定 10s、禁止 stale、客户端 TTL 同步及 entry v3 恢复边界；本次不改变上游响应选择与 fallback。
>
> 2026-11-09 局部评审：基线 `08a774724e2304f72dd7b8d0a9d0299983f9b0e5` 加工作树，仅核对 v3 AnswerKey、ECS scope、前缀索引及刷新/late-result 契约；未新增测试或运行验收。
>
> 关联实现：[service.rs](../../../../backend/src/cache/service.rs)、[moka.rs](../../../../backend/src/cache/moka.rs)、[snapshot.rs](../../../../backend/src/cache/snapshot.rs)、[snapshot_owner.rs](../../../../backend/src/cache/snapshot_owner.rs)
>
> 关联文档：[后端设计](../overview.md) · [配置参考](../../../implementation/configuration.md) · [DNS 管线](../../../implementation/backend/dns-pipeline.md) · [后台服务](../../../implementation/backend/background-services.md)

## 1. 职责与边界

Cache 管理逻辑池、entry 生命周期、single-flight、optimistic refresh 和独立完整快照。`CacheFacade` 编排语义，Moka 内存集合是缓存唯一权威；快照只负责进程启动预热和周期覆盖，不维护第二份增量集合。缓存文件与业务统计 SQLite、详情日分片使用不同路径、owner 和故障边界。

| 组件 | 设计职责 |
| --- | --- |
| key / admission | 稳定编码、响应分类、TTL、checksum 与质量 |
| memory / Moka adapter | 同一 CacheStore 契约、容量和逐 entry 过期 |
| snapshot codec / file adapter | 完整文件头、流式记录、完整性校验、恢复与原子发布 |
| snapshot owner | 进程级周期覆盖、generation 仲裁、冷启降级和有序关闭 |
| facade / commit candidate / finalizer | lookup、CAS、single-flight lease 和晚到结果生命周期 |

## 2. Namespace 与 key

每个请求最多选择 global、strategy、client-identity+strategy 或 disabled 中的一个逻辑池，不逐层查询 fallback。具体继承只由[配置参考](../../../implementation/configuration.md)定义。

namespace 使用稳定 typed components，不拼接可伪造原始 client ID。client identity 使用域分隔 digest，持久化格式包含版本。

key format v3 的 `AnswerKey` 包含 namespace、transport compatibility、target 子树 fingerprint、规范化 question（qname/qtype/qclass）、RD/CD/DO、排序后的保留 EDNS options，以及最终成员 ECS 维度。EDNS 是否存在本身不入 key；ECS 单独编码，COOKIE、PADDING、TCP keepalive、UDP payload size 不入 key，未知 option 保守保留。DNS ID、入口、客户端、policy/request fingerprint、全局资源 hash 和 runtime revision 不入答案身份；namespace 仍是用户显式隔离边界。

每个请求先完成 `prepare_context → evaluate_route → prepare_query` 再查缓存，Fast 路径已删除且不保留决策 memo。listener/strategy hosts 本地回答绕过 response cache。规则、hosts、策略或入口变化只改变决策；同 namespace 内最终 target 与查询不变时复用答案，不扫描清库。

target fingerprint 只覆盖所选子树：group 的模式、权重、成员/fallback 顺序、超时与嵌套结构；direct 定义、完整 DoH URL（含 path/query）、connect_ip、bootstrap 链、所用 outbound 的已解析内容与上游 hosts 内容。outbound 摘要复用 connector 构造时的 profile，不二次读取 SecretRef；完整 URL 和凭据只输入摘要，不进入 key 明文或日志。

`prepare_query` 遍历 primary、fallback、nested group 的全部 direct leaves。rule/strategy/client 显式 ECS 统一覆盖成员，否则按成员 upstream/global 继承。最终 ECS 一致时使用 uniform 维度；真正异构时将按成员 ID 排序的 `(member, 最终 ECS 或 None)` 编成独立摘要，精确匹配并正常参与 lookup、single-flight、commit，不参与 scope 扩展。

## 3. Entry、TTL 与质量

entry 保存 canonical response、inserted/expiry/stale-until、原始 RR TTL、response class、producer revision、quality、checksum/format，以及缓存生产请求的 target/actual upstream provenance。上游 TC 额外受 transport compatibility 限制；producer revision 只用于诊断/CAS，不作为全局失效开关。

- 正常 NOERROR 按可用 RR TTL 决定生命周期，返回时逐 RR 扣减。
- REFUSED 与所有通过协议校验、Answer 为空的上游响应固定缓存 10s，不依赖 SOA/原 TTL，也不受 failure TTL、negative TTL 上限或 TTL override 改变。客户端首答的 RR TTL 与 SOA.MINIMUM 同步改为 10s，命中时使用剩余寿命；无 RR 时不伪造记录。origin response 与 checksum 保留原文。
- 非空 NXDOMAIN 仍按 SOA/最小 RR TTL/failure TTL 与负 TTL 上限处理；普通非空 NOERROR（包括仅 CNAME 链）保持原 TTL。非空 SERVFAIL/上游 TC 沿用 failure TTL。非空未知类、正答案零/缺失 TTL、malformed、question mismatch、连接/TLS/HTTP failure 或 timeout 不准入。
- TTL override 只改变非短响应的 client-visible TTL，不延长 entry expiry；持有 origin response 的候选不得被输出覆写污染。
- 质量顺序是完整 NOERROR/TC=0、NODATA/NXDOMAIN、SERVFAIL/TC/REFUSED/Other；低质量不得覆盖仍 fresh 的高质量，同质量默认保留先到值到 expiry。

## 4. Lookup 与 single-flight

Facade 区分 Disabled、Miss、Fresh、Stale+一次性 refresh permit、Expired、StoreUnavailable。store unavailable 时继续解析，不能把存储错误伪装成正常命中。Expired 表示条目存在但已过期且当前不可乐观返回：本次必须回源，观测值也必须据此记录为「条目过期」，不能与「从未缓存」合并成同一个状态。

统一 ECS 的 lookup 使用 base（含地址族）下的 covering 前缀计数索引，从长到短探测不超过查询 source 的前缀，最后探测相同 source/网段的 exact key。按顺序选择 fresh 或当前池允许返回的 stale；较宽 fresh 不越过可返回的更具体 stale。不可返回的旧条目继续保留版本供 CAS。single-flight 使用 `AnswerKey::load_key`，按查询 ECS 精确值合并；存储 key 则在应答返回后由 `storage_key` 决定：

| 最终查询 / 应答 ECS | 存储与适用范围 |
| --- | --- |
| 带 ECS，source S > 0，合法一致应答 scope C ≤ S | covering `/C`；只服务该网段内 source ≥ C 的查询 |
| C > S，S 已达 IPv4 /24 或 IPv6 /56 上限 | covering `/S` |
| C > S，S 未达上限 | exact `/S`，只服务 source/网段相同的查询 |
| 应答不带 ECS，或 FAMILY/SOURCE/ADDRESS 与查询不一致 | 忽略应答 ECS，按查询 source 存 covering `/S`；不一致应答仍返回与缓存，这是对 RFC 7871 MUST drop 的有意偏离，不扩大共享范围 |
| 查询 source = 0 | 始终 exact `/0`，区别于应答 scope 0 的 covering `/0`；exact 本身不扩大共享范围 |
| 查询无 ECS | 独立无 ECS 桶，精确匹配，不服务带 ECS 的请求 |

异构成员组合与无 ECS 桶均使用独立编码，不与 uniform scope 条目 alias。single-flight 生命周期如下：

1. 首个 miss 创建 producer，后续 waiter 订阅同一结果；单 waiter 取消不影响其他人。
2. 无 waiter 且无 late cache value 时可取消 producer；optimistic refresh 在独立窗口内可以继续。
3. producer 返回共享 response，将请求、origin response 和不可 clone 的 RAII lease 移交 `CacheCommitCandidate`。
4. 后台 cache worker 使用独立 100ms deadline 执行 admission/CAS，发布 Ready/Miss/Failed；磁盘覆盖由进程级 snapshot owner 独立调度。
5. 队列拒绝、取消、panic、abandon 或 drop 都必须结束 lease 并唤醒 waiter，不能永久占位。

占位表受容量和空闲超时保护；超限允许独立解析并计数，不全局阻塞。异步 Stored/Rejected/Conflict/Unavailable/Dropped 与响应前的 cache lookup status 分别计数。

## 5. Optimistic 与 late result

只有 optimistic 开启、未超过 stale-until、transport compatible、响应类允许且 refresh admission 有容量时才可先返回 stale。REFUSED/空 Answer 永不 stale，10s 到期即回源，即使条目带有旧的长 stale-until 也不能绕过。其余条目按质量选窗口：完整正应答用 `max_age`，非空 NXDOMAIN 用 `negative_max_age`，失败类不允许 stale。共享 store 按启用池的最大窗口写入，lookup 按当前所选池再次收紧。

短期拒绝/空结果不能用长 stale 窗口兜底，以免遮蔽新增记录。后台刷新仍可将旧正答案替换为新的短期结果，但该结果本身到期后必须同步回源。刷新与写入活动保留响应类及实际写入 TTL，用于区分空响应与有效记录。

乐观缓存未启用或乐观窗口已经结束时，lookup 返回 Expired 而不是 Miss；两者都要回源，但必须保留这一区分，否则无法判断某次解析是因为条目过期还是因为从未缓存。过期条目仍占用 key，写回必须按版本 CAS 替换，不能当作不存在写入。

stale 刷新切到最新可用 core，重新执行完整 `prepare_cache_query`，不复用旧 connector/rule pointer；目标已有 fresh 则跳过。刷新回源前记录目标 store 的版本水位，写回前再次核对 `AnswerKey`。新 scope key 按探测到的旧版本 CAS，未见条目使用 `Absent`；写入成功后，在 store 锁内确认新条目版本仍有效，再删除同 base 下被它完整覆盖且版本不晚于水位的旧条目，不删除刷新期间更新的条目。新条目已失效/替换则不清理；准备、回源、匹配、写入或清理失败都不延长旧 stale。

`PolicyLateResultSink` 保存生产请求的 `AnswerKey`，不保存 policy semantics。切换 latest core 后完整重新决策，只有答案身份相等才允许旧响应跨 runtime 写入，写前再次核对；policy/资源变化本身不再触发丢弃，target 或最终查询变化则拒绝。fresh/stale 目标已有同等或更高质量时不覆盖；已过期目标按版本替换。finalizer 有界接收，shutdown 取消并等待已接收任务；晚到结果不改变已返回响应。

调试日志使用 `operation = "cache_scope"`、`reason = "ecs_mismatch"`、`operation = "cache_refresh"`、`operation = "cache_late"` 等低基数字段，不输出明文 ECS 或客户端 IP；旧 `group_ecs_differs` 绕过事件已删除。实际接线与证据边界见[DNS 管线](../../../implementation/backend/dns-pipeline.md)。

## 6. Memory store

生产选用 Moka，替代内存 adapter 必须遵循同一契约：

- 所有 namespace 共享一个 weight 预算；计入 key、wire、索引/元数据，不承诺等于 RSS。
- oversized entry 在 CAS 前明确拒绝，不能绕过全局预算。
- 条目可见截止为应答窗口（`expires_at` 与 `stale_until` 中较晚者）再加固定过期保留期：窗口内可观察合法 stale，窗口结束后仍可短暂观察已过期条目，用于区分回源原因。保留期内的过期条目不计入命中，也不改变任何可返回给客户端的答案。
- size eviction 单独计数，不混入显式失效、替换和 TTL 过期。
- single-flight reservation/wait/publish/abandon 与 record 存储解耦，不向 Core 暴露 Moka guard/future。
- covering 前缀索引按 base/前缀长度计数，每次写入新版本增加，所有移除通知（含替换）减少旧版本计数，避免读写之间淘汰导致漏记；恢复通过正常插入路径重建，不单独持久化。异步通知可留下空探测，但不能漏掉有效条目或返回错误答案。
- subnet 条目共享全局 weight，没有每 base 条目上限；不能将前缀索引描述成独立容量配额。
- shutdown 清理记录、唤醒 waiter，后续操作返回关闭状态。

确定性 HashMap/Mutex adapter 用于替代实现和契约测试，不是生产默认的证据。

## 7. Snapshot

快照使用单个非 SQLite 二进制文件。文件头包含独立 snapshot format version、生成时间、记录数、body 长度和 SHA-256 完整性摘要；每条记录继续复用 cache key/entry format、canonical response、绝对到期时间、fingerprint 及 upstream provenance。快照格式版本不复用配置、详情 layout 或 WS 版本。

一个进程级 owner 从 Moka 当前可见集合分批取得记录，释放查询锁后编码并顺序写入同目录临时文件。单条和总记录数有内部上界；同一轮按完整 key 摘要去重。并发更新、淘汰可能让本轮少量记录未被捕获，这是允许的弱一致性，不得通过保留旧磁盘条目补齐。完成 header、长度和摘要后 flush/sync，再经过 owner generation 仲裁发布；失败保留上一份完整文件。

快照没有用户可配置的磁盘大小配额，也不承诺文件大小等于 Moka weight 或 RSS。读取仍受文件字节数、单条大小、记录数、批次和 deadline 保护；先验证整个文件的长度与摘要，再分批解码。恢复逐条检查 key/entry version、checksum、绝对 expiry/stale-until 和当前内存预算，只装载仍可用于应答的条目（写入侧可见的过期条目计入 `expired`，不补回内存），停机时间不会重新补满 TTL。损坏、未知版本、超时或预算不足只形成冷启/部分恢复状态，不阻止 DNS 服务。

cache key 升为 v3，旧 v2 key 记录恢复时为 `Incompatible`，升级需冷启动一次；外层 `FDCS` 与 entry v3 格式不因此改变。entry codec 仍保留同布局普通非空 v2 payload 转换能力、拒绝旧短响应 payload，但不能绕过 key 版本检查。绝对到期时间仍是恢复依据，不因重启补满 TTL。详细兼容性见[配置参考](../../../implementation/configuration.md)。

内存 commit 不再产生逐条 persistence 队列。周期任务覆盖当时的完整可见集合，被内存预算淘汰或显式清理的记录会从下一份快照消失。owner/path 切换和未来 clear 必须递增 generation，使旧任务失去发布权；正常 shutdown 只在统一剩余预算内尽力补写，不无限延长退出。

BC-07 已将正式 app/runtime/dns/service 切换到一个进程级 owner：初次启动恢复、周期完整覆盖、reload generation/source 切换和 finalizer 后最终写入均已接线。BC-26 已由正式 v2 loader 提供独立 `persistence.enabled/path/snapshot_interval`；旧 SQLite/FDCP adapter 和增量 persistence port 已删除；快照文件与共用条目 codec 独立于内存 finalizer。

## 8. 显式失效

Facade 提供 exact key、namespace、typed predicate 和 all 失效。普通资源刷新不调用这些接口。WebUI 当前没有缓存清除功能；将来增加时必须先评审权限/审计，不直接操作 store。

## 9. 契约验证要求

- namespace 隔离、v3 question/RD/CD/DO 与 EDNS 规范化、target 子树定向变化，以及入口/身份/规则变化不直接切换答案 key。
- 正/负/failure TTL、REFUSED/空 Answer 的 10s 准入与到期回源、RR/SOA TTL 同步及 override 隔离、质量 CAS、并发乱序与 entry v2/v3 恢复。
- 多 waiter 取消、candidate drop、commit 终态、占位上限和关闭后拒绝。
- uniform/heterogeneous 成员 ECS、scope 矩阵、最长前缀、source 0 exact 与 scope 0 covering、无 ECS 桶隔离、single-flight 精确合并；刷新水位清理、并发更新保护、新条目失效不清理及 late-result AnswerKey 接纳。
- Moka 全局 weight、前缀计数索引增删/恢复重建、expiry、分批导出、key v2 冷启动及 format/header/checksum/recovery；预算不等于实际文件大小。
- 真实替换/权限/空间失败与恢复不破坏上一份快照、不阻塞 DNS；测试 hook 不替代真实介质。
- 显式失效范围、历史 owner drain、失败摘要，以及 ECS/IP/Secret 不进入普通缓存日志。

这些是验证要求，不是本次通过记录。当前构造与证据见[后台服务实现](../../../implementation/backend/background-services.md)。
