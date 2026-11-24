# Fishpond（鱼塘）

> 一个以「可验证账号状态」为事实来源、面向闲鱼等平台的账号调度与操作基础设施。
> 实施契约见会话文档《Fishpond—架构设计与 Code Agent 实施规范》v1.0（2026-11-25），本文件只做摘要。

## 定位（一句话）

**闲鱼账号不是 Cookie，是一个长期运行、可被观测、可人工接管、可被审计的会话实体。**

Fishpond 不替代任何上游开放平台的官方 API、授权协议或风控策略；它是围绕「已合法授权账号」的账号会话调度、操作执行、状态观测、事件分发和任务审计基础设施。

## 设计原则

- **先有 Mock 与 Provider 抽象，再接真实上游。** 所有测试先以模拟上游完成。
- **浏览器页面不是事实源。** 本地 DB 保存 `account_id` 映射、当前授权状态、最后已知页面状态和事件。
- **状态与操作分离。** 观测状态（`observed`）、执行操作（`operated`）与上游确认（`confirmed`）严格分开。
- **所有账号状态变更必须可追溯。** 事件日志、操作意图、操作执行、上游结果、审计记录关联。
- **凭据不进日志。** 敏感数据加密存储，脱敏输出。

## 状态模型（摘要）

`UNREACHABLE → WAITING_SCAN → WAITING_CONFIRMATION → AUTHORIZING → ONLINE → DEGRADED → RECOVERING → WAITING_HUMAN → REAUTH_REQUIRED → OFFLINE → CLOSED`

## 安全边界

- 平台凭据只能存在于 Credential Store。
- 任何日志、事件、指标、CLI 输出不得包含 Cookie、token、密钥、二维码内容、短信内容。
- 真实上游连接必须显式启用；默认仅 Mock。

## 仓库结构

见会话文档 §3.2。`crates/` 为 Rust 服务核心，`browser/`（Playwright Worker）、`web/`（Vue Console）、`proto/`（Protobuf）、`tests/`（契约/混沌/负载/安全）、`deploy/`（Compose/systemd）、`docs/`（ADR/API/Runbook）。

## 开发入口

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## 参考实现（仅供调研，不得直接复制上游代码）

`refs/` 目录（gitignore，不入库）：XianyuAutoAgent（GPL-3.0）、xianyu-auto-reply-fix（AGPL-3.0）、xianyu-pilot-pro（Apache-2.0）、xianyu-guanjia（MIT）、XianYuApis（无许可证）。能力矩阵见 `docs/capability-matrix.md`。
