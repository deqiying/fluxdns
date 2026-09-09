# WebUI 管理后台重构配套后端重构方案

> 文档状态：草案
>
> 计划状态：待评审
>
> 适用范围：WebUI 重构所需的客户端身份、查询关联、缓存快照、统计/详情保留及管理接口改造
>
> 上位需求：[WebUI 管理后台重构需求](webui-management-requirements.md)
>
> 代码基线：`21fd23f3f711e2f7acc712b9ff715915c5248180`（2026-09-07 决策修订与配置/Runtime 定向核对；存储现状保留既有静态依据）

详细开发拆解见[后端开发计划](webui-management-backend-development-plan.md)，对应页面见[前端开发计划](webui-management-frontend-development-plan.md)；跨端顺序、阶段门槛及分批提交规则统一见[开发总计划](webui-management-development-plan.md)。本文继续维护目标设计与不变量，不重复维护子计划的任务和 Git 检查点。

## 1. 目标与边界

本方案按[已确认决策](webui-management-decisions.md)修订，不是当前实现说明。本轮只修改方案文档，不修改 Rust、正式配置、数据库、OpenAPI 或前端代码；不执行数据删除或服务重启。

以下五项为本次目标约束：

1. 各配置模块 `name` 是唯一管理/引用键；客户端另有一个唯一 `client_id` 用于请求匹配和历史归属。保留 ID 优先、IP/CIDR 回退；IP 匹配不能补造请求原始 ID。
2. 请求详情采用“原始 ID/IP＋最小匹配结果”：保存 `client_id`、`client_ip`、`matched_client_id` 和 `client_match_source`。TCP/UDP 等未携带 ID 的请求仍保存 `client_id = null`；不保存客户端名称或完整配置快照。
3. DNS 内存缓存保持共享容量预算；缓存持久化改为独立、可丢失的非 SQLite 快照，用于启动预热，不承担统计或详情存储职责。
4. 取消缓存持久化大小配额和详情记录数上限。缓存周期性覆盖快照；历史数据按统计配置中的天数、参考大小和额外宽限期，在每天 01:00 清理。
5. 统计与详情共用一份保留配置和一个有效清理截止日期；`dns.resolve_log` 只保留详情记录开关。

配置编辑先热应用再持久化，外部文件变化仅提示；完整流程见[配置专项](webui-management-config-runtime-plan.md)。`logs` 改为热更新，`database/webui/work` 本期只读。取消旧版迁移和兼容，采用全新开发基线，不自动删除本地文件。

## 2. 现状与差异

| 核对对象 | 当前源码事实 | 目标变化 |
| --- | --- | --- |
| [客户端模型](../../backend/src/config/model.rs)、[匹配索引](../../backend/src/policy/client.rs) | 以资源名称定位，`match.ids` 为数组，ID 匹配后可回退 CIDR | name 仍是唯一管理键；单 client_id 为匹配身份；保留最长前缀 |
| [观测事件](../../backend/src/ports/observation.rs)、[详情投影](../../backend/src/storage/resolve_log.rs) | `ResolutionDetailSource` 有 IP，无请求原始 ID；详情保存匹配后的 `client_bucket` | 接入时捕获原始 ID/IP，Policy 另行冻结实际匹配 ID/来源，两者分别传至详情存储 |
| [分片查询读口](../../backend/src/storage/detail_query.rs)、[响应映射](../../backend/src/management/query.rs) | 新读口已返回原始身份与当前名称；旧单库读口已删除 | 返回原始身份、当时匹配结果和当前显示名称，不重算历史 IP 匹配 |
| [缓存装配](../../backend/src/dns/policy.rs)、[SQLite 缓存](../../backend/src/cache/sqlite.rs) | 生产链使用独立 SQLite 缓存及异步增量写入，有持久化编码预算 | 内存为权威，后台周期性生成完整二进制快照，不保留增量数据库镜像 |
| [文件缓存 adapter](../../backend/src/cache/persistence.rs) | 已有 `FDCP` magic、版本与条目 codec，但维护第二份记录集合，并受文件容量限制 | 复用经审查的编码能力；不能只更换扩展名或直接替换为现有 adapter 就宣称完成 |
| [详情写入](../../backend/src/storage/sqlite.rs) | `apply_resolve_records_with_limits` 在批写事务内按年龄删除、计数、按条数淘汰并计算可写余量 | 批写只做有界入队/插入，移除按条数扫描与历史清理；独立后台任务回收日期分片 |
| [业务存储](../../backend/src/storage/service.rs)、[v2 统计布局](../../backend/migrations/0001_statistics.sql) | 统计与详情已分离，事件时间为 UTC 毫秒，统计按 UTC 日聚合 | 保留统计 SQLite；详情迁往 UTC 日分片目录，统一保留协调器 |
| [现有 API](../../frontend/openapi/management-api-v1.yaml) | 配置查询/写入、原始客户端 ID、保留状态等目标能力尚不完整；统计窗口固定上限另有校验 | 同批修改 ports、DTO、OpenAPI 和生成类型，不只修改页面 |

上述为定向静态核对，不覆盖全仓库行为或运行验收。现有 [Config](../architecture/backend/modules/config.md)、[Policy](../architecture/backend/modules/policy.md)、[Cache](../architecture/backend/modules/cache.md)、[Storage](../architecture/backend/modules/storage.md) 仍描述已接受的旧契约；本方案实施时再同步这些文档，不能提前把计划写成实现事实。

## 3. 目标数据流

```text
Transport
  └─ RequestIdentity { actual_client_id?, effective_client_ip }
       ├─ Policy：ID 优先，未命中/无 ID 时按最长 IP/CIDR 前缀
       └─ ResolutionEnvelope：原始身份 + 当时匹配 ID/来源
            ├─ Cache commit → 有预算的内存缓存 → 周期性 .db 快照
            ├─ Statistics → 业务 SQLite 中的 UTC 日聚合
            └─ Detail（开关开启时）→ queries/YYYY-MM-DD.sqlite3

Management query
  └─ 已持久化的原始身份/匹配结果 + 当前客户端目录快照
       └─ 原始记录 + 当时匹配结果 + 当前名称（名称不写回详情）

Retention coordinator（每天服务器时间 01:00）
  └─ 统计配置 + 详情文件大小快照 → 统一 UTC 日期截止线
       ├─ 删除过期统计日
       └─ 回收过期详情日文件
```

请求路径不得执行文件快照、SQL 查询、历史计数、日期清理或管理目录关联。后台队列继续有界；删除业务条数配额不等于取消队列、批次、内存和查询结果的保护上限。

## 4. 客户端身份与最小匹配结果

### 4.1 客户端配置

建议新 schema 的客户端条目使用以下逻辑字段，具体序列化形态在实施时纳入正式配置参考：

| 字段 | 目标契约 |
| --- | --- |
| `client_id` | 必填、单个、非空、全局唯一；沿用实际协议 ID 的大小写语义，不按显示名称生成 |
| `name` | 必填、client 命名空间内唯一，可改名的管理/引用键；不作为统计维度 |
| `match.ips` | 可选 IPv4/IPv6 地址或 CIDR 数组；保留请求期的 IP 策略匹配，单地址按 /32 或 /128 归一化 |
| `strategy`、`cache`、`ttl_override`、`edns_client_subnet` | 保留已有覆盖语义，对 ID 或 IP 命中的客户端配置生效 |

客户端 ID 创建后普通编辑只读，name 按旧名定位并更新名称引用；不因改名重写历史匹配 ID。所有模块名称改动使用同一类型化引用更新规则，上游与组共享名称空间。普通配置引用只用最新 name，请求身份不因此改成名称字符串。

IP/CIDR 是匹配条件，不是管理键。不同前缀包含允许，重复规范化 CIDR/同优先级冲突在候选校验时拒绝，不能靠列表顺序选择。name 和 client_id 分别校验唯一；IPv4-mapped IPv6 在 transport、配置和匹配端统一归一化。

实际 ID 命中优先于 IP；无 ID 或 ID 未命中时仍可回退 IP/CIDR，保持现有行为。未知实际 ID 可以与命中的配置 ID 不同，这正是原始身份与匹配结果必须分开的原因。

### 4.2 请求事实与决策事实

新详情记录必须始终包含 `client_ip` 和 `client_id` 两列；值反映接入事实：

- `client_id` 只来自 transport 实际解析的 ID。未传入、协议不支持或裸 DoH 路径没有 ID 时为 `null`；不能存空字符串、配置名称、关联 ID 或 IP 推断结果。
- `client_ip` 保存本次请求的有效客户端地址。UDP/TCP 来自对端；DoH 保持已有可信代理校验后的有效来源，不接受未经信任的 Header。它不是客户端配置中的关联 IP，也不是事后更新的地址。
- 不能通过某个配置是否存在决定要不要保存实际 ID。请求传入未配置 ID 时也原样保存，以便以后查询。
- 原始身份捕获之后，Policy 使用同一请求配置快照另行生成 `matched_client_id` 和 `client_match_source`，记录实际命中的客户端配置 ID 及 `id`/`ip`/`none`，不能在后台按最新配置重新匹配。
- `name`、IP 配置列表和完整客户端配置不进入详情表。查询得到的当前名称只是显示信息；策略、上游和 Answer 等其他请求/决策事实继续保留。
- transport 校验完成后即固定身份，进入后台队列后也不受客户端改名、IP 修改或配置 reload 影响。`Debug`、日志及未认证接口继续脱敏。

约束如下：`source=id` 时原始 ID 非空且等于匹配 ID；`source=ip` 时匹配 ID 非空，原始 ID 可以为空，也可以是未配置的其他 ID；`source=none` 时匹配 ID 为空。实际 ID 未命中且 IP 也未命中时，原始 ID 不得被清空。

本期只接纳新格式详情，不再设计旧版 `identity_status`/`legacy_unknown` 兼容路径。新记录按上述三种来源保证字段约束；不能按现有配置推导或改写已经完成请求的事实。

### 4.3 请求期匹配矩阵

| 原始 ID | 原始 IP | 请求时配置 | 持久化结果 |
| --- | --- | --- | --- |
| A | 任意 | ID A 存在，IP 可命中 B | 原始 A；匹配 A；来源 `id` |
| A | 有值 | A 不存在，IP 命中 B | 原始 A；匹配 B；来源 `ip` |
| 未传入 | 有值 | IP 命中 B | 原始 `null`；匹配 B；来源 `ip` |
| A 或未传入 | 有值 | ID/IP 均未命中 | 原始值不变；匹配 `null`；来源 `none` |
| 任意 | 有值 | IP 命中不同长度前缀 | 最长前缀决定匹配 ID；同优先级冲突在配置阶段拒绝 |
| 历史身份/匹配未保留 | 已保留或缺失 | 只有当前配置 | 保持历史未知，不补写或伪装成新的请求期匹配 |

DoH 与 UDP 请求即使来自同一 IP，也可能匹配不同配置。IP 策略匹配保留业务效力，但不证明物理设备身份；NAT、DHCP 与共享代理仍需由管理者配置可信的规则。

### 4.4 查询显示与过滤

Management 按已保存的 `matched_client_id` 批量查询当前客户端目录，仅补充“当前显示名称”和存在状态，不按当前 IP 绑定重新归属历史记录。同一页捕获一份目录 revision，避免混用不同版本或逐行查询配置。

原始身份和最小匹配结果保留在记录顶层，当前名称放入独立显示对象，例如 UDP 请求的拟定响应片段：

```json
{
  "client_id": null,
  "client_ip": "192.0.2.101",
  "matched_client_id": "workstation",
  "client_match_source": "ip",
  "identity_status": "recorded",
  "matched_client_display": {
    "current_name": "工作站",
    "status": "present"
  },
  "client_directory_revision": "demo-revision"
}
```

这不是现有 API。列表可显示当时匹配 ID、当前名称和 ID/IP 标记；详情分为“请求事实”“当时策略匹配”“响应记录”。原始 ID 为空时显示“未传入”，不能把 `workstation` 放到原始 ID 的位置。

例如同一 IP 先匹配 A、后来改为 B，旧详情仍显示“当时匹配 A”；A 改名后可显示 A 的当前名称，A 被删除时保留匹配 ID 并标为“配置已删除”。ID 删除后复用也不能证明当时的名称或物理设备身份，必须继续标明“当前名称”。如未来需要历史名称或完整配置还原，应另行设计版本化配置快照，不在每条请求复制客户端对象；现有 `runtime_revision` 本身不是历史配置存档。

最小匹配结果表达的是当时实际采用的配置归属，不是身份证明，也不是精确的 IP 规则命中快照。本轮不再保存匹配 CIDR、名称副本或整个策略配置；当 IP 配置已变更时，不能仅凭匹配 ID 解释当时具体命中了哪条 CIDR。如需这一层审计能力应另行评审，不扩大本次记录开销。

客户端过滤默认使用历史 `matched_client_id`；“原始客户端 ID”“请求 IP”分别过滤原始列，不与请求记录自身的 `request_id` 混淆。按当前名称搜索时先解析成当前 ID 集合，再在数据库分页前过滤匹配 ID，不能重新按当前 IP 计算历史归属或在前端分页后筛选。查看浮层期间冻结记录及当前名称，关闭或刷新后再应用目录变更，沿用已有的新增记录缓冲规则。

### 4.5 对策略、缓存和统计的影响

`Policy::ClientIndex` 保留 ID 优先、最长 CIDR 回退。ID/IP 命中均可使用相应客户端策略、独立缓存、TTL 和 ECS；均未命中才继承入口/路由。保存匹配结果不增加一次策略计算，直接复用请求已固定的 `ClientMatch`。

保留当前按实际匹配身份隔离客户端缓存的语义：ID 命中以实际 ID 的域分隔摘要隔离，IP 命中以实际请求 IP 的域分隔摘要隔离，并结合生效策略。不能只因 CIDR 命中了同一 `matched_client_id` 就把整个网段合并成一个客户端池。显示名称不影响缓存；IP 匹配规则或覆盖配置变化仍需让受影响请求使用正确 fingerprint。

统计客户端维度按请求当时的 `matched_client_id` 聚合，ID/IP 命中都归到命中配置，无匹配才归 `unknown`；不存显示名称，不把任意原始 ID/IP 作为无界统计维度。配置 reload 时冻结事件的匹配 ID，不按消费者看到的最新目录改写归属；维度数量和删除后遗留 ID 继续受有界基数保护，不因目录持续变更无限增长。

## 5. 缓存快照

### 5.1 格式与所有权

使用单个 `dns-cache.db` 二进制快照，不是改名后的 SQLite 文件。扩展名不定义格式：文件头明确 magic、快照版本、key/entry 版本、生成时间及完整性信息；复用已有条目 codec 的 TTL、canonical wire、namespace、语义 fingerprint 和缓存生产上游 provenance。

内存缓存是唯一权威集合。文件写入端不再维护独立的全量 `HashMap` 镜像，不保留每次缓存提交的持久化增量队列；被内存预算淘汰或手动清理的条目应在下一次完整快照中消失。

缓存文件、临时文件和清理标记只能放在缓存专属路径下，与业务统计数据库及详情目录分离。配置校验拒绝路径相同、覆盖业务库/分片目录以及危险别名；运行时文件操作还须核对规范化目标和文件身份，不能只比较原始字符串或扩展名。

### 5.2 周期写入

按 D-06 默认每 5 分钟生成快照，可配置正 duration；没有内存变化时可跳过。过程如下：

1. 一个进程级 worker 捕获当前缓存代际和有界遍历游标，只导出仍可用的全局、策略、客户端池条目。
2. 分批取得条目引用，释放锁后编码并顺序写入同目录临时文件；不得长时间锁住查询，也不得复制一整份缓存加完整序列化大 buffer。
3. 同一快照中按 key 去重；并发更新、淘汰导致少量条目未捕获是允许的。不能导出本次遍历未见的旧磁盘条目补齐文件。
4. 完成长度/校验信息，flush/sync 后替换正式快照。发布前确认 owner epoch 和 clear generation 仍有效。
5. 发布失败保留上一份完整快照，报告失败原因并在以后重试，不回滚内存、不阻止 DNS。

Windows 验证同目录替换、权限和中断恢复；Linux 特有实现仅要求代码审查，本轮不专门实测。不能把两文件操作描述为跨文件原子事务。上一轮快照未完成时合并任务，不并发写同一个文件。

不提供持久化 `max_size_bytes`。编码开销、遍历期间的并发变化以及替换时“旧文件＋临时文件”可能使磁盘峰值大于内存预算，不能宣称一比一字节映射。流式读取的单条 wire/长度保护、恢复时间预算与内存准入保护仍必须保留，这些不是用户可配置的磁盘配额。

### 5.3 恢复与失败

首次启动在有限预热期限内尝试恢复，逐条走当前内存预算、类型版本、语义 fingerprint 和有效期校验。记录持久化的是可跨进程解释的到期时间，恢复后扣除停机时间，不能重新给满 TTL；已过乐观缓存可用期的条目直接丢弃。

文件不存在、版本不兼容、校验失败、权限不足或恢复超时均允许冷启动；只将缓存恢复状态标为不可用/部分恢复，不把 DNS 服务整体判为不可用。非法配置和越界路径仍是配置错误，不能借“缓存可选”绕过安全校验。恢复不重放 DNS 请求，不改变上游生产来源。

正常关闭可在现有有界 shutdown 期限内尽力补写一次；超时不无限阻止退出。reload 不重新从旧文件恢复已运行的缓存，持久化 worker 按进程管理；缓存所有权或路径切换后，旧 owner 不得覆盖新快照。

### 5.4 独立清理

未来受限清缓存命令需要同时处理内存和恢复文件，且不触及统计/详情：

- 在排他清理边界递增 generation，使已开始的旧快照失去发布权，并清空内存。
- 用持久化 reset generation/空快照使旧文件不可再恢复；成功确认必须包含这个持久化失效步骤。
- 失效或删除失败时返回“内存已清理、磁盘失效失败”，不得谎报完全成功；保留旧缓存可能在重启后恢复的风险提示。
- 启动时先检查 reset 信息，只接纳 generation 不早于 reset 的完整快照；发布与清理交错必须经过同一个串行仲裁边界。

离线删除缓存文件可以冷启动；运行中只删除文件不等于清空内存，下一次快照可能重新生成。以上只是未来命令契约，本轮不执行清理或删除。

## 6. 统一历史保留

### 6.1 配置归属与定义

保留配置放在顶层 `statistics.retention`，WebUI 在 DNS 配置的“统计数据”分区编辑；详情分区仅保留 `dns.resolve_log.enable` 开关。建议的配置片段如下，属于下一版 schema 提案，不可直接拼入当前 version 1 配置运行：

```yaml
statistics:
  retention:
    days: 7
    grace_days: 3
    reference_size_bytes: 1073741824
dns:
  resolve_log:
    enable: true
  cache:
    memory:
      max_size_bytes: 8388608
    persistence:
      enabled: true
      path: ./data/dns-cache.db
      snapshot_interval: 5m
database:
  type: sqlite
  path: ./data/fluxdns.sqlite3
  records_path: ./data/queries
```

`days = R` 是正常统计时间范围，预设 1、3、7、30 天，另提供自定义正整数；`grace_days = G` 是达到 R 后额外保留的天数，可为 0。建议默认 7＋3 天、参考大小 1 GiB；这些默认值是评审建议，不代表从现有条数上限推算出的等价值。

`reference_size_bytes = T` 是详情存储的参考大小，不是缓存配额，也不是全业务库硬上限。清理开始前采样 `S`：所有受管理详情日文件及其 WAL 的逻辑文件长度之和，包含索引与页开销，不包含统计数据库、缓存、SHM、用户备份或迁移副本。测量口径应在 API 中明示，不能用 `COUNT(*) × 固定行宽` 假装真实文件大小。

统计与详情共用依据详情存储压力算出的期限。统计库体积另外只读展示，不让它偷偷改变判断条件；无详情文件时 `S = 0`，统计同样使用 R＋G 天。无法可靠采样 S 时不将其当作 0 或自动选择更激进删除，本轮清理失败并告警。

### 6.2 清理算法与时间

为保持现有统计日契约，统计和详情分片都采用 UTC 日；任务在**服务器时区的每天 01:00** 调度，UI 同时展示调度时区和 UTC 统计日口径。调度时区不是浏览器时区，不在前端换算后反写配置。

令本轮开始时的 UTC 日期为 D，冻结配置 revision、大小采样和有效截止线：

```text
N = R      when S > T
N = R + G  when S <= T
keep_from_day = D - (N - 1) days
删除 day < keep_from_day 的统计日和完整详情日文件
```

“N 天”包含当前 UTC 日；等于大小阈值仍享受宽限期；正好位于截止日的记录保留。两种条件都只能清理超期数据，不能为了压到 T 以下删除有效期内的记录。默认查询范围是 R 天，宽限期内尚存的历史可通过显式扩展时间范围查询，并标为额外保留数据。

示例：本轮 UTC 日期为 9 月 7 日，R=7、G=3、T=1 GiB。若 S=1.2 GiB，则保留 9 月 1 日及之后；若 S=0.8 GiB，则保留 8 月 29 日及之后。统计和详情一律使用本轮同一条截止线，不在清理一半、文件变小后临时切换到另一档。

时区切换、夏令时重复/跳过、系统时间回拨与漏跑需测试：以服务器日任务键去重，跳过的时刻在当天首次有效机会执行；进程启动晚于计划时间时只补做一次当前应执行任务，不按停机天数连续回放多次。重新计算下一次墙上时钟时间，不用固定睡眠 24 小时代替每天 01:00。

### 6.3 详情按日分文件

建议继续使用已安装的 SQLite/SQLx 处理业务数据，但将高频详情放入 `records_path/YYYY-MM-DD.sqlite3`：

- 每个文件只接收事件发生时对应 UTC 日的详情；当前日和有限的迟到写入日使用受控连接池，不能为所有历史天数常驻 writer。
- 请求线程仍只发布有界事件。后台批写不执行 `COUNT(*)`、历史 DELETE、VACUUM 或条数限额计算。
- 事件时间早于有效清理水位时计为 `expired_before_write` 并放弃详情写入，不能重新创建已退役日文件。乱序/迟到事件进入尚未退役的正确日期。
- 全局记录 ID 改为包含分片日期和本地行 ID 的不透明稳定标识；查询按 UTC 时间、稳定 ID 排序，分页游标不能只保存单库自增 ID。
- 时间查询先定位有界日期集合，再合并各分片的索引结果；查询总数按匹配条件跨分片计算，不能在分页后再关联过滤。统计查询不依赖详情仍然存在。

选择日文件是为了回收整日数据，不是假定给 SQLite 增加了自动分区能力。SQLite 删除行通常留下可复用页，不保证文件马上变小；`VACUUM` 又涉及重建和空间成本。因此本方案不把每晚对巨大详情表 DELETE 后紧接全库 VACUUM 作为默认方案。依据见 [SQLite VACUUM](https://www.sqlite.org/lang_vacuum.html)。

`records_path` 与统计库路径属于只读系统配置，通过启动配置设置，不开放普通 WebUI 在线迁移。日文件名来自已解析日期，不能拼接任意请求字符串。

### 6.4 统一水位与文件生命周期

跨统计库与详情文件不承诺物理删除原子完成。保留协调器在统计库事务中写入单调前进的 `retired_before_utc_day` 和任务 manifest，并删除过期的日聚合；只有事务成功后才回收详情文件。管理查询读取同一水位，使过期统计与详情同时退出可见范围。

日文件退役前禁止新增 writer，等待有界写入排空和只读 lease 释放；超时则记为待回收，下次继续。使用 SQLite 正常关闭/checkpoint 能力处理 WAL，再删除确定归属且不再打开的主文件和 sidecar，不在连接仍存活时按扩展名批量删除。WAL 与读者/checkpoint 的关系参见 [SQLite WAL](https://www.sqlite.org/wal.html)。

统计 pending batch 在事务提交前也要检查水位，跳过已退役日期的增量并正常推进幂等确认，不能用重试把已删统计日重新写回。保留 `StatsPersistenceWorker` 的 batch/ledger 去重；ledger 的回收以已确认 epoch/replay 下界为依据，不与统计日期简单同时删除。

某些文件删不掉时保留失败清单和重试，不撤销已发布的逻辑水位，也不谎报释放了磁盘空间。物理释放状态、逻辑截止日和文件压力分别上报。缩小保留期可推进截止线，增大保留期不恢复已经删除的数据，保存前应预览影响并确认。压力从高档降回低档时也不能倒退水位以“恢复”宽限期，后续日期自然累积至新目标。

API 分开返回目标保留天数、已发布逻辑水位、实际可查日期与下一次清理的预计截止日。预览按下次服务器 01:00 对应的 UTC 日期计算，并取其与既有水位中更晚的一条；当前 S 仅用于估算，执行时重新采样。不得把目标 N 天或未经执行的预览当作实际已经保留的日期范围。

### 6.5 压力与开关

每天一次清理和额外宽限期都不是磁盘硬限制，高频流量仍可能在一天内耗尽空间。保留只读大小、剩余空间/写入错误、队列丢弃及清理失败告警；异常时沿用业务写入失败的明确状态，不在后台擅自缩短期限或删除新记录。

关闭详情仅停止后续详情生产，不停止统计、清理任务或既有详情文件大小采样；重新开启不补造关闭期间的请求。实现时将详情开关纳入受控热应用，切换点按事件接入时捕获的开关 revision 固定，不因 worker 处理时开关变化而重解释旧事件。

## 7. WebUI 管理接口配套

| 能力 | 后端工作 |
| --- | --- |
| 客户端资源 | 唯一 name 定位，单 client_id 匹配；名称/IP/CIDR 编辑与冲突校验，移除旧 `match.ids` 数组 |
| 查询列表/详情 | 原始身份、当时匹配 ID/来源、当前名称、目录 revision 和跨日游标；有界 Answer 与缓存生产上游信息 |
| DNS 缓存配置 | 内存预算、可选快照、路径及周期；移除持久化大小配额；展示最近恢复/快照状态，不暴露缓存 wire |
| 统计配置 | 一次提交 R/G/T，返回规范化单位、调度时区、下一清理时间、大小口径、预计截止日和配置 revision |
| 详情开关 | 独立的受控写操作，只切换 enable，不接受保留天数、最大条数或数据库路径 |
| 配置型一级模块 | 在现有 ConfigStore 思路上增加类型化、资源级写入与预校验，不提供整份 YAML 任意替换 API |
| 服务指标与进程信息 | 定义有单位的采样窗口、QPS/RPM、在线身份去重、内存及线程状态，缺数不返回假零 |
| 实时推送 | 鉴权订阅、序列/游标、有限缓冲和断线补齐；记录目录变更不覆盖正在查看的详情 |
| 配置文件同步 | 当前活动源查询、应用后持久化、外部差异提示/还原/组合采用、未同步重试 |

已认证用户都按管理员处理，不增加 RBAC；继续登录、CSRF/Origin 和只读字段保护。写操作携带活动/文件版本及旧 name，候选更新全部类型引用；先运行时应用，后落盘，区分已应用并同步、已应用未同步、应用失败和结果待确认。

常规 DNS 配置、日志、详情开关纳入热应用，快照/保留任务使用明确版本边界；启动级系统路径/绑定仍只读。外部修改不自动 reload，表单以活动源为准，文件还原/差异采用走受限操作。首次用户事务不能直接冒充通用事务，应用补偿和新 journal 语义见配置专项。

统计查询的旧 31 天固定上限需随自定义保留范围重新设计：以实际可用日期与有界响应/查询时间为限制，较大统计窗口采用分段读取或预聚合；禁止界面允许自定义后端仍无说明地截断。服务状态的在线身份不能直接用 IP 关联后的名称去重，可按实际 ID，缺 ID 时按有效 IP 统计，并明确这与已配置客户端数量不同。

## 8. 新基线与兼容退出

按 D-01/D-10 使用新配置/API 和全新数据布局，无现有运行服务需要迁移。删除原迁移预览、旧 ID 映射、SQLite 流式搬迁、legacy 查询及旧版维护回退的要求。

新目录直接初始化，新格式须支持正常关闭后重启、journal/manifest 恢复。误指旧格式时明确拒绝，不静默转化或清空；删除旧代码以新接线完成和引用检查为前提，不顺手删除不明本地数据。新格式故障恢复不能因取消兼容而省略。

## 9. 实施顺序

| 阶段 | 主要修改边界 | 验收门槛 |
| --- | --- | --- |
| B1 契约与新基线 | config model/resolve/validate、ports、v2 schema 与新布局初始化 | name/client_id、热更新状态及 R/G/T 确定，无旧版转换 |
| B2 原始身份链路 | transport → RequestContext → observation → resolution → storage detail | TCP/UDP ID 始终为空，DoH 实际 ID 不丢失、不推断；敏感字段不进入日志 |
| B3 匹配结果与查询显示 | Policy ID/IP 索引、Management 目录索引、查询 DTO/过滤 | 保留 IP 策略行为，记录原始身份与最小匹配结果；名称修改无需历史 UPDATE |
| B4 缓存快照 | cache ports、memory 导出、codec、进程级 worker、prepare/shutdown | 周期快照、冷启动降级、预算恢复、清理代际和跨平台替换 |
| B5 详情日分片 | storage detail writer/read model、新布局、跨日游标 | 批写无 COUNT/历史清理；跨日排序、迟到写入和连接上限正确 |
| B6 保留协调器 | statistics retention、调度、watermark/manifest、回收任务 | 同一截止线、01:00 补跑、压力判断、读写并发和故障恢复 |
| B7 管理写入与推送 | ConfigStore 扩展、API、OpenAPI/生成类型、WS | 受限写、revision 冲突、生效状态和稳定详情交互 |
| B8 集成与文档收口 | 配置模板、架构/实现文档、Windows WebUI 联调 | 新版重启、配置故障恢复、约 10 客户端和核心命中 2ms |

先建立 typed 契约和测试，再逐段替换生产装配；不同时替换全部 adapter，也不维护已取消的迁移支线。依赖和阶段性提交以开发总计划为准。

## 10. 测试与退出条件

必须覆盖以下针对性场景：

1. 有 ID、未知 ID、无 ID、裸 DoH、TCP/UDP、可信代理 IP；原始值不因异步投影或 reload 改变。
2. 客户端改名、重复 name/ID 拒绝、CIDR 包含/冲突、配置消失/ID 复用；历史不重算，未知 ID 回退时原始值不变。
3. IP 规则修改影响后续策略与缓存隔离，不改已完成匹配/统计；纯名称变更保持答案语义，类型引用同步。
4. 快照关闭/不存在/损坏/版本错误/无权限/磁盘满/中断/恢复超时；DNS 冷启动成功，业务数据不被修改。
5. TTL 与乐观期限跨停机扣减、内存预算缩小、并发淘汰、资源 fingerprint 改变、producer provenance 保留。
6. 周期写入与 clear/reload/shutdown 的交错；旧代际不能复活缓存，同目录临时文件和重启 reset 校验有效。
7. S 小于、等于、大于 T；R 的四个预设和自定义、G=0、UTC 日期边界、截止日等号、配置缩短/延长。
8. 01:00 时区、夏令时、回拨、漏跑、重启补跑；单次冻结 S 和 revision，采样失败不激进删除。
9. 清理与查询 lease、迟到 writer、stats pending 重试交错；不重建退役日期，不破坏 ledger 幂等。
10. 文件删除失败/进程中断后 manifest 恢复；逻辑不可见与物理未释放分别上报，缓存文件不受影响。
11. 详情关闭时统计和保留任务持续工作；队列/磁盘故障显式可观测，不伪造记录或保证无损。
12. 跨日分页、同时间 ID 排序、原始 ID/IP 过滤、匹配 ID 过滤、宽限期查询及大于 31 天的统计窗口。
13. Windows 新目录初始化、新版重启和 journal/manifest 故障恢复；外部改配置不自动生效，支持还原/组合采用和未同步重试。
14. Windows release/预热缓存、约 10 客户端下测 DNS 主链路 2ms，不包括客户端 I/O；后台快照/清理不阻塞查询，写入路径无历史 COUNT/DELETE/VACUUM。Linux 与超高负载不要求另行运行验收。

退出条件为新逻辑正式接线、API/前端对齐、Windows 必需验收完成；实现事实沉淀至 `docs/implementation/`，变更设计同步 `docs/architecture/`，再删除计划/索引。本轮只修订方案，以上不是已通过测试。

## 11. 配套图稿

| 后端契约 | 评审视图 |
| --- | --- |
| 单客户端 ID、可选名称及 IP/CIDR 策略匹配 | [客户端列表](webui-management-designs/webui-clients.svg)、[客户端编辑](webui-management-designs/webui-client-editor.svg) |
| 原始身份与最小历史匹配结果分离 | [解析列表](webui-management-designs/webui-query-records.svg)、[ID 匹配详情](webui-management-designs/webui-query-record-details.svg)、[IP 匹配详情](webui-management-designs/webui-query-record-ip-details.svg) |
| 独立非 SQLite 快照，无持久化配额 | [缓存编辑](webui-management-designs/webui-dns-cache-editor.svg) |
| 统计统一保留与详情开关 | [DNS 配置概览](webui-management-designs/webui-dns-settings.svg)、[统计保留编辑](webui-management-designs/webui-statistics-editor.svg) |
| 统计库与详情日目录只读展示 | [系统配置](webui-management-designs/webui-system-settings.svg) |

[本地评审总览](webui-management-designs/review.html)保留其余模块。图稿是静态演示，不代替类型校验、真实查询、配置同步或运行验收；过期标注以图稿索引的决策修订说明为准。
