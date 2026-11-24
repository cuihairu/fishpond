# ADR-0001：Fishpond 自身许可证决策

**日期**：2026-11-25
**状态**：Proposed（待人工拍板）

## 上下文

- 会话文档 §3.1 明确「Fishpond 自身许可证尚未决定」。
- 参考仓库许可证实查（`refs/` 浅克隆，只读）：

| 参考仓 | commit | 许可证 |
|---|---|---|
| XianyuAutoAgent | 540bbc2 | GNU GPL v3 |
| xianyu-auto-reply-fix | 6673f44 | GNU AGPL v3 |
| xianyu-pilot-pro | 4ac9222 | Apache-2.0 |
| xianyu-guanjia | 24b93d3 | MIT |
| XianYuApis | c6c87e6 | **无许可证文件**（代码视为保留所有权利，不可复用） |

- 上游平台协议本身未开源（§3.1：公开文档不可用或需要授权）。

## 决策

许可证候选（对比表，需人工拍板）：

| 方案 | 优点 | 缺点 | 适用 |
|---|---|---|---|
| **Apache-2.0**（推荐） | 与 xianyu-pilot-pro 同族，专利授权；商用友好；允许闭源衍生品 | 无 copyleft 传染性 | 想被集成、想商用、允许闭源下游 |
| GPL-3.0 | 强制开源传播 | 与闭源 Console/服务化冲突；与 Apache 系参考实现组合摩擦 | 强调开源社区 |
| AGPL-3.0 | 网络服务也传染 | 与「托管平台形态」根本冲突，几乎封死交付 | 不适用 |
| MIT | 最宽松 | 无专利授权 | 最小保护 |

**推荐：Apache-2.0**（§3.1 已点名 Apache-2.0 为候选之一，且参考矩阵里唯一 Apache 系实现可最大程度复用其公开设计思路）。

## 影响

- 若确认 Apache-2.0：`LICENSE` 落 Apache-2.0 全文，`NOTICE` 记录第三方依赖。
- `XianYuApis` 无许可证 → 一律视为不可复用（仅可作能力调研）。
- GPL/AGPL 系参考仓（XianyuAutoAgent、xianyu-auto-reply-fix）：只读研究，**禁止**复制其代码与协议逆向产物。

## 待拍板

- [ ] 人工确认 Apache-2.0 / 其他
