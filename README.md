# FluxDNS

> 文档状态：有效
>
> 适用范围：项目概览、使用入口与仓库导航

面向策略分流的 DNS 服务，采用 Rust 后端与 React WebUI，可将管理界面内嵌为单个发布二进制。

正式后端入口已连接 UDP/TCP/DoH、Policy、真实上游 connector、Moka/SQLite 缓存、资源刷新、聚合统计和可选解析详情。独立 Management HTTP 服务提供首次初始化、Cookie session、七个只读 API 与 SPA。上述为源码接线摘要，不表示所有平台、故障或浏览器场景已经验收。

能力与证据见[当前实现](docs/implementation/README.md)，剩余差距见[活动计划](docs/plans/README.md)。DoT/DoQ、主动上游健康检查和通用配置编辑不属于当前已接线能力。

## 使用入口

- 配置格式：[配置参考](docs/implementation/configuration.md) 与 [config-example.yaml](config-example.yaml)。示例值不能直接作为受保护的生产配置。
- 构建、内嵌打包和显式配置启动：[交付实现](docs/implementation/delivery.md)。
- 前端开发：[frontend/README.md](frontend/README.md)。
- 工具来源和版本：[环境规则](docs/rules/environment-usage.md)；本地配置与运行数据：[本地测试规则](docs/rules/local-testing.md)。

本地测试文件统一放在忽略的 `_fluxdns/`；凭据、真实账号/hash、数据库和日志不得提交。CLI 默认配置与开发脚本显式配置参数的区别见交付文档。

## 仓库布局

| 路径 | 职责 |
| --- | --- |
| `backend/` | Rust binary crate；从根目录用 `--manifest-path backend/Cargo.toml` 调用 |
| `frontend/` | 独立 WebUI、OpenAPI、生成类型与契约 fixtures |
| `docs/` | [plans、architecture、implementation、rules](docs/README.md) |
| `script/` | 本地打包、版本与进程管理入口 |
| `.agents/skills/project-doc-maintenance/` | 文档维护技能与检查器 |
| `.github/workflows/release.yml` | tag 门禁与三平台发布流程 |
| `VERSION` | 发布版本的唯一入口 |
| `deploy/`、`_fluxdns/` | 忽略的发布物与本地运行数据 |

设计取舍见[架构入口](docs/architecture/README.md)，变更协作从 [AGENTS.md](AGENTS.md) 开始。项目名使用 `FluxDNS`，binary/服务标识使用 `fluxdns`。
