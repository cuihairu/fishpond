# RC 能力矩阵（F041）

**日期**：2026-11-27。证据 = TODO.md 各 DONE 行 + 全绿测试套（399 cargo / 96 套 + Node 侧 vitest 82）。
上游参考仓对照见 `docs/reference-audit.md` §3（其结论照旧有效，此处不重复）；本文只回答一件事：**这个 RC 现在能做什么、明确不能做什么**。

## 1. 平台面（已实现，测试在环）

| 能力 | 承担任务 | 状态 | 证据面 |
|---|---|---|---|
| 租户隔离（RLS + 显式租户绑定事务） | F007/F037 | 已实现 | 全 storage 套 + 隔离测试 |
| 账号生命周期（状态机/级联删除） | F009/F040 | 已实现 | f040_account_lifecycle |
| 授权登录域（凭据/API key/会话/接管 grant） | F008/F012/F034 | 已实现 | authz/credentials/takeover 套；REST 契约 |
| Worker 注册/心跳/lease 租约（围栏代数） | F005/F013 | 已实现 | f013_scheduler + f039_faults |
| 操作持久化（幂等键/attempt 重试/超时收割） | F014/F031 | 已实现 | operations 套 + fault 链 |
| 节点 drain/undrain（按名聚合、先标记后迁移） | F037 | 已实现 | f037_nodes |
| Profile 登记/独占锁/备份/安全迁移 | F022/F037 | 已实现 | f022 + f037_migrate |
| 接管恢复（死亡→接管→围栏→重认领） | F031/F039 | 已实现 | f039_faults 端到端链 |
| 双引擎选择 + 背压门（browser 通道分流） | F024/F031 | 已实现 | f031_sessions 踩踏用例 |
| Provider（**Mock 引擎**） | F004/F026 | 已实现 | mock E2E |
| 事件 inbox/投影/审计事件 | F017/F040 | 已实现 | events 套 |
| MCP 服务面（stdio/HTTP） | F006 | 已实现 | rmcp 套 |
| REST + OpenAPI + 三语 SDK | F025/F036 | 已实现 | openapi fixture + sdk 契约测试 |
| Console 管理台（api 客户端/接管面板） | F035 | 已实现 | vitest 28 |
| 遥测管道（webhook 签名/深度观测/IncidentLog） | F030/F031 | 已实现 | f031_integration/security |
| 部署面（compose 栈/多节点/doctor/migrate） | F029/F037 | 已实现 | compose config 校验 + doctor |
| 数据保留（purge/dry-run/审计永不清扫） | F040 | 已实现 | purge 套 |
| 稳定性（soak/故障注入/备份恢复演练/容量报告） | F039 | 已实现 | soak/faults/report 套 + 演练记录 |
| 供应链（SBOM/license/advisory 政策） | F041 | 已实现 | sbom/ + deny.toml |

## 2. Provider 面（诚实边界）

| 能力 | 状态 | 边界 |
|---|---|---|
| Mock Provider（默认且唯一 CI 目标） | 已实现 | 行为模型在环，非真实协议 |
| 官方开放平台 API 适配（F032） | **BLOCKED** | 平台文档需授权（见 reference-audit §4）；未获授权不得实现 |
| 网页协议适配（F033） | **BLOCKED** | 协议未开源；参考仓方案只可作设计思路，禁止复制逆向产物 |
| 端到端真实发送（F038） | **BLOCKED** | 依赖 F032/F033 授权 |

## 3. 红线（RC 交付声明）

- Mock 是默认且唯一 CI 目标；未获人工授权不启用真实连接。
- 平台凭据不进日志/事件/指标/CLI 输出（渲染面有已知密钥+形状扫描测试）。
- 禁止发布正式版本；每日构建/nightly 滚动二进制是交付面（验收测试通过前不进 Releases 页）。
