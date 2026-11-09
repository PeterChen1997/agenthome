# AgentHome 文档索引

面向使用者的文档：

| 文件 | 内容 |
|---|---|
| [features.md](features.md) / [features.zh-CN.md](features.zh-CN.md) | 逐页面的功能说明（英文 / 中文），配截图 |
| [12-成本口径核对.md](12-成本口径核对.md) | 成本怎么算：单价、缓存读写分价、长上下文分档、跨会话去重，以及与 ccusage 的逐项对照 |
| [screenshots/](screenshots/) | 截图（全部为演示数据） |
| [../CHANGELOG.md](../CHANGELOG.md) | 版本变更记录 |

下面是开发过程中的设计、调研与记录，主要是中文。标「内部记录」的是开发过程留档，内容停留在写作当时，可能与现状不一致，以代码和上面的文档为准。

## 产品与架构

| 文件 | 内容 | 性质 |
|---|---|---|
| [00-调研总览.md](00-调研总览.md) | GPUI / GPUI Kit 是什么、为什么火、独特性、风险、我们借什么 | 背景调研 |
| [01-产品与架构方案.md](01-产品与架构方案.md) | 产品定义、存储策略（软链 vs 拷贝，结论：拷贝 + 三层仓库）、架构、Provider 设计、路线图、已拍板决策 | 设计方案 |
| [03-功能清单.md](03-功能清单.md) | 信息架构与功能分解（token 总量、趋势、重置信用、到期提醒等），带实现状态标记 | 设计方案 |
| [04-栖息地状态系统.md](04-栖息地状态系统.md) | 三位住户的状态机、触发条件、动画、素材清单与实现思路 | 设计方案 |
| [06-statusline-安装.md](06-statusline-安装.md) | 旧版 Claude statusline 包装脚本的原理与安装步骤（额度现已改走官方 `/usage`，此方式仅作兜底） | 内部记录 |
| [design/](design/README.md) | 设计稿 v2.1「会动的栖息地」源文件、素材、截图、方向头脑风暴，以及实现与设计稿的差异 | 设计稿 |

## 开发过程

| 文件 | 内容 | 性质 |
|---|---|---|
| [02-P0-验证记录.md](02-P0-验证记录.md) | P0 退出标准逐项结果、真实数据暴露的问题 | 内部记录 |
| [05-P1-开发计划.md](05-P1-开发计划.md) | P1 分块、接口约定、退出标准 | 内部记录 |
| [07-P1-集成记录.md](07-P1-集成记录.md) | P1 集成结果、版本历史、已知偏差 | 内部记录 |
| [08-构建与发布流程.md](08-构建与发布流程.md) | 发版全流程：工具链、签名、公证、GitHub Release、发布后验证，含踩坑 | 维护者手册 |
| [09-开发踩坑记录.md](09-开发踩坑记录.md) | GPUI、数据解析、额度、生图素材、验证工具的坑 | 内部记录 |
| [10-设计落地清单.md](10-设计落地清单.md) | 设计稿与规格逐项对照代码的验收清单、收尾批次 | 内部记录 |
| [11-开发流水线.md](11-开发流水线.md) | 每项功能从认领到发布的关卡与检查项 | 内部记录 |

## 原始调研

| 文件 | 内容 |
|---|---|
| [research/01-gpui-framework.md](research/01-gpui-framework.md) | GPUI 框架深度调研 |
| [research/02-gpui-kit-ecosystem.md](research/02-gpui-kit-ecosystem.md) | GPUI Kit 组件库与生态应用 |
| [research/03-agent-session-formats.md](research/03-agent-session-formats.md) | 17 种 agent 的本地会话存储位置与格式、规范化模型草案 |
| [research/04-usage-apis.md](research/04-usage-apis.md) | 用量 / 配额数据来源、ToS 风险、竞品实现细节 |

贡献代码前请先读仓库根目录的 [CONTRIBUTING.zh-CN.md](../CONTRIBUTING.zh-CN.md)。
