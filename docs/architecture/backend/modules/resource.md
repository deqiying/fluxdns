# Resource 模块设计

> 文档状态：有效
>
> 适用范围：hosts/rule 解析、加载、snapshot、持久化和刷新发布
>
> 最后评审：2026-09-05（模块边界与关键契约静态核对，基线见[模块索引](README.md)；不含运行验收）
>
> 关联实现：[loader.rs](../../../../backend/src/resource/loader.rs)、[fetcher.rs](../../../../backend/src/resource/fetcher.rs)、[remote.rs](../../../../backend/src/resource/remote.rs)、[rules.rs](../../../../backend/src/resource/rules.rs)、[hosts.rs](../../../../backend/src/resource/hosts.rs)
>
> 关联文档：[后端架构](../overview.md) · [配置字段参考](../../../implementation/configuration.md) · [Policy](policy.md) · [Runtime](runtime.md) · [Upstream](upstream.md)

## 1. 职责

Resource 模块负责 hosts 和 rule_set 的读取、下载、解析、规范化、编译、落盘、刷新和 per-resource snapshot。

内部结构：

| 文件 | 职责 |
| --- | --- |
| `hosts.rs` | JSON/hosts 格式、本地 RR 索引 |
| `rules.rs` | JSON/Clash/`geosite.dat` 规则解析和 matcher |
| `loader.rs` | const/file、大小限制、稳定读取与 parser 边界；remote 内容加载由 `remote.rs` 编排 |
| `orchestrator.rs` | schedule、refresh coordinator、due/backoff、CAS publish 和 stop 语义的 Runtime-facing 纯逻辑编排 |
| `snapshot.rs` | metadata、revision、registry 和 publish input |
| `remote.rs` | remote fetch、manifest/content 原子落盘与恢复校验 |
| `fetcher.rs` | Reqwest HTTP(S)/proxy、deadline、body 限额 |
| `scheduler.rs` / `refresh.rs` | 周期/backoff、reservation、file/remote 刷新 worker |

查询热路径只读取编译后的不可变索引，不访问文件、网络或 parser。

## 2. ResourceSnapshot

每个资源独立记录：

- typed resource ID/name；
- monotonically increasing epoch/revision；
- content hash；
- source fingerprint：file 长度/mtime 等元数据，remote 为内容 checksum/长度摘要；当前不是 ETag/Last-Modified 条件验证器；
- parser/compiler version；
- `fetched_at`，没有独立 `compiled_at` 字段；
- source kind 与脱敏位置；
- 是否使用已落盘 fallback；
- stale/degraded 状态；
- immutable compiled index。

顶层 ResourceRegistrySnapshot 是资源名到 `Arc<ResourceSnapshot>` 的不可变 map。

候选 Runtime 以自身 registry 为基准合并旧 Runtime 的资源结果时，只允许定义兼容的资源进入合并集合，并且仅接收严格更高的 `ResourceVersion`；同版本内容保留候选自身的值，避免旧 Runtime 覆盖新配置候选。该原语只处理 immutable registry；Policy、worker schedule 与 Runtime metadata 的同步由 Runtime 边界统一协调。

## 3. 加载流水线

```text
bounded read/fetch
  → content hash
  → format parse
  → canonical normalize
  → semantic validation
  → compile immutable index
  → persist raw+manifest atomically when needed
  → publish candidate with epoch
```

解析和编译必须全部成功后才能落盘为“有效 snapshot”。下载了一半或只通过语法解析的内容不能替换旧版本。

## 4. Source

### const

直接读取 Config 已提供的字符串，仍执行大小、格式和语义校验。

### file

- 路径已由 Config 归一化；
- 使用周期性 stat/content fingerprint，不依赖平台文件 watcher；
- 文件变化后完整读取并编译；
- 读取期间文件变化时丢弃并重试；
- `auto_update=false` 只在 prepare 加载一次。

### remote

- 通过已绑定 ResourceFetcher/outbound 下载；
- 使用 deadline、body 大小上限和 HTTPS 校验，显式禁用环境代理和自动重定向；
- 每次发起普通 GET；`ResourceFetchRequest` 没有条件请求字段，不发送 `If-None-Match`/`If-Modified-Since`；
- 只接受 HTTP 2xx，304 当前作为非成功状态失败，`modified_at` 返回 `None`；
- body 先有界读入内存、计算 hash 并完成解析，再落盘；不是边下载边写临时文件；
- v1 没有 expected checksum 配置，不把内部 hash 宣称为来源真实性校验。

## 5. Hosts 格式

v1 本地回答支持 A、AAAA 和 CNAME：

- JSON 可为单值或数组，并尊重显式 `enable=false`；
- hosts 行格式接受 `IP name...`，同一 owner 的 A/AAAA 合并；
- `*.example.com` 只匹配子域，不匹配 apex；
- exact 优先于 wildcard，wildcard 取最长后缀；
- 同一 owner 的 CNAME 不能与 A/AAAA 并存；
- CNAME target 也按 canonical domain 校验；
- 重复相同 RR 去重，冲突类型报错；
- TTL 使用模块实现常量，最终仍受 Policy TTL override。

`upstreams[type=hosts]` 可复用 parser/compiler，但产出 connector；顶层 `hosts[]` 产出本地回答 matcher，两个资源命名空间不混用。

## 6. Rule 格式

规则格式包括 JSON、Clash 与 V2Ray `geosite.dat` protobuf selector map。`Full`/`RootDomain`/`Regex`/`Plain` 分别映射 exact/suffix/受限 regex/keyword；未知 protobuf 字段按 wire type 跳过，输入、selector 和规则数量必须有界。parser/compiler 标识用于持久化兼容性检查，实际版本以[配置参考](../../../implementation/configuration.md)和源码为准。

### JSON

FluxDNS legacy JSON 顶层支持明确字段：

- `domain`：exact；
- `domain_suffix`：apex 与所有子域；
- `domain_keyword`：substring keyword；
- `domain_regex`：受限 regex 列表或单值。

legacy 顶层未知字段拒绝。相同的 `format: json` 还可读取 `version + rules[]` 结构的 sing-box source JSON，当前接受 version 1 到 5；每个 rule 只读取上述四个直接域名字段，其他字段一律忽略。没有四个字段的 rule 跳过，最终未生成任何 matcher 的文档拒绝。该投影可能丢弃 `invert`、network、IP、port 或 logical 等组合语义，因此只属于面向域名集合文件的部分兼容，不表示完整实现 sing-box rule-set。regex 在加载时编译，并限制数量、长度和总 program size。

### Clash

v1 接受 `DOMAIN`、`DOMAIN-SUFFIX` 和 `DOMAIN-REGEX` 行。空行和注释忽略；未知 rule type、缺列或多余不可解释字段报带行号错误。

### dat（geosite.dat）

目标契约为 selector → domain matcher map，采用 V2Ray `GeoSiteList` protobuf schema。selector 必须非空、不超过 128 bytes，且仅含除 `:`、`@` 外的不含空白可打印 ASCII；加载时统一转为小写，大小写归一化后重复的 selector 报错。该规则允许 `geolocation-!cn`。`geosite:cn` 先解析大小写敏感的资源名 `geosite`，再查 canonical selector `cn`。解析器只依赖现有代码，拒绝截断、非法 wire type、未知 domain type、超限 selector 和超限规则；所有 selector 在资源加载或刷新时一次性校验并编译到不可变 map，Policy 在 prepare 阶段校验引用存在，查询热路径只查当前引用的 matcher。

## 7. Matcher

当前编译索引：

- `HostsIndex` 的 exact/wildcard 使用 `BTreeMap`，wildcard 按域名 label 逐级查最长后缀；
- `RuleIndex` 的 exact/suffix 使用 `BTreeSet`，suffix 逐级枚举查询，不是 reversed-label trie；
- keyword 使用有序列表，按 first-match 返回；
- regex 使用有界的自有 `CompiledRegex` token 列表，支持受限原子和量词，拒绝分组/分支等语法，不是外部 RegexSet；
- dat selector 使用不可变 map，值为已编译 `Arc<RuleIndex>`。

Hosts 为 exact → 最长 wildcard；rules 为 exact → 最长 suffix → keyword → regex。matcher 无内部 mutable cache，snapshot 可跨线程共享。

## 8. 首次启动与 fallback

所有配置资源在 bind 前必须有有效 snapshot：

1. const/file 直接加载；
2. remote 先读取 content/manifest pair，并校验 manifest schema、资源身份、格式、parser 版本、字节长度和内容 hash；
3. 已校验的落盘 snapshot 可作为 fallback，并标记 `used_fallback`；
4. 没有可用 fallback 时再尝试本次远程获取；
5. 获取成功使用新内容；
6. 两者都不可用则 prepare 失败。

落盘 manifest 包含资源 ID、format、byte length、checksum/content hash、parser version 和可选 modified time；不保存 URL、ETag 或独立的成功抓取时间。当前 fetcher 不返回 modified time，恢复生成的 `fetched_at` 不能视为已持久化的远端更新时间。版本不兼容时不使用。

## 9. 刷新与发布

每个资源 single-flight：

- scheduler 分配新 epoch；
- 下载/读取可并发；
- 编译成功后提交 `PublishResource(resource, epoch, snapshot)`；
- epoch 旧于当前时丢弃 stale result；
- Runtime coordinator 基于最新 ActiveRuntime 合并并 CAS；
- 成功发布只替换该资源；
- 不触发全局 cache clear。

失败退避指数增长并封顶 5 分钟。连续三次计划刷新失败或超过 `3 × update_interval` 无成功时标记 stale，但继续使用旧 snapshot。

刷新协调器只组合 schedule、reservation、per-resource single-flight、epoch/CAS、backoff、cancel 与 shutdown，不自己执行 I/O。worker 在 reservation 内完成有界读取/抓取、hash、parse/persist，再发布候选。资源内容更新不创建完整 Runtime candidate；Policy 内部 matcher/version/hash 一起发布，再更新 Runtime metadata，不构成跨两个对象的原子事务。真实 fetcher、auto-update task 与 prepare 接线见[后台服务实现](../../../implementation/backend/background-services.md)。

## 10. 原子落盘

remote 有效内容与 manifest：

1. 写同目录临时文件；
2. `sync_all` 文件内容；
3. 原子 rename 内容；
4. 写入、`sync_all` 并原子 rename manifest；
5. 当前操作失败时清理本次临时文件。

content 与 manifest 各自原子替换，但不构成跨文件事务；恢复时只有 resource id、format、parser version、byte length 和 content hash 均一致的 pair 才可使用。校验失败不会产生 fallback snapshot，服务器宕机时的跨文件绝对持久化不作为当前 MVP 阻塞项。

## 11. 安全与观测

- URL 只记录 scheme/host 的脱敏摘要，不记录 credential/query；
- 资源内容、域名清单和 regex 原文不进入普通日志；
- parser error 可记录行号、字段路径和短截断 token；
- remote body、压缩解码和 selector 数量均有上限；
- 不跟随非 HTTP(S) scheme，不把 file URL 当远程资源。

## 12. 测试

- hosts JSON/line、A/AAAA/CNAME、wildcard/exact；
- JSON/Clash rule、`geosite.dat` protobuf selector 及其稳定错误边界；
- sing-box source 四类域名字段投影、其他 rule 字段忽略、无可用域名规则和未知 version；
- canonical domain、重复、冲突和 regex 限制；
- const/file/remote 首次加载；
- 普通 GET、body limit、拒绝重定向/304 和代理 failure；条件请求属于[未接线差距](../../../plans/backend-contract-gaps.md)，不是现有用例通过项；
- fallback manifest/hash/version；
- 单资源刷新失败保留旧版本；
- epoch 乱序、并发不同资源 CAS 不丢更新；
- 原子落盘中断恢复；
- 刷新不触发全局 cache invalidation。
