# 负缓存（NODATA/NXDOMAIN）新鲜度收敛方案

> 文档状态：有效
>
> 适用范围：response cache 中 `NOERROR/NODATA`、`NXDOMAIN` 及失败类条目的 TTL、乐观（stale）返回窗口、后台刷新观测
>
> 计划状态：待验收
>
> 代码基线：`e811b713292467ce964534c1cb7e2ef44002cdf6`
>
> 关联文档：[Cache 模块设计](../architecture/backend/modules/cache.md) · [配置参考](../implementation/configuration.md) · [DNS 管线](../implementation/backend/dns-pipeline.md)
>
> 实施状态（2026-10-04）：§7 步骤 1–6 的代码与文档沉淀已完成，按 §10 建议值实施；当前行为以[配置参考](../implementation/configuration.md#81-dnscache)、[DNS 管线](../implementation/backend/dns-pipeline.md#负缓存新鲜度验证)与[前端页面实现](../implementation/frontend/pages.md)为准。`/clients` 页面没有乐观缓存编辑项，未加入新字段。剩余仅 §9 的本地 `_fluxdns/` 运行验收；完成后删除本方案及索引项。

## 1. 问题

开启乐观缓存后，一个域名在新增解析记录前被查询过并缓存了 `NOERROR` 空应答（NODATA）。近一天后再次查询时，多次请求都由“乐观缓存”返回，每次都触发了“后台刷新 / 更新缓存”，但客户端迟迟拿不到已经生效的新解析。

要求是：负应答不能导致频繁同步回源而拉高客户端耗时，同时新解析应在下一次或随后几次查询中生效。

## 2. 结论摘要

1. **当前 NODATA 的 TTL**：取 authority 段 SOA 的 `min(RR TTL, SOA.MINIMUM)`；没有 SOA 时取响应中任意 RR 的最小 TTL；仍没有时使用 `dns.cache.failure_ttl`（默认 `5s`，允许 `1s..=5m`）。**没有上限。** 本例的 `mjbbs.com` 托管在 Cloudflare，SOA 为 `1800 / MINIMUM 1800`，所以条目最长保持 fresh 30 分钟。
2. **根因不是 TTL 本身，而是乐观窗口对负应答没有区分**：条目过期后，`stale_until = expires_at + optimistic.max_age`（默认 `86400s`）。这个窗口对正应答、NODATA、NXDOMAIN 甚至 SERVFAIL/TC 都相同。所以 00:03 缓存的空应答在 23:44 仍被视为“可乐观返回”。乐观缓存本身就是“本次返回旧值、后台刷新、下次才拿到新值”，第一次查询因此必然拿到一个将近 24 小时前的“不存在”。
3. **建议方案**（不增加常态同步回源）：
   - **核心**：为负应答单独设置较短的乐观窗口（建议默认 `5m`），失败类（SERVFAIL/TC）不允许乐观返回。超出窗口的冷门负条目按 `Expired` 同步回源一次；热门负条目（如大量 IPv4-only 域名的 AAAA、HTTPS NODATA）仍在窗口内由后台刷新维持，不增加耗时。
   - **建议**：为负缓存 TTL 增加上限（建议默认 `300s`），同时把客户端可见的 SOA TTL 截到相同上限。这样即使 SOA.MINIMUM 很大，新增记录在 FluxDNS 这一层也最多 5 分钟可见。
   - **观测**：在后台刷新活动中记录刷新拿到的响应类和 TTL，并在列表中区分 NODATA，避免“NOERROR · answered”掩盖空应答。
4. 上游递归解析器自身的负缓存（同样按 SOA.MINIMUM，可达 30 分钟）不在 FluxDNS 控制范围内，本方案无法消除，只能让它在详情中可被辨认。

## 3. 现状依据

| 行为 | 代码位置 | 事实 |
| --- | --- | --- |
| 响应分类 | [`classify_response`](../../backend/src/dns/message.rs) | `NOERROR` 且 answer 段为空即 `NoData`。answer 段只要有 CNAME 就算 `Positive`（本例当前 AAAA/HTTPS 都是只有 CNAME 链的 `Positive`） |
| 负 TTL 提取 | [`extract_ttl_metadata`](../../backend/src/dns/message.rs) | 仅 `NoData/NxDomain` 计算 `negative_ttl = min(SOA RR TTL, SOA.MINIMUM)`，只看 authority 段 |
| 负 TTL 准入 | [`admit_response` / `negative_ttl`](../../backend/src/cache/admission.rs) | `negative_ttl` → `min_ttl` → `failure_ttl`，**无上限**；质量为 `Negative` |
| 乐观窗口 | [`admit_response`](../../backend/src/cache/admission.rs) | 所有准入类都设置 `stale_until = expires_at + optimistic_max_age`，与响应类无关；`optimistic_max_age` 取所有启用池中最大的 `max_age`（[`cache_runtime_options`](../../backend/src/dns/policy.rs)） |
| stale 判定 | [`CacheFacade::lookup_at`](../../backend/src/cache/service.rs)、[`stale_answer_ttl`](../../backend/src/dns/policy.rs) | 只比较时间和当前池的 `max_age`，不看 `response_class/quality`。设计文档要求“响应类允许”才可返回 stale（[Cache 设计 §5](../architecture/backend/modules/cache.md)），但实现里没有类别过滤，这是设计与实现的差距 |
| stale 应答 TTL | [`CanonicalResponse::set_ttl`](../../backend/src/dns/message.rs) | 所有 RR（包括 SOA）统一改为 `optimistic.answer_ttl`（默认 `10s`），之后仍会应用 TTL override |
| fresh 应答 TTL | [`fresh_cache_response`](../../backend/src/dns/policy.rs) | 按已缓存时长递减原始 RR TTL；负应答的 SOA TTL 原样透传给客户端（本例最长 1800s） |
| 后台刷新 | [`schedule_optimistic_refresh`](../../backend/src/dns/policy.rs) | 每次 stale 命中都提交一次后台 exchange（2s deadline），结果按版本 CAS 写回；`refresh permit` 只在一次 lookup 内去重，不跨请求合并 |
| 刷新观测 | [`CacheActivity`](../../backend/src/dns/trace.rs) | 只有 kind/outcome/upstream 名称，**不记录刷新得到的响应类和 TTL**；“更新缓存”无法区分写回的是新记录还是又一个 NODATA |
| 列表结果 | [`outcome_from_row`](../../backend/src/storage/detail_query.rs) | `answered` 仅由 rcode 推导，NODATA 也显示为 “NOERROR · answered”；详情中保存了最多 16 条 answers，可用于核对 |

## 4. 截图时间线分析

外部核对（2026-10-04，`dns.google` JSON API，本机执行）：

- `mjbbs.com SOA`：`TTL 1800`，`MINIMUM 1800`。
- `ai-load.mjbbs.com A`：CNAME 链 `smart → cdn → hk.cdn4mjj.online`，链尾 A 记录 TTL `30`；AAAA/HTTPS 只返回同一 CNAME 链和 `cdn4mjj.online` 的 SOA（TTL 300）。

据此推演：

| 时间 | 观测 | 解释 |
| --- | --- | --- |
| 10/3 00:03 | 请求上游 / 新建缓存 | 当时为 NODATA，`expires_at ≤ 00:33`，`stale_until ≈ expires_at + max_age`（默认再加 24h） |
| 23:44:26 | 乐观缓存 / 后台刷新 / 更新缓存 | 已过期约 23 小时，仍在乐观窗口内，于是返回 00:03 的空应答（SOA TTL 改为 `answer_ttl`），后台刷新写回结果 R1 |
| 23:46:35 | 再次乐观缓存 / 后台刷新 / 更新缓存 | 距上次 129s 又是 stale，说明 R1 的 `expires_at` 已过，即 R1 的 TTL < 129s |

推论（未经运行数据验证）：

- 如果 R1 是带 SOA 1800 的 NODATA，23:46 应当是“命中缓存”而不是“乐观缓存”。“乐观缓存”与链尾 TTL 30s 的正应答更吻合，也就是说 **23:46 返回的很可能已经是新记录**，持续失败可能来自客户端（sing-box、浏览器、系统）缓存或 TTL override 的下限。
- 另一种可能是上游递归解析器仍持有负缓存，并返回了 SOA TTL 已衰减到不足 129s 的 NODATA，或者没有 SOA 的 NODATA（此时回退为 `failure_ttl` 5s）。
- **如何判别**：打开 23:46:35 那几行的详情，查看 answers。有 CNAME/A 说明 FluxDNS 已在第二次查询收敛；为空则是上游仍返回 NODATA。这个缺口正是 §6.3 要补的观测。

两种情况下都有一个确定的问题：**第一次查询（23:44）返回了将近 24 小时前的“不存在”**。这完全由负应答共用 24h 乐观窗口造成，也是本方案的主要修复对象。

## 5. 目标与非目标

目标：

- 冷门负条目（长时间无人查询）过期后，第一次查询即拿到上游当前结果，不再返回长期陈旧的“不存在”。
- 热门负条目继续由后台刷新维护，常态下客户端不因负应答增加同步回源耗时。
- 新增记录在 FluxDNS 层的最长不可见时间可控、可配置。
- 能从解析记录判断刷新拿到的是新记录还是仍为空。

非目标：

- 不改变正应答的乐观窗口和 TTL 语义。
- 不改变上游递归解析器的负缓存行为，也不主动绕过上游缓存（例如加随机前缀、CD 位等）。
- 不引入“stale 时同步等待上游一小段时间”的混合模式（见 §6.5）。
- 本方案不新增按域名清除缓存的管理接口（作为后续独立需求，见 §6.4）。

## 6. 变更设计

### 6.1 按响应类区分乐观窗口（核心）

新增配置 `optimistic.negative_max_age`（duration），与 `answer_ttl`、`max_age` 并列，在全局 `dns.cache.optimistic`、策略级和客户端级 `cache.optimistic` 中都可用，继承规则与 `max_age` 一致：

- 语义：`NODATA/NXDOMAIN` 条目过期后仍可乐观返回的最长时间；`0s` 表示负应答从不乐观返回。
- 默认值：`5m`。使用 `serde(default)`，旧配置无需改动即可加载。
- 校验：`0s <= negative_max_age <= max_age`。
- 失败类（`SERVFAIL`、上游 `TC`）一律不乐观返回。serve-stale 的意义是在上游不可用时提供“可用旧答案”，返回旧的失败没有价值。

实现要点：

1. [`CacheAdmissionPolicy`](../../backend/src/cache/admission.rs) 增加 `negative_max_age: Option<Duration>`，取各启用池中 `negative_max_age` 的最大值，由 [`cache_runtime_options`](../../backend/src/dns/policy.rs) 汇总。`admit_response` 按质量计算 `stale_until`：`Complete` 用 `optimistic_max_age`，`Negative` 用 `negative_max_age`，`Failure` 为 `None`。
2. [`CacheDecision::Pool`](../../backend/src/policy/plan.rs) 增加 `optimistic_negative_max_age`；[`stale_answer_ttl`](../../backend/src/dns/policy.rs) 改为接收条目的 `quality`（或 `response_class`），按类别选择当前池的窗口。这样快照中按旧规则恢复的长 `stale_until` 负条目，也会在 lookup 时被当前池规则截断，无需迁移快照格式。
3. 超出窗口的负条目沿用现有 `Expired` 路径：single-flight 同步回源、`cache_status = expired`、按版本 CAS 替换。不新增状态。

效果推演（默认 `negative_max_age = 5m`）：

| 场景 | 现状 | 方案后 |
| --- | --- | --- |
| 本例：NODATA 00:03 缓存，23:44 首次再查 | 返回 23h 前的空应答，下一次才可能拿到新值 | 早已超出 `TTL + 5m`，按 `expired` 同步回源一次（约 100–250ms），**第一次查询即拿到新记录** |
| 热门 AAAA NODATA（v4-only 站点，持续有查询） | 过期后乐观返回 + 后台刷新 | 不变：查询间隔小于 5 分钟时始终在窗口内，后台刷新维持，不增加耗时 |
| 冷门负条目 | 24h 内都可能返回旧的“不存在” | 每个 key 最多每 `TTL + 5m` 同步回源一次，频率很低 |
| 缓存的 SERVFAIL | 过期后可乐观返回旧 SERVFAIL 长达 `max_age` | 过期即回源 |

### 6.2 负缓存 TTL 上限（建议）

新增全局配置 `dns.cache.negative_ttl_max`（duration，与 `failure_ttl` 同级，仅全局）：

- 语义：`NODATA/NXDOMAIN` 条目 fresh 生命周期的上限，即 `expires_at = now + min(negative_ttl, negative_ttl_max)`。
- 作用范围：只对 `ResponseClass::NoData/NxDomain` 生效，准入和输出两个阶段都按响应类判断，不按 RR 类型或是否带 SOA 判断。正应答（包括 answer 段只有 CNAME 链的 `Positive`）、SERVFAIL/TC 都不受影响，仍分别使用 RR 最小 TTL 和 `failure_ttl`。正应答 authority 段里偶尔带的 SOA/NS 也不会被截断。
- 默认值：`300s`。校验范围建议 `1s..=1d`（实施时最终确定）。
- 客户端可见 TTL：负应答在输出阶段（fresh 命中、上游直出、single-flight 共享结果）把 authority/additional 段 RR TTL 截到 `negative_ttl_max`，并且不超过条目剩余寿命。处理顺序为先截上限，再应用 TTL override，避免客户端按 SOA 1800s 自行长期缓存“不存在”。用户显式设置的 TTL override `min` 仍可能重新放大该值，这是用户自己的选择，需要在配置文档中说明。
- 不修改缓存中保存的 origin response，以符合“候选不得被输出覆写污染”的设计约束。

作用：SOA.MINIMUM 为 1800/3600 甚至 86400 的 zone，新增记录在 FluxDNS 层最多 5 分钟可见。热门负条目会更频繁地进入窗口，但刷新都在后台进行，上游 QPS 增量上限约为“热门负 key 数 / 300s”。

### 6.3 刷新结果可观测

- [`CacheActivity`](../../backend/src/dns/trace.rs) 增加可选字段 `response_class`（`positive`、`nodata`、`nxdomain`、`servfail`、`truncated`）和 `ttl_secs`。只有刷新或写入实际产生候选时才填充。字段要同步到详情分片存储、[OpenAPI](../../frontend/openapi/management-api-v2.yaml)、生成类型和前端标签，例如“更新缓存 · NODATA 300s”。
- 列表结果区分空应答：rcode 为 `NOERROR` 且 answer 数为 0 时显示 “NOERROR · NODATA”。如果列表载荷尚未包含 answer 数，先在详情中展示，列表展示作为同批可选项，由实施时的接口核对决定。
- 调试日志 `operation = "cache_refresh"` 增加低基数的 `class` 字段。

### 6.4 后续独立需求（本方案不实施）

在管理面按域名或 key 清除缓存，作为运维兜底，例如刚改完 DNS 时手动清除。这需要新增 Management API 和权限评审，应另立方案。

### 6.5 已评估但不采用

| 方案 | 不采用原因 |
| --- | --- |
| 负应答完全不缓存或不乐观 | AAAA/HTTPS NODATA 占比高，每次过期都同步回源会明显拉高常态耗时 |
| stale 负条目同步等待上游 N ms，超时再返回 stale（类似 serve-expired-client-timeout） | 本例上游 RTT 100–250ms，等待时间要足够长才有效，等于对所有负 stale 增加耗时；§6.1 已覆盖冷门条目 |
| 负条目 refresh-ahead（剩余 TTL 不足时预取） | 只能减少 stale 返回次数，不能解决“冷门条目首查拿到旧值”；增加调度复杂度，收益有限 |
| 仅调小全局 `max_age` | 会同时削弱正应答的乐观收益，与“不增加耗时”的目标冲突 |

## 7. 实施步骤

1. 配置契约：在 [`OptimisticDto`](../../backend/src/config/model.rs)、[`GlobalCacheV2`](../../backend/src/config/contract.rs) 默认值、[`ResolvedOptimistic`/`ResolvedGlobalCache`](../../backend/src/config/resolve.rs) 和 [`validate_optimistic`](../../backend/src/config/validate.rs) 中加入 `negative_max_age`、`negative_ttl_max`，同步配置示例和 [配置参考](../implementation/configuration.md)。
2. 缓存准入：`CacheAdmissionPolicy` 加入负窗口和负 TTL 上限；`admit_response` 按质量计算 `expires_at/stale_until`；补充单元测试，覆盖 NODATA、NXDOMAIN、SERVFAIL 和正应答不受影响。
3. 策略层：`CacheDecision` 携带池级负窗口；`stale_answer_ttl` 按类别判定；在 Fast/Resolved 两条 stale 路径、late-result 和 refresh 写回处核对一致性；输出阶段对负应答截断 TTL。
4. 观测：扩展 `CacheActivity`、详情存储、OpenAPI、生成类型和前端标签；列表区分 NODATA。
5. 前端配置页：在 `/dns-settings`、`/strategies`、`/clients` 中加入新的 duration 字段（`DurationInput`），保证缺失和零值的往返保真。
6. 文档收口：实现完成后更新 [Cache 设计 §3/§5](../architecture/backend/modules/cache.md)（负/失败类乐观规则、负 TTL 上限）、[配置参考](../implementation/configuration.md)、[DNS 管线](../implementation/backend/dns-pipeline.md)和前端实现文档，然后删除本方案及索引项。

## 8. 风险

- **行为变化**：默认值会让负应答更早过期、更早停止乐观返回。上游故障期间，冷门负条目不再兜底返回旧的“不存在”。由于旧的“不存在”本身价值很低，这一风险可以接受。
- **上游 QPS**：`negative_ttl_max = 300s` 会提高热门负 key 的后台刷新频率。需要在发布说明中给出调整方式，必要时可调大。
- **快照兼容**：旧快照中的负条目带有长 `stale_until`，依靠 lookup 端按类别截断来保证正确，不改变快照格式版本。
- **多池汇总**：admission 使用各池最大值，lookup 使用当前池的值。必须保持“admission 宽、lookup 严”，否则某个池的较大窗口会被截断。
- **上游负缓存**：上游递归解析器的负缓存仍可能让新记录延迟最长 SOA.MINIMUM。§6.3 的观测只用于定位，不能消除。

## 9. 验证与退出条件

- 单元测试：admission 按质量计算 `expires_at/stale_until`；负 TTL 上限生效；SERVFAIL 不设 stale；正应答结果不变。
- 策略层测试：NODATA 条目超出 `negative_max_age` 后 lookup 为 `Expired`，同步回源并替换；窗口内为 `Stale` 并触发后台刷新；池级覆盖生效；旧长 `stale_until` 条目被当前池截断。
- 输出测试：fresh 负应答的 SOA TTL ≤ `negative_ttl_max`，且不超过剩余寿命；随后才应用 TTL override。正应答（含只有 CNAME 的 answer、authority 带 SOA 的正应答）的条目 TTL 和客户端可见 TTL 与改动前完全一致。
- 配置测试：缺失新字段时使用默认值；越界值被拒绝；前端往返保真。
- 运行验收（`_fluxdns/` 本地夹具，遵循[本地测试规则](../rules/local-testing.md)）：用可控上游先返回 NODATA，再切换为 A 记录。核对冷门条目首查拿到新记录、热门条目只有后台刷新，详情能显示刷新拿到的响应类和 TTL。
- 执行 `cargo test`、`clippy`（按[环境规则](../rules/environment-usage.md)）、前端 `typecheck` 和 Vitest，以及文档检查。
- 退出条件：以上验证通过，实现和设计文档已同步，本方案及索引项已删除。

## 10. 已决策项（按建议值实施）

| 决策 | 建议 | 备选 |
| --- | --- | --- |
| `negative_max_age` 默认值 | `5m` | `0s`（负应答不乐观，冷门首查必回源）/ `30m` |
| 是否引入 `negative_ttl_max` 及默认值 | 引入，`300s` | 不引入（完全遵循 SOA）/ `60s`（更快收敛，刷新更频繁） |
| 失败类是否允许乐观 | 不允许 | 跟随 `negative_max_age` |
| 列表区分 NODATA | 与观测同批实施 | 仅在详情中展示 |
