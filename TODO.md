# Fishpond（鱼塘）TODO

> 目标契约见会话文档《Fishpond—架构设计与 Code Agent 实施规范》v1.0（2026-11-25）。
> 本 TODO 按 §22 任务清单维护；任务状态 TODO / IN_PROGRESS / BLOCKED / DONE。
> 每任务完成模板见 §22.2。

## M0（F000–F008）：构建、数据库、契约、权限与 Mock 基础

- [x] F000 初始化 README、AGENTS、TODO、许可证决策 ADR
  - Dependencies: —
  - Implementation files: README.md / AGENTS.md / TODO.md / docs/adr/0001-license.md
  - Commands and actual results:
  - Limitations / BLOCKED reasons:
  - Commit:
- [x] F001 参考仓库固定 commit、许可证审计、能力矩阵；未验证项标 BLOCKED
  - Dependencies: F000
  - Implementation files: docs/reference-audit.md / docs/capability-matrix.md
  - Commands and actual results:
  - Limitations / BLOCKED reasons: 上游接口可用性未验证，全部能力默认 unverified
  - Commit:
- [x] F002 Rust workspace、Node workspace、CI、锁文件、fmt/clippy/test；干净环境构建
  - Dependencies: F000（DONE）
  - Implementation files: Cargo.toml（workspace: resolver=3, edition 2024, rust-version 1.88, workspace.lints）/ rustfmt.toml / Cargo.lock / crates/{domain,provider-sdk,provider-official,provider-mock,storage,runtime,scheduler,router,operations,events,monitor,server,worker,cli}/（14 成员，server|worker|cli 含 main.rs）/ crates/domain/tests/no_external_deps.rs / crates/provider-mock/src/lib.rs / scripts/check_workspace.py / package.json / pnpm-workspace.yaml / pnpm-lock.yaml / packages/console/**（Vue+Vite 占位）/ packages/browser-worker/**（Playwright 占位）/ .github/workflows/ci.yml / .gitignore（+dist/）
  - Commands and actual results:
    - `python3 scripts/check_workspace.py` → `workspace structure ok: 14 members, layering rules satisfied`
    - `cargo fmt --all -- --check` → OK（0 diff）
    - `cargo clippy --workspace --all-targets -- -D warnings` → 零警告
    - `cargo test --workspace` → 32 个 target 全 `test result: ok`（含 domain 零外部依赖钉测试、provider-mock smoke 各 1 条）
    - `pnpm install --frozen-lockfile` → `Lockfile is up to date, resolution step is skipped`
    - `pnpm run lint` → console / browser-worker eslint 均 Done
    - `pnpm run test` → vitest `Test Files 1 passed (1)`, `Tests 1 passed (1)`
    - `pnpm run build` → vite `✓ built`（dist/assets 64.86 kB）+ browser-worker `tsc` Done
    - 远端 CI（run 45502289757，headSha 8b0fbc0，completed / success）：`structure (§3.2 members / §3.3 layering)` success；`rust (ubuntu-latest, 1.88)` success；`rust (macos-latest, 1.88)` success；`rust (windows-latest, 1.88)` success；`node (lint/test/build)` success
  - Limitations / BLOCKED reasons:
    - Node 侧门禁用 `pnpm install --frozen-lockfile` 代替 AGENTS.md 中的 `npm ci`（pnpm workspace 无 package-lock.json）；AGENTS.md 的 Node 命令行建议后续统一为 pnpm 写法。
    - 各 crate 当前为空实现且无相互依赖；§3.3 分层规则由 `scripts/check_workspace.py`（operations 禁依赖 server/worker/router；provider-* 仅允许 provider-sdk/domain/storage；domain 禁外部 crate）与 `crates/domain/tests/no_external_deps.rs` 双重钉住，后续加入依赖即自动生效。
    - 本单不带 cargo-wix/msi 发布，也未建 compose/`proto/`/`deploy/`（属 F020/F029 范围）。
    - README「仓库结构」摘要仍写 `browser/`、`web/`，与本次 §3.2 实际布局（packages/console、packages/browser-worker）不一致；因需以会话文档 §3.2 原文为准，留待确认后再改。
  - Commit: 9ae76be（Rust workspace）/ 90c6348（Node workspace）/ 8b0fbc0（CI workflow）
- [ ] F003 domain DTO、强类型 ID、错误及状态转换；非法转换单元测试
  - Dependencies: F002
  - Commit:
- [ ] F004 provider-sdk、manifest、结果语义、版本协商；mock 契约测试
  - Dependencies: F003
  - Commit:
- [ ] F005 Mock Platform 与 Mock Provider（授权/写入/断连/重复事件故障开关）
  - Dependencies: F004
  - Commit:
- [ ] F006 PostgreSQL migrations、tenant FK、索引、repository
  - Dependencies: F003
  - Commit:
- [ ] F007 租户与 API key、scope、RBAC、RLS 策略及跨租户测试
  - Dependencies: F006
  - Commit:
- [ ] F008 Credential Store 信封加密、轮换、撤销；秘密不进日志测试
  - Dependencies: F006
  - Commit:

## M1（F009–F020）：账号/调度/可靠任务与事件

- [ ] F009 账号管理、授权会话、状态 API 服务逻辑（依赖 F006,F008）
- [ ] F010 AccountSupervisor/Actor、bounded mailbox、取消、重启（依赖 F009）
- [ ] F011 SessionCoordinator、凭据版本、互斥、人工接管状态转换（依赖 F010）
- [ ] F012 Worker 注册、心跳、lease/generation、CAS；T01/T02/T20（依赖 F006,F010）
- [ ] F013 Scheduler 容量和亲和、drain、故障接管（依赖 F012）
- [ ] F014 operation/attempt 持久化、claim、deadline、幂等；T03/T04/T07（依赖 F006,F012）
- [ ] F015 reconcile 服务、UNKNOWN 和人工核对（依赖 F014,F004）
- [ ] F016 Capability Router、授权优先级、错误分类、熔断；T05/T06/T23（依赖 F004,F014）
- [ ] F017 inbox/投影/outbox、事件序列、去重、缺口事件；T13（依赖 F006,F004）
- [ ] F018 Webhook 订阅、签名、SSRF、delivery 重试/重投；T14/T17（依赖 F017,F007）
- [ ] F019 SSE 续订/回放/权限、慢消费者处理；T15/T16（依赖 F017,F007）
- [ ] F020 Protobuf 生成、内部服务身份、契约兼容测试（依赖 F004,F012）

## M2（F021–F031）：双引擎、API、单机完整 Mock 交付

- [ ] F021 Browser Worker、Playwright、Mock 页面 Provider、资源队列（依赖 F020,F011）
- [ ] F022 Profile Manager、独占锁、存储、备份/恢复、销毁；T11/T12（依赖 F021,F008,F012）
- [ ] F023 Page Object 操作、前后条件和结果证据；T08（依赖 F021,F014）
- [ ] F024 双引擎路由接入、按能力降级、浏览器背压；T09/T25（依赖 F021,F016,F022）
- [ ] F025 Axum REST/OpenAPI、Problem Details、cursor、幂等、契约测试（依赖 F009,F014,F016,F017,F018,F019）
- [ ] F026 Mock 端到端流程：授权→消息→商品→订单→操作→Webhook（依赖 F025）
- [ ] F027 Monitor、健康注册、能力禁用、契约异常诊断、incident API（依赖 F016,F004）
- [ ] F028 Provider manifest/digest、签名校验、版本并存、灰度与回滚；T19（依赖 F027,F020）
- [ ] F029 Compose、迁移 job、secret mount、健康检查、CLI doctor（依赖 F025,F026）
- [ ] F030 tracing、指标、队列深度、资源告警；脱敏扫描（依赖 F010,F014,F017）
- [ ] F031 security/integration/chaos 测试和 1,000 Mock 会话初测（依赖 F026,F029,F030）

## M3（F032–F038）：真实授权能力逐项验收、Console、SDK、多节点

- [ ] F032 已批准官方 API 适配；未获授权项保留禁用（依赖 F001,F004,F025）— BLOCKED：待官方授权确认
- [ ] F033 已允许网页操作适配；授权流程与页面回归（依赖 F001,F023,F024）— BLOCKED：待授权范围确认
- [ ] F034 远程人工接管服务、短时凭证、访问隔离；T10（依赖 F011,F022,F025）
- [ ] F035 Vue Console 各管理页、权限及 UNKNOWN 正确展示（依赖 F025,F027,F034）
- [ ] F036 从 OpenAPI 生成 Python/TS/Go SDK（依赖 F025）
- [ ] F037 多节点部署、Profile 安全迁移、租户隔离、节点 drain（依赖 F013,F024,F029）
- [ ] F038 授权测试环境 Provider 灰度、契约回归与故障 runbook（依赖 F028,F030,F032,F033）

## M4（F039–F041）：稳定性和发布

- [ ] F039 72h 模拟稳定性、故障注入、备份恢复、容量报告（依赖 F031,F037）
- [ ] F040 完整账号删除、凭据撤销、数据保留与审计；T18/T21（依赖 F007,F008,F022,F035）
- [ ] F041 发布 RC：SBOM、许可证、漏洞、性能、能力矩阵、回滚文档（依赖 F039,F040）

## M5（F042–F044）：扩展

- [ ] F042 集群 10,000 模拟会话压测（依赖 F039）
- [ ] F043 HA PostgreSQL 与控制平面多副本演练（依赖 F041）
- [ ] F044 外部受控 Browser Provider 接入（依赖 F041）
