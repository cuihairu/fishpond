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
- [x] F003 domain DTO、强类型 ID、错误及状态转换；非法转换单元测试 — DONE（2026-11-25）
  - Dependencies: F002（DONE）
  - Implementation files: crates/domain/src/{lib.rs, id.rs, error.rs, state.rs, dto.rs}（std-only：10 个强类型 ID 新类型 + 校验、AccountState/OperationState 转换表、DomainError、DTO 与不变量——UNKNOWN 必须带 note、确认态禁 note、消息文本非空、价格非负、attempt 序号从 1 起、事件 payload 必须是 JSON 对象、UnixMillis 拒绝负值）
  - Commands and actual results:
    - `cargo test -p fishpond-domain` → 26 单元 + 1 no_external_deps = 27 全 ok（非法转换：DISABLED 无出口×4、同态自转非法、PENDING 不能直接 SUCCEEDED/FAILED、终态×5 全拒绝；ID 校验：空/超长/坏字符/非 ASCII）
    - `cargo clippy -p fishpond-domain --all-targets -- -D warnings` → 零警告；`cargo fmt --all` 已过
  - Limitations / BLOCKED reasons: domain 保持零依赖（serde/时间库禁入），wire 层（REST/MCP）自行映射；DTO 与 F009M 的 MCP wire DTO 并存，整合属 F025。
  - Commit: fd498bf
- [x] F004 provider-sdk、manifest、结果语义、版本协商；mock 契约测试 — DONE（2026-11-25 批量令 F003-F008）
  - Dependencies: F003（DONE）
  - Implementation files: crates/provider-sdk/src/{lib.rs, capability.rs, version.rs, result.rs, manifest.rs, provider.rs, contract.rs} + tests/contract_with_stub.rs
    - capability：7 个能力枚举 + 规范 wire 名（accounts:read … events:stream）与 parse 往返
    - version：EngineKind、ProtocolVersion(major.minor) 解析/展示、compatible_floor（同 major 取低 minor）、negotiate（降序取最高共享 major + floor）、NegotiateError{NoOverlap, EmptyOffer}
    - result：ErrorClass 7 类 + retryable 语义（Transient/RateLimited/Timeout/Auth 可重试）、ProviderError{class, code, message, retry_after_ms}、Display 为 `class (code): message`
    - manifest：provider_id/display_name/provider_version/engine/protocol_versions/capabilities + validate（非空、版本号 x.y[.z] 且至少 x.y、≥1 版本、≥1 能力、禁重复能力）、supports、highest_protocol、content_digest（对版本/能力排序后求哈希 → 与声明顺序无关，内容漂移敏感）
    - provider：CallContext（write 校验非空幂等键 → Invalid empty_idempotency_key；read 无键）、SendMessageRequest、AuthorizeOutcome{session_ref, expires_at}、SendMessageOutcome{message, deduplicated}、`#[async_trait] Provider`（manifest / negotiate_protocol 默认实现把 NegotiateError 映射为 Unsupported protocol_negotiation_failed / authorize / send_message / list_conversations / list_messages / list_products）
    - contract：`run_contract_suite(&dyn Provider, &ContractFixture) -> ContractReport` 9 项不变量（manifest 合法、版本协商、授权后发送且首送非去重且出站且文本/会话回环、同键重放返回原 message id 且 deduplicated、历史按时间升序且只含本会话、会话列表含 fixture 会话、空文本 → Invalid 不可重试、未知会话 → Invalid 不可重试、未授权账号 → Auth 可重试）；CheckResult/ContractReport 收集式报告（assert_ok 列全失败项），供 F005 provider-mock 复用
  - Commands and actual results:
    - `cargo test -p fishpond-provider-sdk` → 14 单元 + 4 集成 = 18 全 ok（集成：诚实 stub 通过 9 项契约、故意损坏的去重实现被契约套件精确抓到 `["idempotent_replay"]`、V2-only offer 映射为 Unsupported(protocol_negotiation_failed) 不可重试、空白幂等键被拒）
    - `cargo clippy -p fishpond-provider-sdk --all-targets -- -D warnings` → 零警告；`cargo fmt --all --check` 通过
    - `cargo test --workspace` → 40 个 test suite 全 ok、零失败；`python3 scripts/check_workspace.py` → workspace structure ok: 15 members, layering rules satisfied
    - CI 工具链对拍：`rustup run 1.88.0 cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` 均过（1.88 的 uninlined_format_args 已前置规避）
  - Limitations / BLOCKED reasons: 契约套件仅覆盖 F004 已定义的读/写面（authorize/send/history/catalog）；真实 Provider 实现属 F032/F033（授权受限），CI 唯一目标是 F005 的 mock。manifest 的 content_digest 为 std DefaultHasher，非密码学摘要（F028 灰度比对用）。
  - Commit: 1e2f5f0
- [x] F005 Mock Platform 与 Mock Provider（授权/写入/断连/重复事件故障开关） — DONE（2026-11-25 批量令 F003-F008）
  - Dependencies: F004（DONE）
  - Implementation files: crates/provider-mock/src/{lib.rs, platform.rs, provider.rs} + crates/provider-mock/Cargo.toml + Cargo.lock
    - platform：MockPlatform（内部 Mutex 世界状态：accounts/conversations/messages/products/sessions/幂等表/事件日志 + 每租户事件序号 + 单调 mock 时钟）、四个独立故障开关 MockFault{AuthorizationDenied, WriteRejection, Disconnect, DuplicateNextEvent}（on/off 可切换、可查询）、seeded() 契约夹具（t-default / acc-mock / conv-mock-1 + 2 条入站历史 + 1 个商品）、所有调用面 session 门禁
    - provider：MockProvider 实现 `provider_sdk::Provider`（manifest：mock / api engine / 1.0+1.1 / 6 能力），authorize/send/list_conversations/list_messages/list_products 全部委托 platform；set_fault/fault/platform() 暴露故障注入与事件日志
    - 事件语义：成功发送才发 `message.sent` 事件（payload 为 JSON 对象 {conversationId, messageId, text}），envelope 携带每租户递增 seq 与平台级唯一 evt-N id；DuplicateNextEvent 一次性地把同一 envelope 重复投递两次并自清
  - Commands and actual results:
    - `cargo test -p fishpond-provider-mock` → 8 条全 ok（故障开关 on/off 往返、seeded 夹具形状、每租户事件序号 [1,1,2] 且平台 id 递增、契约套件 9 项全过、四个故障开关各一条聚焦测试）
    - 断连开关：5 个调用面（authorize/send/list_conversations/list_messages/list_products）全部 Timeout(connection_lost) 可重试；断连前成功发送 → 断连期间同键发送失败且不留状态 → 恢复后同键重放 `deduplicated=true` 且 message id 与断连前一致（幂等跨断连存活）
    - 写入故障：Transient(write_rejected) 可重试、`event_count()==0`（被拒写入不发事件）、同夹具下读路径不受影响、故障解除后同键首次成功发送仍 `deduplicated=false`
    - 重复事件：DuplicateNextEvent 下取走 2 条完全相等 envelope（同 id 同 seq）、开关自清；下一发送只发 1 条；按 id 去重后 2 条真实事件
    - 授权故障：Auth(authorize_fault) 可重试；解除后 authorize 返回 `mock-session-acc-mock`
    - 门禁：`cargo fmt --all --check` 通过、`cargo clippy --workspace --all-targets -- -D warnings` 零警告、`cargo test --workspace` 零失败、`python3 scripts/check_workspace.py` → workspace structure ok: 15 members, layering rules satisfied；CI 工具链对拍 `rustup run 1.88.0` fmt/clippy 均过
  - Limitations / BLOCKED reasons: mock 只做进程内模拟，不发起任何真实连接（§7.3 红线）；故障开关是测试夹具而非生产配置，运行时开关只服务 CI/本地演练。真实 provider 实现（F032/F033）仍待授权。
  - Commit: 3dc7522
- [x] F006 PostgreSQL migrations、tenant FK、索引、repository — DONE（2026-11-25 批量令 F003-F008）
  - Dependencies: F003（DONE）
  - Implementation files: crates/storage/Cargo.toml（+sqlx 0.8 `runtime-tokio,postgres,macros,migrate`、`default-features=false`；dev-dep tokio）/ crates/storage/migrations/0001_core.sql / crates/storage/src/{lib.rs, error.rs, store.rs} / crates/storage/tests/db_repositories.rs / Cargo.lock
    - migrations：`0001_core.sql` 建 tenants / accounts / conversations / messages / products 五表；所有表带 `tenant_id`；FK：accounts→tenants、conversations→(accounts,tenants)、messages→(conversations,tenants)、products→tenants；4 个索引 idx_accounts_tenant / idx_conversations_tenant_account / idx_messages_conversation_time / idx_products_tenant；时间列一律 BIGINT epoch ms
    - 迁移嵌入：`static MIGRATIONS = sqlx::migrate!("./migrations")`（编译期内嵌，CI 无 DB 也能编译；路径必须写成 `./migrations`，sqlx 0.9 宏拒绝无 parent 的相对路径）
    - Store：`connect(url)`（max_connections=5、acquire_timeout=5s、连上即跑 migrations）/ `run_migrations()`（可重入）/ `pool()`；`TenantRecord{id,name,created_at}`（domain 只有 id 无 Tenant DTO）
    - repository（全部运行期 `sqlx::query`，刻意不用编译期 `query!` 宏，保证 CI 无 `DATABASE_URL` 可编译）：upsert/get/list tenant；upsert/get/list/update_account_state account（按 tenant 过滤）；upsert/get/list_conversation（按 tenant+account、updated_at 倒序）；insert_message（`ON CONFLICT DO NOTHING RETURNING id` → bool，false=去重命中）/list_messages（`ORDER BY sent_at ASC, id`，按 tenant+conversation 过滤）；upsert/list_product（按 tenant、upsert 走 `DO UPDATE`）
    - StorageError{Database, Migration, NotFound{what,id}, Domain} + Display / `std::error::Error::source` / From<sqlx::Error>、From<MigrateError>、From<DomainError>；行→DTO 映射经 `domain::Account::new` 等构造器二次校验，脏行会显式报 DomainError 而非静默通过
  - Commands and actual results:
    - 真库验证（本机 Postgres 18.6，非超级用户角色 fishpond_test，专用库 fishpond_test）：`FISHPOND_TEST_DATABASE_URL=postgres://…@127.0.0.1:5432/fishpond_test cargo test -p fishpond-storage` → **7 条全 ok，连跑两轮（rerun 稳定，实体会话级唯一 id 避免 PK 冲突）**
    - 逐条覆盖：五表 + `_sqlx_migrations` 存在、`_sqlx_migrations` 显示 version 1 `core` success（二次 connect 幂等）、4 个索引齐备、tenant upsert→改名单值一致 + NotFound{what:"tenant", id:"t-no-such-tenant"}、账号跨租户列表互不可见（A 只见 A、B 只见 B）、ONLINE→UNKNOWN 带 note 往返一致、会话按 tenant+account 过滤且 updated_at 倒序、消息乱序插入 → 读回 oldest-first 且重复插入返回 `false` 且行数仍为 3、商品跨租户隔离 + upsert 原地更新
    - 无 DB 降级：不设 `FISHPOND_TEST_DATABASE_URL` 时 6 条真库测试打印 `skip: FISHPOND_TEST_DATABASE_URL is not set (CI has no database)` 并正常通过（CI 无库，§7.3 Mock 是唯一 CI 目标）；`unreachable_database_surfaces_a_database_error` 不依赖库（端口 1 拒连 → `StorageError::Database`），实测 5s 内返回
    - 门禁：`cargo fmt --all --check` 通过、`cargo clippy --workspace --all-targets -- -D warnings` 零警告、`cargo test --workspace` 零失败、`python3 scripts/check_workspace.py` → workspace structure ok: 15 members, layering rules satisfied；CI 工具链对拍 `rustup run 1.88.0 cargo clippy -p fishpond-storage --all-targets -- -D warnings` 零警告
  - Limitations / BLOCKED reasons: RLS 属 F007（本任务只保证查询形状上按 tenant 过滤）；messages 自持 `tenant_id` 列是为 RLS/FK 方便，写入时由调用方显式给（`Message` 无 tenant 字段）；测试数据不自动清理（靠每轮唯一 id 规避冲突），本地库仅用于测试，密码只出现在环境变量不进代码/日志。
  - Commit: eef1d57
- [x] F007 租户与 API key、scope、RBAC、RLS 策略及跨租户测试 — DONE（2026-11-25 批量令 F003-F008）
  - Dependencies: F006（DONE）
  - Implementation files: crates/storage/migrations/0002_tenant_keys_and_rls.sql / crates/storage/src/{authz.rs, error.rs, store.rs, lib.rs} / crates/storage/Cargo.toml（+sha2 0.10）/ crates/storage/tests/{tenant_isolation.rs, db_repositories.rs, common/mod.rs} / Cargo.lock
    - 迁移 0002：tenant_roles（RBAC 角色=scope 捆绑，种子 owner/operator/viewer）+ api_keys（key_hash 主键=SHA-256 hex、key_hint 尾 4 字符、tenant FK、role FK、scopes TEXT[] 发签时冻结、created/expires/revoked/revoked_at）；tenant-owned 五表全部 ENABLE + FORCE ROW LEVEL SECURITY，每表一条 `USING (tenant_id = current_setting('app.tenant', true)) WITH CHECK (…)` ALL 策略；未绑定会话读零行写零行（fail closed）
    - Store::begin_tenant(tenant)：事务级 `set_config('app.tenant', …, TRUE)` 绑定（事务结束自动失效，连接池下不泄漏租户）；全部 repository 方法改为在租户绑定事务内执行，隔离由 `WHERE tenant_id` **和** RLS 双层保证；get_account/get_conversation/update_account_state 增加必填 tenant 参数（裸 id 是他租户可达的越权面，RLS 下本就该按租户寻址）；pool() 文档明确未绑定访问对租户表零可见
    - authz：hash_api_key（SHA-256，高熵随机 key 免盐）、key_hint 只存尾 4 字符、upsert_role/list_roles、issue_api_key（冻结 role scope；role 不存在 → NotFound）、resolve_api_key（revoked/expired/unknown 一律 None，不构成 oracle）、revoke_api_key（幂等，二次返回 false）、require_scope（缺 scope → PermissionDenied{tenant, role, scope}，报错永不回显 key 原文）；scope 词汇与 crates/mcp/src/auth.rs 一致（accounts:read … events:read），ApiKeyRecord{tenant_id, scopes} 语义对应 M0 内存 key store
  - Commands and actual results:
    - 真库验证（本机 Postgres 18.6，非超级用户 fishpond_test——超级用户绕过 RLS，会让隔离断言全空转）：`FISHPOND_TEST_DATABASE_URL=postgres://fishpond_test:fishpond_test@127.0.0.1:5432/fishpond_test cargo test -p fishpond-storage` → **15 条全 ok（3 单元 + 7 F006 回归 + 5 F007 验收），连跑两轮 rerun 稳定**
    - F007 验收逐条：RBAC 种子词汇（owner/operator 全 6 scope、viewer 无 conversations:write 且 5 scope）、role upsert 幂等、key 发签冻结 role scope（事后放宽 role 不影响已发 key）、viewer 缺写 scope → PermissionDenied 且错误串/存储记录均不含 key 原文、自定义角色可发签、revoked/expired/unknown/blank 一律不解析、撤销幂等、list 回 hash 不回原文
    - RLS 跨租户（故意在绑定事务内跑**无 WHERE** 的 SQL，让数据库而非查询形状做隔离）：A 绑定会话 SELECT accounts/tenants/conversations/messages/products 只见 A 自己的行；B 同理；未绑定 pool 对五表读零行；未绑定 INSERT 被 WITH CHECK 拒绝；A 绑定下无 WHERE 的 UPDATE 只命中 A 的行（rows_affected=1）且 B 行平台字段未被污染；A 绑定 INSERT 伪装 tenant_id=B 被 WITH CHECK 拒绝；A 绑定 DELETE 只删 A 行、B 数据完好
    - 无 DB 降级（CI）：不设环境变量时 12 条真库测试全部走 skip 分支正常通过（§7.3 Mock 是唯一 CI 目标），unreachable_database_surfaces_a_database_error 不依赖库仍验证错误面
    - 门禁：`cargo fmt --all --check` 通过；`cargo clippy --workspace --all-targets -- -D warnings` 零警告；`cargo test --workspace` → 45 个 test suite 全 ok、100 passed、0 failed；`python3 scripts/check_workspace.py` → workspace structure ok: 15 members, layering rules satisfied；CI 工具链对拍 `rustup run 1.88.0` fmt/clippy 全过
  - Limitations / BLOCKED reasons:
    - 初版迁移漏写 `ENABLE ROW LEVEL SECURITY`（只写了 FORCE，策略实际休眠）——本机真库测试抓到后已修（0002 尚未提交前即修正，未发布过任何错误迁移；本地测试库按 checksum 变更手工重放了 0002）。
    - tenant_roles/api_keys 不带 RLS：key 必须在知道租户前可解析，且表内只有 hash 与公开元数据；运维/迁移操作走超级用户（按设计绕过 RLS）。
    - messages 无 tenant 字段的写入仍由调用方显式给 tenant（沿 F006 决策）；跨租户写测试证明即使调用方谎报 tenant_id，WITH CHECK 也会拒绝。
    - 角色种子是迁移内固定词汇；运行时改角色 scope 只影响新发 key（冻结语义），不追溯。
  - Commit: 580fe97
- [ ] F008 Credential Store 信封加密、轮换、撤销；秘密不进日志测试
  - Dependencies: F006
  - Commit:
- [x] F009M MCP Server 面：crates/mcp（rmcp 3.11，stdio+streamable-http）、10 工具、API key/scope 鉴权、CLI `fishpond mcp`、快照/往返/鉴权/脱敏测试 — DONE（2026-11-25，用户令「fishpond 需要补上一个 mcp 功能」）
  - Dependencies: F002（DONE）
  - Implementation files: Cargo.toml（members +crates/mcp）/ Cargo.lock / crates/mcp/Cargo.toml / crates/mcp/src/{lib,auth,redact,services,mock,tools,server,cli}.rs / crates/mcp/tests/{common/mod.rs,tools_snapshot.rs,tools_roundtrip.rs,auth_isolation.rs,idempotency.rs,secret_redaction.rs} / crates/mcp/tests/snapshot/tools_list.json（golden 快照）/ crates/cli/{Cargo.toml,src/main.rs}（二进制命名 fishpond + `mcp` 子命令分派）/ docs/adr/0002-mcp.md / README.md（MCP 接入节）/ scripts/check_workspace.py（+fishpond-mcp 成员与内部依赖白名单）
  - Commands and actual results:
    - `python3 scripts/check_workspace.py` → `workspace structure ok: 15 members, layering rules satisfied`
    - `cargo fmt --all -- --check` → OK（0 diff；另在 CI 基线 rustc 1.88.0 下复核同样 0 diff）
    - `cargo clippy --workspace --all-targets -- -D warnings` → 零警告；**另在 rustc/clippy 1.88.0 下复跑同样零警告**（CI 首轮在 1.88 上命中 `uninlined_format_args`，已在 eba0944 修掉）
    - `cargo test --workspace` → 39 个 target 全 `test result: ok`；fishpond-mcp 单独：lib 12 + auth_isolation 6 + idempotency 1 + secret_redaction 1 + tools_roundtrip 10 + tools_snapshot 2 = 32 条
    - 端到端冒烟（真进程，非测试替身）：`fishpond mcp` stdio 下 initialize→tools/call `list_accounts` 返回 3 条 t-default 账号；`get_account acc-102` 返回 `UNKNOWN` + stateNote；`--transport http` 起 `POST /mcp`，`x-fishpond-api-key` 与 `Authorization: Bearer` 两种头都通过，缺头返回 `-32600 unauthenticated`，跨租户 key 只见 t-other 数据
    - 远端 CI（run 45523507628，headSha eba0944）：`structure (§3.2 members / §3.3 layering)` success；`rust (ubuntu-latest, 1.88)` success；`rust (macos-latest, 1.88)` success；`rust (windows-latest, 1.88)` success；`node (lint/test/build)` failure
    - node job 的 failure 与本单无关且先前已存在：dependabot 合并提交 dfffb51 的 CI（run 45508619587）同样失败，同一错误 `ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION`——vitest 3.2.7→4.1.11 拉入 `tinyexec@1.4.0`（发布于 2026-11-24T22:03:38Z），落在 pnpm 12 默认最小发布龄策略窗口内。本单未改动任何 Node 文件。
  - Limitations / BLOCKED reasons:
    - `node (lint/test/build)` 在 headSha eba0944 上仍为 failure，原因为上述既有依赖策略冲突（pnpm 12 默认 minimumReleaseAge 拒绝 24h 内发布的 tinyexec@1.4.0），与本单改动无关，预计 2026-11-25T22:03Z 后自愈；处理它属于依赖策略决策（回退 dependabot 升级 / 显式放宽策略），未擅自改动。
    - M0 范围限制：`subscribe_events` 只返回事件快照，live 推送随 F017 落地后桥接为 MCP notification；`send_message` 只走 Mock provider（F032 BLOCKED 不受影响）；未实现 browser/Profile 工具（F021/F022 未落地）。
    - M0 鉴权用 `ApiKeyStore` 内存种子（CLI 从 `FISHPOND_API_KEY`/`FISHPOND_TENANT_ID`/`FISHPOND_SCOPES` bootstrap，默认租户 t-default），F007/F025 落地后替换为持久化 key store，`AuthContext` 形状保持不变。
    - 结构规则：`fishpond-mcp` 只允许内部依赖 domain/storage/server，禁止 worker/browser/provider 实现，已由 `scripts/check_workspace.py` 钉住（CI structure job 覆盖）。
  - Commit: 9597d49（mcp facade crate + 测试 + ADR-0002 + README + 结构规则）；eba0944（1.88 clippy 修复）

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
