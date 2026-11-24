# 参考实现调研与许可证审计（F001）

**日期**：2026-11-25
**方法**：GitHub 浅克隆（depth=1，只读，`refs/`，gitignore 不入库）+ LICENSE 文件实读 + 平台文档抓取。

## 1. 参考仓库固定 commit 与许可证

| 参考仓 | 固定 commit | Stars | 许可证（实读 LICENSE 首行） | 可否复用代码 |
|---|---|---|---|---|
| shaxiu/XianyuAutoAgent | `540bbc2` | 7327 | **GNU GPL v3** | 禁止复制（copyleft，会传染 Apache 产物） |
| GuDong2003/xianyu-auto-reply-fix | `6673f44` | 145 | **GNU AGPL v3** | 禁止复制（AGPL 与托管服务形态根本冲突） |
| dameng2026/xianyu-pilot-pro | `4ac9222` | 37 | **Apache-2.0** | 可借鉴设计思路（不含平台协议逆向产物） |
| shudaxiag/xianyu-guanjia | `24b93d3` | 130 | **MIT** | 可借鉴设计思路 |
| cv-cat/XianYuApis | `c6c87e6` | 209 | **无许可证文件** | **一律视为保留所有权利，不可复用**；仅作公开能力调研 |

**合规红线**：
- GPL/AGPL 系两仓（XianyuAutoAgent、xianyu-auto-reply-fix）只作只读研究，禁止复制代码与协议逆向产物。
- XianYuApis 无许可证 = 法律上「保留所有权利」，同上。
- 上游平台协议本身未开源，需申请授权（§3.1）。

## 2. 维护活跃度（截至 2026-11-25）

| 参考仓 | 最近提交 | 提交频率 | 活跃度 |
|---|---|---|---|
| xianyu-pilot-pro | 2026-11-24 | 2026-09-02 起 114 次提交 | **高**（近 3 个月 114 次） |
| xianyu-guanjia | 2026-11-07 | 2026-10-20 起 23 次提交 | **高**（近 2 个月 23 次） |
| xianyu-auto-reply-fix | 2026-10-26 | 近 30 天仅 2 次 | 中低 |
| XianyuAutoAgent | 2026-02-06 | 近 9 个月无提交 | **停滞** |
| XianYuApis | 2024-02-16 | 近 3 年无提交 | **停滞** |

> 活跃度≠可靠性。最活跃的 xianyu-pilot-pro（Apache-2.0）恰好是唯一可复用协议的；停滞的 XianyuAutoAgent stars 最多但已 9 个月不动，平台风控变更后极易失效。

## 3. 能力矩阵

每行 = 一个 Fishpond 需要的 Provider 能力；状态：`present` / `partial` / `absent` / `unverified`。

| Fishpond 能力 | 对应 Provider 能力 | XianyuAutoAgent (GPL) | auto-reply-fix (AGPL) | pilot-pro (Apache) | xianyu-guanjia (MIT) | 结论 |
|---|---|---|---|---|---|---|
| F004 provider-sdk manifest | provider 元信息声明 | unverified | unverified | unverified | unverified | **absent**（无仓有标准 manifest，须自研） |
| F009 账号管理 | 账号列表/状态上报 | partial | absent | absent | absent | 须自研 |
| F010 授权登录 | 扫码登录（含确认回调） | absent | absent | partial | absent | 须自研（pilot-pro 仅含滑块/短信，无扫码） |
| F011 会话协调 | 登录态持久化（cookie 保活） | partial | partial | partial | partial | 全部为静态 cookie 文件，**无会话生命周期管理**，须自研 |
| F012 Worker 注册/lease | 多账号并发租约 | absent | absent | absent | absent | 须自研 |
| F014 操作持久化/幂等 | 操作意图落库 + 幂等键 | absent | absent | absent | absent | 须自研（§11.4 的幂等模型全部参考仓都不具备） |
| F016 Capability Router | 能力声明式路由 | absent | absent | absent | absent | 须自研 |
| F017 事件 inbox/投影 | 聊天/订单事件流 | partial | partial | absent | absent | 须自研（参考仓无事件持久化，均内存态） |
| F021 网页执行 | Playwright 自动化 | present | present | present | partial | 可借鉴页面操作封装思路 |
| F022 Profile 管理 | 浏览器 profile 隔离 | partial | absent | absent | absent | 须自研（参考仓共享浏览器实例，无独占锁） |
| F027 监控/契约异常 | 契约测试 + 异常诊断 | absent | absent | partial | absent | 须自研（pilot-pro 有基础健康检查） |
| F032 官方 API 适配 | 闲鱼开放平台 API | absent | absent | absent | absent | **全 absent**（平台无公开 API 文档；`open.taobao.com` / `open.goofish.com` 需授权） |
| F033 网页操作适配 | 页面协议（协议逆向） | partial | partial | present | partial | 参考仓最集中的能力，但**协议未开源**，须自行调研并申请授权 |

**关键结论**：
1. **无任何参考仓提供 Fishpond 核心的「状态-操作-确认」三段分离、幂等执行、Profile 独占锁、lease 租约**——这些是本项目真正的价值增量，必须自研。
2. 参考仓能借鉴的只有两块：**Playwright 页面操作封装思路**（F021/F023）与 **xianyu-pilot-pro 的滑块/短信登录机制**（Apache-2.0，F010 授权面可参考其公开设计思路）。
3. **官方 API 全 absent**：`open.taobao.com`、`open.goofish.com` 均需授权访问，§3.1 的授权路径未打通 → F032/F033/F038 **BLOCKED**。

## 4. 平台文档可访问性

| 文档 | URL | 可访问性 |
|---|---|---|
| 闲鱼开放平台 | `https://open.goofish.com/` | 需授权 |
| 淘宝开放平台 | `https://open.taobao.com/` | 需授权 |
| 闲鱼三方开发平台 | `https://open.taobao.com/`（闲鱼类目） | 需授权 |

→ 全部 BLOCKED，须在获得授权前不得实现真实 API 对接（§7.3 第 2 条）。
