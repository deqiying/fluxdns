# FluxDNS 配置字段参考

> 文档状态：有效
>
> 适用范围：本文与当前模板同步，描述配置契约、校验和已实现运行时边界；未定义行为不应视为已支持。
>
> 最后核对：2026-09-05（加载入口、路径、运行支持与缓存容量计费边界；其余字段明细沿用原文 2026-09-04 记录，未逐字段复验）
>
> 核对基线：`8223d819efb83fed642900e6b121825083e8c1dd`
>
> 依据：[config-example.yaml](../../config-example.yaml)
>
> 关联文档：[后端架构](../architecture/backend/overview.md)

## 加载与支持边界

正式入口是 [`app::run_command`](../../backend/src/app.rs) -> [`ConfigLoader::load_from_path`](../../backend/src/config/load.rs) -> [model](../../backend/src/config/model.rs) / [migrate](../../backend/src/config/migrate.rs) / [resolve](../../backend/src/config/resolve.rs) / [validate](../../backend/src/config/validate.rs)。普通 loader 不读取 SecretRef 实际值；run 在 prepare 前执行 accessor 校验，validate 不写 snapshot、不做资源/数据库/网络可用性检查。

| 能力 | 代码实现 | 正式入口接线 | 验证证据 | 已知限制 |
| --- | --- | --- | --- | --- |
| 严格配置与路径 | ConfigLoader、ResolvedConfig、validate | run/validate 共用加载 | 本轮静态追踪，字段表继承来源如头部所示 | 未运行配置矩阵或重新核对每一字段 |
| 资源与缓存 | async PreparedRuntime、PolicyDnsCore | run 加载后 prepare | 本轮核对生产接线 | 能解析不表示网络/文件/SQLite 已成功打开 |
| WebUI | webui model + ManagementService | enable 时创建服务 | 本轮静态 | origin、DB 与 bind 必须可用；默认 binary 可仅提供 API，SPA 需要 embed feature，详见[管理端](backend/management.md) |
| 首用户写回 | ConfigStore + source-preserving editor | setup，run 前恢复 journal | 本轮静态 | loader 为 8 MiB，writer 为 4 MiB；可加载不等于可写回 |
| 热重载 | `process_owned_reload_change` | service-aware watcher | 本轮核对 guard | database、logs、webui enable/address/port/public_origin、dns.resolve_log 改变需重启；users 可动态更新 |

本轮未执行 Cargo 或配置 validate。协议/策略字段定义不自动意味着所有 adapter 组合已验收，真实入口和未支持项见[后端实现](backend/README.md)。规范中的目标要求与代码冲突时，按[差距计划](../plans/backend-contract-gaps.md)处理，不仅修改字段说明来掩盖实现缺口。

## 1. 配置模型概览

FluxDNS 的配置由命名资源和引用关系组成。请求入口选择策略，策略按顺序匹配规则，再决定使用本地 hosts 或远程上游：

```text
listener
  ├─ udp/tcp → strategy
  └─ doh → routes → strategy
                         ├─ hosts：本地回答
                         └─ rule_set → upstream / group → outbound
```

- `listener` 定义请求入口。一个普通 listener 可以展开为多个 IPv4/IPv6 socket。
- 一个 `type: doh` listener 是一个逻辑 DoH 服务，可通过多个 `endpoints` 同时提供不同绑定方式。
- `routes` 在同一个 DoH listener 内共享；每条路由选择一个策略。
- `clients` 按客户端标识或 IP 网段选择策略，并覆盖缓存、TTL 和 ECS 等设置。
- `upstreams`、`hosts`、`rule_set` 和 `outbound` 是可复用的命名资源。
- `webui` 定义独立的 WebUI Management Server、浏览器 origin 和登录用户。

## 2. 通用约定

### 2.1 版本

顶层 `version` 是配置 schema 版本。当前模板为 `version: 1`；字段重命名、删除和迁移规则以该字段为边界。加载器应拒绝不支持的版本，而不是静默按旧字段解释。

### 2.2 路径和工作目录

配置路径采用两级基准解析，不能直接把所有相对路径都拼到进程当前工作目录：

1. 先确定启动配置文件的绝对路径。命令行传入的配置文件路径若为相对路径，仅在这一步相对于进程启动时的当前工作目录解析；其父目录记为 `config_dir`。
2. 解析 `work.path`：绝对值直接使用；相对值按 `config_dir.join(work.path)` 解析。结果经词法归一化后形成绝对的 `resolved_work_path`。
3. 解析项目路径字段：绝对值直接使用；相对值统一按 `resolved_work_path.join(field_path)` 解析并做词法归一化，不能再回到 `config_dir` 或进程当前工作目录。
4. `work.rules_path`、`database.path`、`logs.path`、`dns.cache.persistence.path`、TLS 证书和私钥、SecretRef 文件、本地 hosts 与规则文件路径都遵循第 3 步。

词法归一化应消除 `.` 和可消解的 `..`，但不依赖目标文件或目录已经存在。通过 bytes/string 加载且没有配置文件来源路径时，不存在 `config_dir`；此时相对 `work.path` 必须报错，调用方需要提供来源路径或改用绝对 `work.path`，不得隐式回退到进程当前工作目录。

| 启动配置文件 | 配置值 | 解析结果 |
| --- | --- | --- |
| `/opt/_fluxdns/config.yaml` | `work.path: ./` | `resolved_work_path = /opt/_fluxdns` |
| `/opt/_fluxdns/config.yaml` | `work.path: ./`、`database.path: ./data/fluxdns.sqlite3` | `database.path = /opt/_fluxdns/data/fluxdns.sqlite3` |
| `/etc/fluxdns/bootstrap/config.yaml` | `work.path: ../runtime`、`logs.path: ./logs/fluxdns.log` | `logs.path = /etc/fluxdns/runtime/logs/fluxdns.log` |
| `/etc/fluxdns/config.yaml` | `work.path: /var/lib/fluxdns`、`database.path: /srv/fluxdns.sqlite3` | 两个绝对路径均不与其他基准拼接 |

启动时若配置文件所在目录不是 `resolved_work_path`，程序将配置复制到 `<resolved_work_path>/config.yaml`，固定文件名为 `config.yaml`。该文件是工作目录中的配置快照；启动流程负责创建工作目录和所需父目录。

配置副本按 [Config 模块方案](../architecture/backend/modules/config.md) 原子创建：目标不存在时在同目录写临时文件并以 no-replace 方式发布，内容相同则不操作，目标已存在且内容不同时拒绝自动覆盖。SecretRef 解析值不会写回配置副本。

### 2.3 命名与引用

`listener`、`upstreams`、`strategy`、`hosts`、`outbound` 和 `rule_set` 中的 `name` 是资源标识，同一集合内必须唯一。引用字段必须指向对应集合中存在且类型正确的名称。

| 引用字段 | 目标集合或语法 |
| --- | --- |
| `listener[].strategy`、`clients[].strategy`、`listener[type=doh].routes[].strategy` | `strategy[].name` |
| `listener[].hosts`、`strategy[].rules[].hosts` | `hosts[].name` |
| `strategy[].rules[].rule_set` | `rule_set[].name`，或 `geosite:cn` 这类规则集子选择器 |
| `strategy[].rules[].upstream`、`strategy[].default_upstream` | `upstreams[].name` |
| `upstreams[].bootstrap` | `upstreams[].name` |
| `upstreams[type=group].upstreams[].name`、`fallbacks[].name` | `upstreams[].name` |
| `upstreams[].proxy`、`rule_set[].proxy` | `outbound[].name` |

应在加载阶段检查名称唯一性、引用存在性、引用类型和循环引用。

### 2.4 值类型

| 数据类型 | 模板示例 | 说明 |
| --- | --- | --- |
| 时长 | `10s`、`24h`、`1d` | 用于缓存、超时和定时更新；解析为 duration，而不是整数天数或秒数。 |
| 字节数 | `8388608` | `max_size_bytes` 使用整数，单位为字节。 |
| IP/CIDR | `192.168.1.0/24`、`fe80::/10` | 用于监听绑定、客户端匹配、ECS 和可信代理。 |
| URL | `https://dns.google/dns-query` | 必须按字段要求使用合法 URL。 |
| SecretRef | `{env: FLUXDNS_OUTBOUND_SG_URL}` | 从环境变量或文件取得完整敏感值，不能把真实凭据写入模板。 |

`outbound[].proxy_url` 是 SecretRef 对象，不是明文 URL 字符串。当前契约要求 `env` 与 `file` 二选一；读取结果应为完整代理 URL。

### 2.5 覆盖和继承

配置块未出现时继承较低优先级的设置；块中只出现的字段按字段继承。显式的禁用值用于停止继承。

#### ECS 优先级

当多个层级提供 `edns_client_subnet` 时，优先级由高到低为：

```text
strategy.rules[].edns_client_subnet
  > strategy[].edns_client_subnet
  > clients[].edns_client_subnet
  > upstreams[].edns_client_subnet
  > dns.edns_client_subnet
```

`mode` 的取值为 `disabled`、`client`、`custom`：

- `disabled` 显式停止当前请求继续向上游传递 ECS；
- `client` 优先使用并规范化请求携带的合法 ECS；没有时按客户端地址推导 IPv4 `/24` 或 IPv6 `/56` 前缀，避免向上游传递完整客户端地址；
- `custom` 使用 `custom_ip`，因此 `custom_ip` 必填。

当策略目标是 group 时，rule/strategy/client 的显式 ECS 继续覆盖所有成员；否则每个 direct member 使用自身 `upstreams[].edns_client_subnet`，再回退到全局 ECS。成员在 cache lookup 后才由 group 选择，因此存在显式 member ECS 的 group 当前绕过内部缓存，避免不同成员的 ECS 响应共用同一 group key。

#### 缓存和 TTL

缓存和 TTL 覆写是两个独立配置：

```text
clients[].cache > strategy[].cache > dns.cache
clients[].ttl_override > strategy[].ttl_override > dns.ttl_override
```

`ttl_override.enabled: false` 显式关闭当前层级的 TTL 覆写；它不等价于关闭缓存。`ttl_override.min` 或 `max` 为 `0s` 表示该边界不设限。

缓存不是逐层查询的 fallback 链。每个请求只选择一个逻辑缓存池：客户端显式启用时选择“实际客户端身份 + 生效策略”池；否则策略显式启用时选择策略池；策略未配置 `cache` 时才选择全局池。策略或客户端显式 `enabled: false` 会终止选择，不再回退到全局池。详见 [`dns.cache`](#81-dnscache)。

### 2.6 配置迁移与归一化

配置加载不直接把 YAML DTO 当作运行时配置，而是经过显式版本迁移和归一化：

```text
RawConfigVn
  → migrate 到当前 schema
  → normalize 路径、SecretRef、默认值和继承
  → semantic validate
  → ResolvedConfig
```

- 只支持从已知旧版本向当前版本逐级迁移；未来版本直接拒绝，不能按旧字段猜测。
- 迁移必须是可测试、可重放的纯转换，保留缺失、`null`、空数组和空对象的区别；数组替换/合并、cache/TTL/ECS 继承和来源信息在 `ResolvedConfig` 阶段一次性确定。任何有损删除都必须产生 warning 并显式确认，不能静默丢字段。
- 原始配置文件不被自动覆盖。实现 `validate`/`migrate`/`print-normalized`/`diff`/`rollback` 命令时，输出应写到新文件或显式指定的目标，并保存输入/输出 hash、step IDs 和变更摘要。
- 配置 schema 版本、SQLite schema 版本、资源 parser/compiler 版本和 cache key format 版本彼此独立；升级一个版本不能隐式宣称其他版本兼容。
- 运行时升级沿用 `prepare candidate → preflight → atomic activate/keep old`：可热更新项替换 `RuntimeSnapshot`，需要重新绑定的项 drain 后切换，无法安全切换的项拒绝候选并保留旧运行时。

## 3. 顶层字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `version` | integer | 配置 schema 版本；当前必须为 `1`。 |
| `work` | object | 工作目录和规则资源落盘目录。 |
| `database` | object | 默认开启的聚合统计、可选解析详情和其他持久化能力使用的数据库；始终必填。 |
| `logs` | object | 服务日志输出。 |
| `webui` | object | WebUI Management Server、浏览器 origin 和登录用户。 |
| `dns` | object | 全局缓存、TTL、ECS 和解析日志默认值。 |
| `listener` | array | UDP、TCP 和 DoH 请求入口。 |
| `upstreams` | array | hosts 上游、DoH 上游和上游组。 |
| `strategy` | array | 策略和有序路由规则。 |
| `hosts` | array | 可被入口或策略复用的本地 hosts 资源。 |
| `outbound` | array | 远程上游或规则下载使用的代理出口。 |
| `rule_set` | array | 可被策略引用的规则资源。 |
| `clients` | array | 客户端匹配和客户端级覆盖。 |

## 4. `work`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `work.path` | string | 必填 | 工作目录；绝对路径直接使用，相对路径以启动配置文件所在目录为基准。 |
| `work.rules_path` | string | 必填 | 规则资源落盘目录；相对路径以 `work.path` 为基准。 |

启动时，应先得到绝对的 `resolved_work_path` 并确保目录存在；如果配置文件不在该目录中，再将其复制为 `<resolved_work_path>/config.yaml`。数据库、日志、缓存和证书等其他相对路径均以 `resolved_work_path` 为基准。

## 5. `database`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `database.type` | string | 必填 | 当前模板仅定义 `sqlite`。未知类型应拒绝。聚合统计默认开启，因此不能省略。 |
| `database.path` | string | 必填 | SQLite 数据库文件路径；相对路径以 `work.path` 为基准，父目录由程序创建。聚合统计和（启用时的）详情日志共用该数据库。 |

数据库在 prepare 阶段必须完成打开、schema migration 和基本写入检查；失败时拒绝启动。运行中数据库暂时不可写时，DNS 继续服务，由统计 writer 保留进程内补偿计数并重试；未恢复前进程退出可能造成 persistence gap，该状态必须可观测。`database.path` 表示文件，不表示目录。

## 6. `logs`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `logs.enable` | boolean | 必填 | 是否启用服务日志。 |
| `logs.level` | string | 必填 | 日志级别；v1 接受 `trace`、`debug`、`info`、`warn`、`error`，大小写归一化，未知值拒绝。 |
| `logs.path` | string | 必填 | 日志文件路径；相对路径以 `work.path` 为基准。 |

## 7. `webui`

`webui` 控制独立于 DoH 的 Management Server。FluxDNS 在该端口只提供 HTTP，不实现 TLS 终止，也不根据 `X-Forwarded-*` 推断浏览器 origin；公网部署应将监听限制在 loopback 或受保护的内网地址，并由 Nginx 等可信反向代理提供外部 HTTPS。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `webui.enable` | boolean | 必填 | `true` 启动 Management Server；`false` 不创建 management listener 或认证状态。 |
| `webui.address` | string | 必填 | HTTP Management Server 的监听地址；与 DNS TCP/DoH endpoint 统一检查端口冲突。 |
| `webui.port` | integer | 必填 | HTTP Management Server 的监听端口，范围为 `1..=65535`。 |
| `webui.public_origin` | absolute URL | `enable: true` 时必填 | 浏览器实际访问的唯一 `http` 或 `https` origin；不得包含凭据、path、query 或 fragment，用于严格 Origin 校验和 Cookie 策略。 |
| `webui.users` | array | 可选 | 省略或 `[]` 表示需要一次性初始化；非空表示已可登录；显式 `null` 拒绝。 |
| `webui.users[].name` | string | 必填 | 用户名；列表内必须唯一。 |
| `webui.users[].password_hash` | string | 必填 | 初始化时由一次性明文密码生成的单向 hash。 |

配置仍拒绝未知字段、重复用户名、非法 hash 和明文 `webui.users[].password`。首次初始化只把 Argon2id PHC hash 写入源配置和工作目录快照；真实密码和 hash 不应提交到公开仓库。

`public_origin` 使用 HTTP 时，session Cookie 不设置 `Secure`，因此凭据与 session 的传输安全完全依赖管理网络；这只适合 loopback 或明确受信任的隔离网络。常规部署应由反向代理提供 HTTPS，并把 `public_origin` 配置成浏览器看到的 `https://...` origin，此时后端仍只监听 HTTP。

## 8. `dns`

### 8.1 `dns.cache`

`cache` 描述全局池开关、共享内存容量、短期失败 TTL、乐观缓存和持久化；TTL 覆写不再嵌套在其中。

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `dns.cache.enabled` | boolean | 是否启用全局缓存池；它不是策略池和客户端池的总开关。 |
| `dns.cache.memory.max_size_bytes` | integer | 所有逻辑缓存池共享的内存计费容量上限，单位为字节。 |
| `dns.cache.failure_ttl` | duration | `SERVFAIL`、上游截断响应等无自然可用 TTL 的短期缓存时间；必须在 `1s..=5m`。 |
| `dns.cache.optimistic.enabled` | boolean | 是否允许返回已过期记录并在后台刷新。 |
| `dns.cache.optimistic.answer_ttl` | duration | 乐观缓存应答使用的 TTL。 |
| `dns.cache.optimistic.max_age` | duration | 记录过期后仍可乐观返回的最长时间。 |
| `dns.cache.persistence.path` | string | 持久化缓存文件路径；相对路径以 `work.path` 为基准。 |
| `dns.cache.persistence.max_size_bytes` | integer | 当前实现为持久化编码快照的容量预算，单位为字节；不是 SQLite 文件物理硬上限。 |

`memory.max_size_bytes` 是缓存条目按 key、DNS wire 和元数据计算后的容量预算，不承诺等于进程 RSS。`persistence.max_size_bytes` 保留 [`prepare_snapshot`](../../backend/src/cache/persistence.rs) 的计费语义：10 字节快照头，加每条 payload 长度及 4 字节 framing；[`SQLite adapter`](../../backend/src/cache/sqlite.rs) 使用增量 upsert 与编码大小索引维护同一预算，没有据此设置 `max_page_count` 或物理文件收缩。主库页/freelist/索引及 `-wal`/`-shm` 均可能使实际磁盘占用超过这个值，不能据此规划硬磁盘配额。

任一逻辑缓存池启用时，production async prepare 从独立 SQLite 恢复可用记录；内存 CAS 成功后通过有界队列 best-effort 持久化，有序 shutdown 排空已入队批次。超出编码预算时按 entry 插入时间淘汰旧项，不按访问热度；队列满或持久化失败不改变 DNS 响应。原物理容量与近似 LRU 要求见[差距计划](../plans/backend-contract-gaps.md)，本次只校正文档，没有更改字段或代码。

策略级和客户端级 `cache` 只允许 `enabled` 与 `optimistic` 子对象，不包含 `memory`、`failure_ttl` 或 `persistence`。只要出现策略级或客户端级 `cache` 对象，`enabled` 就必须显式提供；整个对象缺失才表示继续向较低优先级选择。

#### 逻辑缓存池选择

v1 只有三类逻辑缓存池，并共享 `dns.cache.memory` 与 `dns.cache.persistence` 的容量预算和存储后端：

1. 全局池：namespace 为 `global`；
2. 策略池：namespace 包含生效的 `strategy.name`；
3. 客户端 + 策略池：namespace 包含实际客户端身份和生效的 `strategy.name`。客户端身份优先使用请求的精确 `client_id`，没有时使用规范化源 IP；不能只使用匹配规则的 `clients[].name`。

每个请求按以下顺序选择且只使用一个池，cache miss 时不跨池继续查找：

```text
clients[].cache.enabled == false  → 不使用缓存
clients[].cache.enabled == true   → 客户端 + 策略池
clients[].cache 整块缺失          → 继续检查策略

strategy[].cache.enabled == false → 不使用缓存
strategy[].cache.enabled == true  → 策略池
strategy[].cache 整块缺失          → dns.cache.enabled 为 true 时使用全局池，否则不缓存
```

cache key format v2 显式区分逐规则匹配前的 `Fast` 模式和完整决策后的 `Resolved` 模式，二者不能 alias。除 pool namespace 与 canonical query 外，key 包含 transport compatibility，以及 opaque 的 policy/request/target/ECS fingerprint；不包含 DNS ID、原始 client 地址、整个配置 revision 或资源 generation。policy fingerprint 只覆盖会改变答案的已解析配置、相关 hosts/rule 内容 hash 和选择安全性，不包含 `logs`、`webui`、`database` 等纯观测/管理配置。请求 fingerprint 对有效 ECS 使用规范化网段；没有 ECS 时只使用脱敏的客户端 `/24`（IPv4）或 `/56`（IPv6）网段。group member ECS 无法在成员选择前安全确定，因此该路径不使用 fast key，并继续绕过 response cache。

规则或 hosts 的变化不会扫描或清空缓存，但会改变 policy fingerprint，使后续请求不再命中旧语义 entry；旧 entry 继续占用容量并按自身 TTL/optimistic 生命周期淘汰。请求先按最新 runtime snapshot 计算客户端、生效策略、ECS、namespace 和 policy/request fingerprint，fast miss 后才执行 rules、hosts 和 upstream target 决策。`hosts[]` 的 listener/strategy 本地回答直接使用当前资源 snapshot 并绕过 response cache；`upstreams[type=hosts]` 属于 upstream connector，其结果按普通上游响应处理。启用 optimistic cache 时，后台刷新重新读取当前 Policy/资源 snapshot 并完整执行 route 与上游选择，不能沿用旧 entry 的目标。后续 WebUI 的清除缓存功能属于显式 `namespace/key/predicate` 操作，不由普通资源刷新隐式触发。

policy fingerprint 只保证实现纳入语义摘要的相关变化切换 key；它不是通用配置 revision，也不替代显式清理接口。旧格式 key 不能与 v2 恢复记录混用，持久化 adapter 将其按不兼容记录隔离。

#### 响应缓存语义

- `hosts[]` 产生的本地回答不进入 response cache；它直接读取已编译内存 snapshot，并仍应用 TTL override 和统计。`upstreams[type=hosts]` 不属于此例外。
- 正常 `NOERROR` 响应按 RR TTL 缓存；`NOERROR/NODATA` 和 `NXDOMAIN` 按负缓存 TTL 缓存，优先使用 SOA TTL 与 SOA.MINIMUM 的较小值。为满足 v1“所有 NODATA/NXDOMAIN 均缓存”的产品语义，没有可用 SOA/负 TTL 时使用 `failure_ttl`；这是对 RFC 2308 “SHOULD NOT cache”建议的有意偏离。
- `SERVFAIL` 和直接从上游收到的 `TC=1` 响应使用 `failure_ttl`；`REFUSED` 可作为上游组的终态响应返回，但 v1 不写缓存。上游 TC 条目额外包含当前入口 transport，只允许相同 transport 命中，不能把 UDP 场景的截断结果复用于 TCP/DoH。
- malformed DNS、问题段不匹配、连接/TLS/HTTP 失败和超时不属于 DNS 响应，不写缓存。
- 缓存保存不含客户端 DNS ID 和传输 envelope 的 canonical response。若本地 UDP 输出因本次客户端 advertised size 而截断，应保存完整 canonical response，并在每次发送时重新编码；只有上游本身返回的 `TC=1` 才保存截断条目。
- 写入按响应质量做 compare-and-replace：完整 `NOERROR/TC=0` 可以提升并替换未过期的 NXDOMAIN/SERVFAIL/TC 条目，SERVFAIL/TC 不能覆盖未过期的完整回答；同质量条目在过期前不因后到竞态反复覆盖。
- optimistic/stale 只适用于已经按上述规则准入的条目；缓存返回时按剩余 TTL 和当前请求重新生成响应。
- 同一 key 的并发 miss/optimistic refresh 通过 single-flight 合并。leader 得到可缓存结果后先形成持有 lease 的 `CacheCommitCandidate` 并返回共享响应；后台 worker 使用独立 100ms deadline 完成 admission/CAS/persistence enqueue 并唤醒 waiter。客户端响应不等待 commit；candidate 被队列丢弃、取消或直接 drop 时，RAII lease 必须发布失败终态，不能永久挂住 follower。

### 8.2 `dns.ttl_override`

`ttl_override` 与 `cache` 平级，用于限制返回给客户端的 DNS TTL：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `dns.ttl_override.enabled` | boolean | 是否启用当前层级的 TTL 覆写。 |
| `dns.ttl_override.min` | duration | 最小 TTL；`0s` 表示不设下限。 |
| `dns.ttl_override.max` | duration | 最大 TTL；`0s` 表示不设上限。 |

策略和客户端可使用同样的字段结构。省略字段按层级继承，显式 `enabled: false` 停止当前层级的 TTL 覆写。

### 8.3 `dns.edns_client_subnet`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `mode` | enum | 必填 | `disabled`、`client` 或 `custom`。 |
| `custom_ip` | CIDR | `mode: custom` 时必填 | 要发送给上游的 ECS 网段。 |

全局 ECS 设置可由上游、客户端、策略和规则覆盖，详见[覆盖和继承](#25-覆盖和继承)。

### 8.4 `dns.resolve_log`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enable` | boolean | 是否记录每次解析请求的详情（请求、策略、规则、ECS、缓存、上游结果和耗时等）。关闭时不写详情表，但不关闭聚合统计。 |
| `eviction_threshold_records` | integer | 详细记录达到该软阈值后开始后台淘汰。 |
| `max_records` | integer | 详细记录的硬上限；不能因并发写入而突破。 |
| `max_record_age` | duration | 记录最长保留时间，例如 `7d`。 |

必须满足 `0 < eviction_threshold_records < max_records`。这两个字段都按详细记录条数计数，不是 SQLite 文件字节上限。请求任务只向统一 resolution ingress 附带 typed question 和共享 response，qname digest、canonical qname 与 answer JSON 在后台 detail projector 中生成，再进入 SQLite adapter 唯一的有界详情 channel；满批立即提交，低流量尾批最多等待 5 秒。达到软阈值后先删除超过 `max_record_age` 的记录，再按时间删除最旧记录，直到回到软阈值以下。若 projection/SQLite 队列已满、数据库忙或提交会突破硬上限，则丢弃新的详细记录，DNS 请求不得等待或失败。

详情中的 `duration_ms` 从 transport 接入计时点计到 DNS core 完成，`dns_core_duration_ms` 只计算 `DnsCore::resolve_with_completion` 主链；两者均在主链返回时冻结，不包含后台观测排队和 SQLite 写入。DoH 的 `duration_ms` 包含入站 TLS/HTTP 处理，但不包含响应编码和网络写回。schema v5 之前的历史记录无法回填主链耗时，Management API 返回 `null`。

聚合统计默认开启且始终依赖 `database`：至少按 UTC 自然日记录总请求数，并按有界的 client bucket、transport class、strategy、source/upstream、RCODE 和 cache lookup status 记录分项计数。未匹配客户端统一进入 `unknown` bucket；不能使用域名、完整客户端 ID 或原始 IP 作为无界维度。请求任务只做一次有界 `ResolutionEnvelope::try_publish`，后台 dispatcher 更新进程内 sharded counters，stats writer 周期性以带 `batch_id` 的 checkpoint 批量 upsert 到 SQLite，并用 batch ledger 幂等去重。detail 下游溢出不影响聚合统计；但统一 ingress 溢出会同时丢失该事件的 stats/detail/cache commit，因此必须累计 `dropped` 并冻结首次 `gap_started_at_utc_millis`。一次请求只计一次 `total_requests`；cache/hosts 命中归入本地 source，parallel 的多个上游尝试不重复计请求。统计不能因为详情记录被丢弃或 `enable: false` 而停止。数据库运行中暂时不可写时继续维护进程内计数并报告 `degraded`/persistence gap；启动阶段数据库不可用则拒绝启动。

“依赖数据库”表示统计/详情的权威持久化后端是 `database`；请求线程不同步等待数据库。Management overview 的 `resolution_pipeline` 暴露 ingress accepted/dropped/首次 gap 时间、cache commit 各终态以及 detail accepted/dropped/failed。`resolve_log` 的详情在当前有界 `max_records` 契约下允许 best-effort 丢弃并计数，若未来需要无损审计需另行定义 spool、背压和磁盘配额。

## 9. `listener[]`

一个 listener 是一个逻辑服务。`addresses` 中的每个地址由运行时展开为独立 socket；同一个逻辑 listener 的地址共享协议和端口。

### 9.1 UDP/TCP listener

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 入口唯一名称。 |
| `type` | enum | 必填 | `udp` 或 `tcp`。 |
| `addresses` | array[string] | 必填 | IPv4/IPv6 绑定地址列表，例如 `[0.0.0.0, "::"]`。 |
| `port` | integer | 必填 | 所有地址共用的监听端口。 |
| `strategy` | string | 必填 | 默认策略，引用 `strategy[].name`。 |
| `hosts` | string | TCP 模板示例中可选 | 入口级本地 hosts 资源；命中时本地回答，未命中再使用 `strategy`。 |

通过把 IPv4 和 IPv6 地址放入同一个 `addresses` 数组，可以用一个逻辑 listener 表达双栈入口。实现必须明确 IPv6 socket 的 v6-only 行为，避免 `0.0.0.0` 与 `::` 展开后的绑定重叠。

### 9.2 DoH listener

DoH 不使用普通 listener 的顶层 `address`、`port`、`strategy` 或单个 `path` 字段，而是由共享路由和 endpoint 组成：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | 逻辑 DoH listener 名称，必须唯一。 |
| `type` | enum | 固定为 `doh`。 |
| `routes` | array | 所有 endpoint 共享的 HTTP 路由列表。 |
| `endpoints` | array | 一个或多个绑定端点；每个 endpoint 独立处理 TLS 和客户端 IP 来源。 |

#### `listener[type=doh].routes[]`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `path` | string | HTTP 路径，可使用一次完整路径段形式的 `{client_id}` 占位符。 |
| `strategy` | string | 路由命中的策略，引用 `strategy[].name`。 |

模板示例使用 `/dns/inner/{client_id}` 和 `/dns/outside/{client_id}`。当 `{client_id}` 位于模板末尾时，单条模板同时匹配裸路径 `/dns/inner` 和带一个非空路径段的 `/dns/inner/<client_id>`；裸路径不产生 client ID，`/dns/inner/` 和额外多段路径不匹配。占位符位于中间时仍必须提供对应的非空路径段。不同 route 不能命中同一个实际 HTTP path，例如 `/dns/inner`、`/dns/inner/client-a` 都会与 `/dns/inner/{client_id}` 产生语义重叠并在配置加载时拒绝。

Transport 只对实际 HTTP path 匹配一次，并把配置模板作为稳定 route ID 传给 Policy；route ID 不包含真实 client ID。`/dns-quer/inner*` 与 `/dns-quer/outside*` 是服务保留路径，不能作为自定义路由；它们与示例中的 `/dns/...` 前缀不是同一组路径。

每条路由固定同时支持 DoH GET 和 POST，不增加方法开关：

- GET 使用唯一的 `dns` query 参数，值为不带 `=` padding 的 base64url DNS wire；
- POST body 是原始 DNS wire，`Content-Type` 必须为 `application/dns-message`；
- 解码后的 GET 消息与 POST body 最大均为 65,535 字节；对应的无 padding GET `dns` 参数最多 87,380 个字符，HTTP request-target 限制不得小于路由路径加该参数所需长度；超限分别由 HTTP 层返回适当的 `413`/`414`，不进入 DNS 核心；
- 其他方法返回 `405` 并包含 `Allow: GET, POST`；不支持的媒体类型返回 `415`，非法 wire 返回 `400`；
- 通过 DNS 协议校验后的 `NXDOMAIN`、`REFUSED`、`SERVFAIL` 等响应仍使用 HTTP `2xx`，失败状态由 DNS RCODE 表达；
- 响应使用 `application/dns-message`。由于答案可能随客户端身份、策略和 ECS 变化，v1 固定发送 `Cache-Control: no-store`，只使用 FluxDNS 内部缓存。

#### `listener[type=doh].endpoints[]`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | endpoint 名称，在同一 DoH listener 内唯一。 |
| `addresses` | array[string] | 必填 | endpoint 的 IPv4/IPv6 绑定地址列表。 |
| `port` | integer | 必填 | endpoint 监听端口。 |
| `tls` | object | 必填 | 该 endpoint 的 TLS 所有权。 |
| `client_ip` | object | 必填 | 请求客户端 IP 的来源和可信边界。 |

同一个逻辑 DoH listener 可以同时包含以下两类 endpoint，而不需要创建两个顶层 DoH 配置：

- `tls.mode: terminate`：FluxDNS 自己完成 TLS 握手和解密，必须提供证书链和私钥文件；
- `tls.mode: external`：Nginx 等反向代理已经终止 TLS，FluxDNS 接收内部 HTTP，证书字段禁止出现。

#### `endpoints[].tls`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `mode` | enum | 必填 | `terminate` 或 `external`。 |
| `certificate_file` | string | `terminate` 时必填 | 证书链文件路径，相对路径以 `work.path` 为基准。 |
| `private_key_file` | string | `terminate` 时必填 | 私钥文件路径，相对路径以 `work.path` 为基准。 |

`external` 模式不得配置 `certificate_file` 或 `private_key_file`。反向代理占用公网端口时，external endpoint 通常绑定本机或内网端口，例如模板中的 `127.0.0.1:8053`。

#### `endpoints[].client_ip`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `source` | enum | 必填 | `peer`、`forwarded_header` 或 `proxy_protocol`。 |
| `header` | string | `forwarded_header` 时必填 | 允许 `X-Forwarded-For`、`X-Real-IP` 或 `Forwarded`。 |
| `trusted_proxies` | array[CIDR] | `forwarded_header`/`proxy_protocol` 时必填 | 可信的反代对端网段，不是最终客户端网段。 |
| `on_missing` | enum | `forwarded_header` 时可选 | Header 缺失时 `reject` 或 `use_peer`。 |
| `on_invalid` | enum | `forwarded_header` 时可选 | Header 非法时 `reject` 或 `use_peer`。 |

`peer` 直接使用 TCP 对端地址。`forwarded_header` 只接受来自 `trusted_proxies` 的请求；转发链应从右到左解析，并且反向代理必须先清理客户端自带的伪造 Header。

`proxy_protocol` 的自动识别只用于区分 PROXY v1 文本头和 v2 二进制头，不表示 header 可选。运行时先按 TCP peer 地址检查 `trusted_proxies`，再在 TLS handshake/HTTP 解析前读取前导头；不可信 peer、缺失头、非法长度、未知协议版本或不能提供 TCP4/TCP6 源地址的头均拒绝连接，不回退为 `peer`。v1 必须在前 107 字节内遇到 CRLF；v2 总前导长度上限固定为 536 字节。v2 中长度合法但未知的可选 TLV 可忽略，以兼容扩展；同一个 endpoint 不同时接收裸连接和 PROXY 连接，需要时应配置两个 endpoint。

### 9.3 绑定校验

`listener[].addresses`、`listener[type=doh].endpoints[].addresses` 与启用后的 `webui.address`/`port` 展开后不得产生相同协议、地址、端口的冲突；WebUI 只占用 TCP，不与同地址、同端口的 UDP listener 冲突。IPv4/IPv6 双栈是否共享 socket、以及 v6-only 行为必须在运行时明确。

当前模板没有 `dot` 或 `doq` listener 字段；DoT/DoQ 应在协议、TLS/QUIC 材料和握手校验契约确定后再加入。

## 10. `upstreams[]`

`upstreams` 中每个资源由 `type` 决定允许的字段集合。未知字段和不属于该类型的字段都应拒绝。

### 10.1 公共字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `name` | string | 上游唯一名称。 |
| `type` | enum | `hosts`、`doh` 或 `group`。 |

### 10.2 `type: hosts`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `format` | string | 必填 | 内容格式；按该格式严格解析。模板示例为 `json`。 |
| `hosts` | string | 必填 | 内联 hosts 数据。 |

该类型可作为其他 DoH 上游的 `bootstrap`，用于避免首次解析依赖系统默认 DNS。内联内容格式错误应报告字段路径。

### 10.3 `type: doh`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `address` | URL | 必填 | DoH 服务 URL。 |
| `bootstrap` | string | 可选 | 引导解析 `address` 主机名的上游名称。 |
| `connect_ip` | IP | 可选 | 实际连接目标 IP；不改变 HTTP `Host` 或 TLS SNI。 |
| `proxy` | string | 可选 | 代理出口名称，引用 `outbound[].name`。 |
| `edns_client_subnet` | object | 可选 | 上游级 ECS 覆盖。 |

`bootstrap` 和 `connect_ip` 解决的是连接建立路径：前者引用上游完成域名解析，后者直接指定连接 IP。二者互斥，均不改变远程服务的 HTTP `Host` 或 TLS SNI。

正式 bootstrap resolver 按 connector 缓存地址，TTL 下限为 0、上限为 3,600 秒；零 TTL 不复用，重新 prepare 不继承旧缓存。没有新增配置字段；并发、过期和失败边界见 [Upstream 设计](../architecture/backend/modules/upstream.md)。

未配置代理时，优先使用 `connect_ip`，其次使用 `bootstrap`，两者都没有时使用系统解析器。配置 `proxy` 时，先解析对应 SecretRef 的 URL scheme：

- `socks5://`：主机名在 FluxDNS 本地解析；顺序仍为 `connect_ip` → `bootstrap` → 系统解析器，再把 IP 交给代理；
- `socks5h://`：`connect_ip` 缺失时把原始主机名交给代理解析，因此禁止同时配置 `bootstrap`；若显式提供 `connect_ip`，代理接收该 IP，HTTP `Host`/TLS SNI 仍使用 URL 主机名。

### 10.4 `type: group`

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `upstreams` | array[object] | 必填 | 主成员列表；每项的 `name` 必填，`weight` 可选。 |
| `upstream_mode` | enum | 必填 | `parallel`、`round-robin`、`load-balance` 或 `failover`。 |
| `timeout` | duration | 必填 | 主组总超时时间。 |
| `fallbacks` | array[object] | 可选 | 主组在 timeout 内没有终态 DNS 响应时使用的回退成员，字段约束与主成员相同。 |
| `fallback_upstream_mode` | enum | 有 `fallbacks` 时必填 | 回退成员选择模式。 |
| `fallback_timeout` | duration | 有 `fallbacks` 时必填 | 回退组总超时时间。 |
| `upstreams[].name` / `fallbacks[].name` | string | 必填 | 成员上游名称。 |
| `upstreams[].weight` / `fallbacks[].weight` | positive integer | 可选，默认 `1` | 正整数权重；省略后在 DTO 边界归一化为 `1`。 |

组成员使用对象而不是 `name:weight` 字符串，避免字符串解析歧义。精确算法见 [Upstream 模块方案](../architecture/backend/modules/upstream.md)：`round-robin` 使用 smooth weighted round-robin，`load-balance` 使用按 weight 归一化的 least-in-flight，`failover` 严格按配置顺序且 weight 只能省略或显式设为 `1`。三种模式都只在 transport failure 时尝试其他成员，任意终态 DNS 响应都会结束当前组；主组完全没有终态响应时才进入 fallback。

`parallel` 的 v1 语义固定如下：

1. 同时向主组成员发起请求。第一个通过 DNS wire 校验、响应标志校验且问题段与请求匹配的 DNS 响应立即返回客户端；`NXDOMAIN`、`REFUSED`、`SERVFAIL` 和 `TC=1` 都属于终态成功。超时、连接/TLS/HTTP 失败、非法 wire 和问题段不匹配不属于成功。
2. 若首个终态响应是完整的 `NOERROR/TC=0`，按缓存准入规则写入并取消其余请求；若首个响应是 `NXDOMAIN`、`REFUSED`、`SERVFAIL` 或 `TC=1`，已经发出的其他主组请求继续到各自完成或组 `timeout`，但不改变已经返回给当前客户端的响应，也不触发 fallback。
3. 对上一条继续运行的后台窗口，收集到全部成员完成或 `timeout` 后再定稿缓存：先从完整的 `NOERROR/TC=0` 响应中按成员配置顺序选择；若没有，再从允许缓存的其他终态响应中按成员配置顺序选择。`REFUSED` 不缓存。这样迟到有效答案仍能写缓存，但缓存结果不由并发完成顺序决定。
4. 主组只有在 `timeout` 内完全没有终态 DNS 响应时才进入 `fallbacks`；fallback 组使用同样的响应和缓存语义。
5. `parallel` 不使用权重；该模式下成员应省略 `weight` 或显式设为 `1`，非 `1` 值会被拒绝，避免配置暗示不存在的优先级。

## 11. `strategy[]`

策略定义规则顺序、默认上游以及策略级覆盖。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 策略唯一名称。 |
| `rules` | array | 必填 | 有序规则列表；从上到下匹配，命中第一条后停止。 |
| `default_upstream` | string | 必填 | 没有规则命中时使用的上游名称。 |
| `cache` | object | 可选 | 策略级缓存覆盖，高于 `dns.cache`。 |
| `ttl_override` | object | 可选 | 策略级 TTL 覆写，与 `cache` 平级。 |
| `edns_client_subnet` | object | 可选 | 策略级 ECS 覆盖。 |

### 11.1 `strategy[].rules[]`

每条规则必须且只能提供一个匹配目标：`rule_set` 与 `hosts` exactly-one-of。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `rule_set` | string | 与 `hosts` 二选一 | 规则集名称或子选择器，例如 `geosite:cn`。 |
| `hosts` | string | 与 `rule_set` 二选一 | 本地 hosts 资源名称。 |
| `upstream` | string | `rule_set` 规则通常必填 | 命中规则集后使用的上游；引用 `upstreams[].name`。 |
| `edns_client_subnet` | object | 可选 | 规则级 ECS 覆盖，优先级最高。 |

校验约束：

- `rule_set` 和 `hosts` 不能同时出现，也不能同时缺失；
- `rule_set` 规则应提供 `upstream`；
- `hosts` 规则是本地回答，不应依赖 `upstream`；
- 所有资源引用必须存在且类型正确。

### 11.2 策略级 `cache`

策略级缓存只允许以下字段，不包含全局 `memory`、`failure_ttl` 或 `persistence`：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enabled` | boolean | `true` 选择策略逻辑池；`false` 对该策略请求完全禁用缓存。 |
| `optimistic.enabled` | boolean | 是否允许过期记录先返回并异步刷新。 |
| `optimistic.answer_ttl` | duration | 乐观缓存应答 TTL。 |
| `optimistic.max_age` | duration | 过期记录可被乐观返回的最长时间。 |

整个 `strategy[].cache` 缺失时才允许回退到全局池；对象存在时 `enabled` 必填。策略级 `ttl_override` 使用 [`dns.ttl_override`](#82-dnsttl_override) 的相同字段结构和继承语义。

## 12. `hosts[]`

顶层 `hosts` 是可被 listener 或策略规则直接命中的本地回答资源；它不同于 `upstreams[type=hosts]`，后者是一个解析上游资源。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | hosts 资源唯一名称。 |
| `type` | enum | 必填 | `const` 或 `file`。 |
| `format` | enum | 必填 | `json` 或 `hosts`；内容必须按格式严格解析。 |
| `hosts` | string | `type: const` 时必填 | 内联 hosts 内容。 |
| `path` | string | `type: file` 时必填 | 本地 hosts 文件路径，相对路径以 `work.path` 为基准。 |
| `auto_update` | boolean | `type: file` 时可选 | 是否自动重载本地文件。 |
| `update_interval` | duration | `auto_update: true` 时必填 | 自动重载间隔。 |

文件型 hosts 的 `auto_update` 只表示本地文件重载，不表示远程同步。

hosts 与 rule_set 使用相同的 per-resource snapshot 机制：本地文件变更只发布该 hosts 资源的新版本，保留其他资源版本；发布时同步更新 Policy 使用的资源内容 hash，使后续 fast cache key 切换 fingerprint。请求和 optimistic refresh 按最新资源重新进行 hosts/rule 匹配，但不因单资源变化扫描或全局清除缓存。

## 13. `outbound[]`

`outbound` 定义访问远程上游或远程规则集时使用的代理出口。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 出口唯一名称。 |
| `type` | enum | 必填 | v1 固定为 `socks5`，表示 SOCKS5 协议族。 |
| `proxy_url` | SecretRef object | 必填 | 通过 `env` 或 `file` 取得完整代理 URL，二选一。 |

SecretRef 解析后的 URL scheme 必须为 `socks5://` 或 `socks5h://`：前者在 FluxDNS 本地解析目标主机名，后者把主机名交给代理解析。实际值可能包含用户名、密码或令牌；不要将环境变量内容、文件内容或真实 URL 写入 Git 跟踪文件。其他代理类型尚未形成配置契约。

## 14. `rule_set[]`

规则集是策略匹配的数据源。资源可以内联、来自本地文件或从远程 URL 下载。运行时每个资源独立形成 `ResourceSnapshot`，拥有自己的 revision/epoch、content hash、来源 fingerprint 和 parser/compiler version；某个资源刷新不会要求其他资源同步刷新或自动清空全局缓存，但新 content hash 会进入后续请求的 policy fingerprint。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 规则集唯一名称，也可作为落盘文件名前缀。 |
| `type` | enum | 必填 | `const`、`file` 或 `remote`。 |
| `format` | enum | 必填 | 模板示例为 `json`、`clash`、`dat`。 |
| `rule` | string | `type: const` 时必填 | 内联规则内容。 |
| `path` | string | `type: file` 时必填 | 本地规则文件路径，相对路径以 `work.path` 为基准。 |
| `url` | URL | `type: remote` 时必填 | 远程规则下载地址。 |
| `proxy` | string | `type: remote` 时可选 | 下载远程规则使用的出口，引用 `outbound[].name`。 |
| `auto_update` | boolean | `type: file` 或 `remote` 时可选 | 是否定期刷新或重载资源。 |
| `update_interval` | duration | `auto_update: true` 时必填 | 刷新/重载间隔。 |

`format: clash` 表示 Clash 行格式，不是标准 YAML。监听器绑定前，所有已配置的 `const`、`file` 和 `remote` 资源都必须形成有效的首次内存快照；解析、读取、下载、代理或格式校验失败均拒绝启动。远程资源可先尝试加载 `work.rules_path` 中上一轮已成功校验并原子落盘的快照；没有有效快照且本次下载失败时必须拒绝启动。

`format: json` 接受两种 JSON dialect：

- FluxDNS legacy JSON：顶层只允许 `domain`、`domain_suffix`、`domain_keyword`、`domain_regex`，各字段接受单个 string 或 string array；
- sing-box source JSON：顶层为 `version` 和 `rules`，当前接受 `version: 1..=5`。每个 `rules[]` 只投影与 legacy JSON 同名的四个域名字段；`type`、`invert`、`network`、`ip_cidr`、端口、客户端条件、嵌套 logical rule 等其他 rule 字段全部忽略，即使忽略会改变原 sing-box 匹配语义。没有上述四个字段的单条 rule 被跳过，整份文档最终没有产生任何域名 matcher 时拒绝该资源。

该行为是面向域名集合文件的部分兼容，不是完整 sing-box source 语义兼容；使用者必须确保输入文件本身适合按四类域名字段做并集投影。sing-box 二进制 `.srs` 不支持。legacy JSON 和 sing-box 顶层结构不能混合，未知顶层字段继续拒绝。规则 parser/compiler 版本已提升为 `rule-index-v2`，旧 parser version 的远程 manifest 不作为可恢复 snapshot，启动时需要重新获取并校验资源。

进程启动后的刷新只有在下载/读取、完整解析和内容校验都成功后才能原子替换当前快照；失败时保留上一份有效快照并记录带字段路径的错误。`auto_update` 只控制启动后的刷新，不降低首次快照要求；重试采用指数退避并封顶 5 分钟，连续三次计划刷新失败或超过 `3 × update_interval` 未成功时标记资源 `stale`，但仍可使用上一份有效快照。没有旧快照的资源引用必须 fail-closed。模板暂未提供远程版本锁定或 expected checksum 字段，当前仅记录内部 content hash/source fingerprint，生产部署的固定版本策略需后续定稿。

`geosite:cn` 这类写法表示引用 V2Ray `GeoSiteList` protobuf `dat` 地理规则集中的命名子集。解析时先尝试完整资源名；完整名不存在时再按第一个 `:` 拆分。资源名大小写敏感；selector 必须非空、不超过 128 bytes，且只包含不含空白的可打印 ASCII，保留拒绝 `:` 分隔符和 `@` attribute 语法，并归一化为小写。因此 `geosite:geolocation-!cn` 可直接引用。格式不支持 selector 或 selector 不存在时在 prepare 阶段失败。`dat` 加载或刷新时一次性解析、校验并编译所有 selector，查询热路径只查当前 rule 引用的 canonical selector 和既有 matcher，不会重新解析 dat 或逐个校验全部 selector。`dat` 内容按二进制 protobuf 读取，不先转换为 UTF-8。

## 15. `clients[]`

客户端规则按请求级选择策略，并提供客户端级缓存、TTL 和 ECS 覆盖。

| 字段 | 类型 | 条件 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 必填 | 客户端规则名称，列表内唯一。 |
| `match` | object | 必填 | 客户端匹配条件，至少包含 `ids` 或 `ips`。 |
| `match.ids` | array[string] | 可选 | 精确匹配 DoH `client_id` 或其他协议提供的客户端标识。 |
| `match.ips` | array[CIDR] | 可选 | 按客户端 IP/CIDR 匹配。 |
| `strategy` | string | 可选 | 命中后使用的策略；省略时继承 listener 或 DoH route 的策略。 |
| `cache` | object | 可选 | 客户端级缓存覆盖，优先级最高。 |
| `ttl_override` | object | 可选 | 客户端级 TTL 覆写，与 `cache` 平级。 |
| `edns_client_subnet` | object | 可选 | 客户端级 ECS 覆盖。 |

匹配优先级固定为：先精确 `id`，再按 IP 的最长 CIDR 前缀。在同一优先级出现多个冲突规则时应在配置校验阶段报错，而不是依赖数组顺序。`match.ids` 和 `match.ips` 至少提供一个；`id`、`ip` 不再是顶层客户端字段。

客户端级 `cache.enabled: true` 选择“实际客户端身份 + 生效策略”逻辑池，`false` 完全禁用当前请求的缓存；只有整个客户端 `cache` 对象缺失时才继续选择策略池或全局池。客户端级 `ttl_override` 和 `edns_client_subnet` 分别遵循[覆盖和继承](#25-覆盖和继承)中的层级规则。

## 16. 配置校验清单

建议 v1 配置加载阶段至少执行以下校验：

1. `version` 存在且为 `1`；未知版本拒绝加载。
2. 拒绝未知字段；每种 `type` 只允许自己的字段集合，条件字段满足 exactly-one-of/required-if 约束。
3. 所有资源集合中的 `name` 唯一，所有引用存在、类型正确且无循环引用。
4. `work.path` 非空；相对值可基于启动配置文件目录解析为绝对的 `resolved_work_path`，缺少来源目录时拒绝；目录不存在时创建，启动配置不在该目录时复制为 `<resolved_work_path>/config.yaml`。
5. `webui.enable` 为 `true` 时创建独立 Management Server，`false` 时不创建管理 listener；普通 listener、DoH endpoint 和 Management endpoint 地址展开后不存在 TCP/UDP bind 冲突，并明确 IPv6 v6-only 行为。
6. `mode`、`format`、`type`、端口、CIDR、URL、duration、权重和内嵌内容格式合法；group 成员缺省权重为 `1`，`parallel` 和 `failover` 的显式权重只能为 `1`。
7. `ttl_override` 与 `cache` 平级；策略/客户端 cache 对象存在时 `enabled` 必填，显式 `false` 不得回退到全局池。
8. ECS 块未配置时继承，显式 `mode: disabled` 才停止继续传递。
9. `webui.users[].password_hash` 必须是受支持算法生成的单向 hash，禁止明文 `password` 字段。
10. `dns.cache.memory.max_size_bytes` 和 `persistence.max_size_bytes` 为正数，`failure_ttl` 在 `1s..=5m`。
11. DoH route 的 path 模板合法且彼此不存在语义重叠；endpoint 的 `tls.mode` 独立校验：`terminate` 必须有证书和私钥，`external` 不得有证书字段；GET/POST wire、Content-Type 和固定消息上限合法。
12. `forwarded_header`/`proxy_protocol` 必须配置 `trusted_proxies`，且可信范围只覆盖反代对端；PROXY v1/v2 前导头缺失、未知或非法时拒绝。
13. DoH 上游的 `bootstrap` 与 `connect_ip` 互斥；SecretRef 解析后的代理 scheme 合法，`socks5h://` 不得同时使用 `bootstrap`。
14. `database.type`/`database.path` 始终存在且为受支持的 SQLite 配置；prepare 阶段数据库打开、migration 或基本写入检查失败必须阻止启动。
15. 聚合统计默认开启，按日和有界 client/transport/strategy/upstream/RCODE/cache 维度持久化；`dns.resolve_log.enable` 只控制详情记录，且 `0 < eviction_threshold_records < max_records`。
16. 所有配置资源在 bind 前形成有效首次 snapshot；任何首次读取、下载或校验失败都阻止启动，后续单资源刷新失败才保留该资源旧版本；成功刷新更新 policy fingerprint，但不能触发全局 cache clear。

## 17. v1 范围外与版本化边界

以下项目不在当前 v1 配置 schema 中，不应通过猜测扩展字段：

1. `dot`/`doq` listener 的字段、TLS/QUIC 材料来源和协议特有校验。
2. 主动上游健康检查、熔断器和持久健康分数配置。
3. 远程规则的 expected checksum、版本锁定和签名验证字段；v1 只记录内部 content hash/source fingerprint。
4. 未来配置版本及 SQLite schema v5 之后的兼容窗口和 migration SQL；当前业务库升级链见 [Storage 模块](../architecture/backend/modules/storage.md)。
5. WebUI 配置写操作、缓存清理、权限分级和更长期的历史统计保留策略。

## 18. 协议依据

- [RFC 8484: DNS Queries over HTTPS](https://www.rfc-editor.org/rfc/rfc8484)
- [RFC 1035: Domain Names - Implementation and Specification](https://www.rfc-editor.org/rfc/rfc1035)
- [RFC 2308: Negative Caching of DNS Queries](https://www.rfc-editor.org/rfc/rfc2308)
- [RFC 9520: Negative Caching of DNS Resolution Failures](https://www.rfc-editor.org/rfc/rfc9520)
- [HAProxy PROXY protocol specification](https://www.haproxy.org/download/1.8/doc/proxy-protocol.txt)
