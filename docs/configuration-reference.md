# FluxDNS 配置字段参考

> 状态：初始设计草案
> 依据：[config-example.yaml](../config-example.yaml)
> 适用范围：本文描述当前配置模板表达的设计意图；在运行时实现和配置校验器落地前，未在模板或本文中定义的行为不应被视为已支持。

## 1. 配置模型概览

FluxDNS 是一个以策略为中心的 DNS 服务。配置不是一组彼此独立的开关，而是一张由命名资源和引用关系组成的策略图：

```text
listener → 客户端匹配 → strategy → 按顺序匹配的 rules
                                        ├─ hosts：本地回答
                                        └─ rule_set → upstream / upstream group → outbound
```

- `listener` 定义请求入口。
- `clients` 可按客户端标识或 IP 网段选择策略，并覆盖部分全局设置。
- `strategy` 依次匹配规则；命中 `hosts` 时使用本地解析，命中 `rule_set` 时选择上游。
- `upstreams` 包含单个解析上游、本地 hosts 上游和上游组。
- `rule_set`、`hosts` 是可被策略引用的数据资源；资源可以来自内联内容、文件或远程地址。
- `outbound` 为远程上游或规则集下载提供代理出口。

## 2. 通用约定

### 2.1 相对路径

`work.path` 是工作根目录。配置中的相对路径（例如 `./rules`、`./data`、`./cache.db`）均以该目录为基准。服务可在目录不存在时创建工作目录。

### 2.2 命名与引用

`listener`、`upstreams`、`strategy`、`hosts`、`outbound` 和 `rule_set` 中的 `name` 是资源标识。引用字段必须指向对应集合中存在的名称。

| 引用字段 | 目标集合 |
| --- | --- |
| `listener[].strategy`、`clients[].strategy` | `strategy[].name` |
| `listener[].hosts`、`strategy[].rules[].hosts` | `hosts[].name` |
| `strategy[].rules[].rule_set` | `rule_set[].name`，或 `geosite:cn` 这类数据集内的子规则标识 |
| `strategy[].rules[].upstream`、`strategy[].default_upstream` | `upstreams[].name` |
| `upstreams[].bootstrap`、`upstreams[type=group].upstreams`、`upstreams[type=group].fallbacks` | `upstreams[].name` |
| `upstreams[].proxy`、`rule_set[].proxy` | `outbound[].name` |

建议在配置校验阶段检查名称唯一性、引用存在性、类型匹配和循环引用。

### 2.3 时长、容量和网络值

| 数据类型 | 模板示例 | 说明 |
| --- | --- | --- |
| 时长 | `10s`、`24h`、`1d` | 用于缓存、超时和定时更新。不同字段的最小单位以字段说明为准。 |
| 缓存大小 | `8388608` | 字节数。 |
| IP/CIDR | `192.168.1.0/24`、`fe80::/10` | 用于客户端匹配与 ECS。 |
| 监听地址 | `0.0.0.0` | 监听绑定地址。 |

### 2.4 覆盖优先级

#### ECS

当多个层级同时提供 `edns_client_subnet` 时，优先级由高到低为：

```text
strategy.rules[].edns_client_subnet
  > strategy[].edns_client_subnet
  > clients[].edns_client_subnet
  > upstreams[].edns_client_subnet
  > dns.edns_client_subnet
```

#### 缓存

缓存配置由高到低为：

```text
clients[].cache > strategy[].cache > dns.cache
```

`cache_enabled: false` 表示当前范围禁用缓存，不再继承更低优先级的缓存设置；`true` 表示在当前范围覆盖或补充更低优先级设置。

## 3. 顶层字段

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `work` | object | 工作目录与规则文件目录。 |
| `database` | object | 解析日志等持久化能力使用的数据库。 |
| `logs` | object | 服务日志输出。 |
| `webui` | object | Web 管理界面及其访问用户。 |
| `dns` | object | 全局缓存、ECS 与解析日志默认值。 |
| `listener` | array | DNS 请求入口。 |
| `upstreams` | array | 单上游、hosts 上游或上游组。 |
| `strategy` | array | 策略与有序路由规则。 |
| `hosts` | array | 可复用的本地 hosts 资源。 |
| `outbound` | array | 代理出口。 |
| `rule_set` | array | 可复用的域名或地理规则资源。 |
| `clients` | array | 客户端匹配规则与客户端级覆盖。 |

## 4. `work`

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `work.path` | string | 是 | 工作根目录；相对路径的解析基准。 |
| `work.rules_path` | string | 是 | 规则资源的落盘目录。规则集会以 `<rule_set_name>.<format>` 形式保存到此目录。 |

## 5. `database`

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `database.type` | string | 是 | 数据库类型。当前模板仅描述 `sqlite`；其他类型尚未形成配置契约。 |
| `database.path` | string | 是 | 数据库文件或目录路径，相对路径以 `work.path` 为基准。 |

当 `dns.resolve_log.enable` 为 `true` 时，必须配置可用的 `database`。

## 6. `logs`

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `logs.enable` | boolean | 是 | 是否启用服务日志。 |
| `logs.level` | string | 是 | 日志级别。模板使用 `debug`；完整枚举应由后续运行时定义。 |
| `logs.path` | string | 是 | 日志文件路径；默认示例为 `./fluxdns.log`。 |

## 7. `webui`

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `webui.enable` | boolean | 是 | 是否启用 Web 管理界面。 |
| `webui.port` | integer | 是 | Web 管理界面监听端口。 |
| `webui.users` | array | 否 | 可登录用户列表。 |
| `webui.users[].name` | string | 是 | 用户名。 |
| `webui.users[].password` | string | 是 | 密码存储值；示例为哈希格式，而非明文密码。 |

部署配置不得将真实密码、令牌或代理凭据提交到版本库。

## 8. `dns`

### 8.1 `dns.cache`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `cache_enabled` | boolean | 是否启用全局缓存。 |
| `cache_ttl_min` | integer | 最小 TTL，单位为秒；`0` 表示使用上游返回的 TTL。 |
| `cache_ttl_max` | integer | 最大 TTL，单位为秒。 |
| `cache_optimistic` | boolean | 是否启用乐观缓存：记录过期后可先返回旧记录并异步刷新。 |
| `cache_optimistic_answer_ttl` | duration | 乐观缓存返回结果的 TTL 覆写。 |
| `cache_optimistic_max_age` | duration | 过期记录仍可被乐观返回的最大时长。 |
| `path` | string | 持久化缓存文件路径。 |
| `cache_size` | integer | 缓存文件大小，单位为字节。 |

### 8.2 `dns.edns_client_subnet`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enabled` | boolean | 是否启用 ECS 处理。为 `false` 时不向上游传递 ECS，并按当前层级规则处理客户端携带的 ECS。 |
| `custom_ip` | CIDR | 自定义 ECS 网段。 |
| `use_custom` | boolean | 是否以 `custom_ip` 覆盖客户端传递的 ECS 或客户端 IP 推导出的 ECS。 |

本节是 ECS 默认值，可被上游、客户端、策略和具体策略规则覆盖，详见[覆盖优先级](#24-覆盖优先级)。

### 8.3 `dns.resolve_log`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `enable` | boolean | 是否记录解析请求、命中策略、命中规则、ECS、缓存状态、上游结果和耗时等审计信息。 |
| `max_records` | integer | 最大保留记录数；超过后删除最早记录。 |
| `max_record_age` | integer | 最大保留天数；超过后删除历史记录。 |

启用此功能依赖 `database`。

## 9. `listener[]`

每个 listener 定义一个接入端点。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 是 | 入口唯一名称。 |
| `type` | string | 是 | 传输类型。当前模板示例为 `udp`、`tcp`、`doh`。 |
| `address` | string | 是 | 监听地址。 |
| `port` | integer | 是 | 监听端口。 |
| `strategy` | string | 是 | 默认策略，引用 `strategy[].name`。 |
| `hosts` | string | 否 | 入口级 hosts 资源；命中时本地回答，未命中时再使用 `strategy`。 |
| `path` | string | `type: doh` 时需要 | DoH HTTP 路径，支持 `{client_id}` 占位符。 |

### DoH 路径待确认项

模板注释声明 `/dns-quer/inner`、`/dns-quer/inner/{client_id}`、`/dns-quer/outside`、`/dns-quer/outside/{client_id}` 是保留路径，但同时将后两个带 `{client_id}` 的路径用作示例 listener 路径。该语义存在冲突，运行时实现前应明确：这些路径究竟是系统保留入口，还是建议使用的内置入口。

当前模板没有 `dot` listener 示例或字段定义；DoT 应被视为计划能力，待协议配置（例如 TLS 证书、私钥、握手策略）明确后再写入字段参考。

## 10. `upstreams[]`

`upstreams` 是可被策略、引导解析和上游组引用的解析资源。不同 `type` 对应不同字段集合。

### 10.1 公共字段

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 是 | 上游唯一名称。 |
| `type` | string | 是 | 上游类型。当前模板描述 `hosts`、`doh`、`group`。 |

### 10.2 `type: hosts`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `hosts` | string | 内联 hosts 数据。当前注释限定为 JSON 格式。 |

该类型可被用作其他域名型上游的 `bootstrap`，以避免首次解析依赖系统默认 DNS。

### 10.3 `type: doh`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `address` | URL | DoH 服务地址。 |
| `bootstrap` | string | 引导解析上游；当 `address` 使用域名时，用于先解析其地址。 |
| `host` | IP 或主机标识 | 直接指定上游地址的字段。模板示例用于避免首次解析依赖系统 DNS。 |
| `proxy` | string | 代理出口，引用 `outbound[].name`。 |
| `edns_client_subnet` | object | 上游级 ECS 设置；优先级低于客户端、策略和具体规则。 |

### 10.4 `type: group`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `upstreams` | array[string] | 成员上游列表。`name:weight` 形式可表达权重，例如 `aliyun:1`。 |
| `upstream_mode` | string | 成员上游选择策略。注释描述 `parallel`、`fallback`、`round-robin`、`load-balance`、故障切换五类语义。 |
| `fastest_timeout` | duration | 快速失败等待时间。 |
| `timeout` | duration | 主上游组总体超时。 |
| `fallbacks` | array[string] | 主上游均失败后的回退上游。 |
| `fallback_upstream_mode` | string | 回退上游组的选择策略。 |
| `fallback_timeout` | duration | 回退上游组总体超时。 |

`parallel` 表示并发请求，采用最快返回结果；`fallback` 表示按顺序请求，直到成功。其余模式的精确定义、失败判定与权重计算公式应在运行时设计确定后补充。

## 11. `strategy[]`

策略定义了请求在规则、hosts 和上游之间的选择方式。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 是 | 策略唯一名称。 |
| `rules` | array | 是 | 有序规则列表；从上到下匹配，命中第一条后停止。 |
| `default_upstream` | string | 是 | 没有规则命中时使用的上游。 |
| `cache` | object | 否 | 策略级缓存覆盖；优先级高于 `dns.cache`。 |
| `edns_client_subnet` | object | 否 | 策略级 ECS 覆盖；优先级高于客户端和上游。 |

### 11.1 `strategy[].rules[]`

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `rule_set` | string | 要匹配的规则集。命中后通常结合 `upstream` 选择解析上游。 |
| `hosts` | string | 要匹配的本地 hosts 资源。命中后不需要 `upstream`。 |
| `upstream` | string | 命中 `rule_set` 后使用的上游。 |
| `edns_client_subnet` | object | 规则级 ECS 覆盖，优先级最高。 |

规则应在校验时满足以下约束：

- 至少包含一个可匹配目标（`rule_set` 或 `hosts`）。
- 使用 `rule_set` 时应提供可解析的 `upstream`；使用 `hosts` 时不应依赖 `upstream`。
- `rule_set`、`hosts` 和 `upstream` 的引用必须存在。

### 11.2 策略级 `cache`

字段集合与 `dns.cache` 一致：`cache_enabled`、`cache_ttl_min`、`cache_ttl_max`、`cache_optimistic`、`cache_optimistic_answer_ttl`、`cache_optimistic_max_age`。策略级缓存不包含独立的持久化 `path` 或 `cache_size` 字段。

## 12. `hosts[]`

顶层 `hosts` 定义可被 listener 或策略规则复用的本地解析资源。它与 `upstreams[type=hosts]` 名称相近，但用途不同：前者是策略可直接命中的本地回答资源，后者是一个可作为引导解析等用途的上游资源。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 是 | hosts 资源唯一名称。 |
| `type` | string | 是 | 来源类型。当前模板示例为 `const`、`file`。 |
| `format` | string | 是 | 内容格式。模板示例为 `json`、`hosts`。 |
| `hosts` | string | `type: const` 时需要 | 内联 hosts 内容。 |
| `path` | string | `type: file` 时需要 | 外部 hosts 文件路径。 |
| `auto_update` | boolean | `type: file` 时可选 | 是否自动重新加载或更新文件型资源。 |
| `update_interval` | duration | `auto_update: true` 时需要 | 自动更新间隔。 |

## 13. `outbound[]`

`outbound` 定义访问远程上游或远程规则集时使用的代理出口。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 是 | 出口唯一名称。 |
| `type` | string | 是 | 出口类型。当前模板示例为 `socks5`。 |
| `proxy_url` | URL | 是 | 代理连接地址。可能含有凭据，不应提交真实值。 |

当前模板只展示 `socks5`，不应据此推断其他代理协议已经支持。

## 14. `rule_set[]`

规则集是策略匹配的数据源。所有规则集最终会转换为独立文件并保存在 `work.rules_path`，服务启动时加载。对于可更新资源，模板意图是在下载或读取成功且格式校验通过后替换旧文件；失败时保留现有内容。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 是 | 规则集唯一名称，也是落盘文件名前缀。 |
| `type` | string | 是 | 来源类型：`const`、`file` 或 `remote`。 |
| `format` | string | 是 | 规则格式。模板示例为 `json`、`yaml`、`dat`。 |
| `rule` | string | `type: const` 时需要 | 内联规则内容。 |
| `path` | string | `type: file` 时需要 | 本地规则文件路径。 |
| `url` | URL | `type: remote` 时需要 | 远程规则下载地址。 |
| `proxy` | string | 否 | 下载远程规则时使用的出口，引用 `outbound[].name`。 |
| `auto_update` | boolean | 否 | 是否定期刷新。 |
| `update_interval` | duration | `auto_update: true` 时需要 | 刷新间隔；当前注释将最小单位说明为小时。 |

`geosite:cn` 这类写法表示引用 `dat` 地理规则集中的命名子集；其完整选择器语法、大小写规则和不存在时的失败语义需要单独固定。

## 15. `clients[]`

客户端配置在请求级选择策略并提供最高优先级的缓存覆盖、较高优先级的 ECS 覆盖。

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `name` | string | 是 | 客户端规则名称。 |
| `id` | string | 是 | 客户端唯一标识。模板建议使用 UUID，且不超过 64 个字符。 |
| `strategy` | string | 是 | 客户端优先使用的策略，引用 `strategy[].name`。 |
| `ip` | array[CIDR] | 否 | 适用客户端 IP 网段。 |
| `cache` | object | 否 | 客户端级缓存覆盖，优先级最高。 |
| `edns_client_subnet` | object | 否 | 客户端级 ECS 覆盖，优先级低于策略、高于上游和全局。 |

客户端匹配同时出现 `id` 与 `ip` 时的匹配顺序、冲突策略和多客户端命中行为尚未在模板中定义，应在实现前补充。

## 16. 配置校验清单

建议配置加载阶段至少执行以下校验：

1. 所有资源集合内的 `name` 唯一。
2. 所有名称引用存在且引用对象类型正确。
3. `listener.type`、`upstreams.type`、`hosts.type`、`rule_set.type` 与对应字段集合匹配。
4. `strategy.default_upstream` 存在；策略规则按 `hosts` 与 `rule_set` 的语义满足字段组合约束。
5. `resolve_log.enable: true` 时数据库可用。
6. 所有 CIDR、端口、URL、时长、缓存大小和规则内容格式合法。
7. 同一监听地址、端口、协议与 DoH 路径之间不存在冲突。
8. 远程规则的 `proxy`、上游的 `bootstrap` 与 `proxy` 均能解析到已定义资源，且不存在不允许的循环引用。
9. 包含密钥、密码、代理凭据的实际配置不被提交；示例只保留无效或脱敏值。

## 17. 实现前需定稿的语义

以下内容在当前模板中尚未形成完整、无歧义的配置契约，建议在实现解析器前定稿：

1. `dot` listener 的字段、TLS 材料来源和协议特有校验。
2. DoH 保留路径与示例路径之间的冲突。
3. 所有枚举的完整取值与未知值的失败行为，例如日志级别、上游模式、格式和出口类型。
4. 上游组的轮询、负载均衡与故障切换的精确算法。
5. `clients` 的匹配顺序与冲突处理。
6. 文件型 `hosts` 的 `auto_update` 是文件重载、远程同步还是两者兼具。
7. 规则资源更新的原子替换、失败回退、首次下载失败和校验失败语义。
8. 配置版本字段、向后兼容与迁移策略。
