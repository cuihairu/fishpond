# Fishpond Agent 工作指令

> 本文档是 §7「Code Agent 实施协议」的仓库落地版。开工前必读。

## 开工顺序（严格按依赖 DAG 执行）

M0：F000 → F001 / F002 → F003 → F004 → F005 → F006 → F007 / F008
M1：F009 / F012 → F010 → F011 → F014 → F013 / F015 / F016 → F017 → F018 / F019 / F020
M2：F021 / F025 / F026 → F022 → F023 → F024 → F027 → F028 → F029 → F030 → F031
M3：F032–F038（多数依赖外部授权确认，先做 F035/F036/F040）
M4：F039 → F041 → F040；M5：F042–F044

## 每个任务六步（不可省略）

1. 读 TODO 行，写 IN_PROGRESS
2. 查依赖 DONE
3. 先写测试再实现
4. 跑门禁：`cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`（Node 侧：`npm ci && npm run lint && npm run test --if-present && npm run build --if-present`）
5. 更新 TODO 实际结果与 BLOCKED 原因
6. commit + push main（英文短句，禁 force push / tag / release / bump；只 add 自己改的文件，禁 `git add -A`）

## 红线（违反即任务失败）

- 未拿到授权不得实现真实连接；未确认 Provider 不得启用；Mock 是默认且唯一 CI 目标。
- 不复制 `refs/` 上游代码；只记录能力矩阵与公开协议。
- 禁止把平台凭据写入日志、事件、指标或 CLI 输出。
- 禁止只写「已完成」；交付必须含测试命令与结果、限制、commit hash。
- 未经人工明确批准，不得修改 TODO 中已 DONE 任务的依赖。
- 任务必须验证后提交，不允许未完成的模拟结果。

## 验证基线

- `cargo test --workspace` 全绿
- `cargo clippy --workspace --all-targets -- -D warnings` 零警告
- `cargo fmt --check` 通过
- 每次改动至少一条端到端路径或测试证明
- 安全、授权、状态转换、幂等、租户隔离、凭据安全必须有测试

## TODO 格式（每行六件套）

```
- [ ] F001 标题
  - Dependencies: F000
  - Implementation files: …
  - Commands and actual results: …
  - Limitations / BLOCKED reasons: …
  - Commit: …
```
