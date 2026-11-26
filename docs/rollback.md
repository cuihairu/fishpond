# 回滚手册（F041，RC 面）

**日期**：2026-11-27。范围：compose 部署、数据库、profile 迁移、操作面、发布面。
原则：**数据库 schema 只前进**（sqlx 迁移无内建 down），所以「回滚」= 回到备份 + 旧二进制，其余路径都是在途可逆操作。

## 1. 服务回滚（二进制/镜像）

- compose 栈：回滚 = 重新部署上一个镜像 tag（`docker compose -f deploy/docker-compose.yml up -d` 指向旧 digest）。先停 worker（或先 drain 节点，见 §4），再动 postgres/migrate 服务。
- 每日构建：nightly 滚动二进制；出问题换回上一个 nightly 产物即可（无版本号 release，符合发布红线）。
- **红线**：禁止正式版本发布与版本号 tag；回滚永远只在 nightly/镜像层发生。

## 2. 数据库回滚

1. **先备份再升级**（任何二进制变更前跑一次）：
   `deploy/backup.sh --database-url <超级用户 url> <备份目录>`（FORCE RLS 烤在迁移里 → app 角色无法 dump 自己的库，备份角色必须绕 RLS，见 deploy/README）。
2. 升级引入新迁移后需要回退数据：`deploy/restore-verify.sh` 把 dump 恢复进 scratch 库并用 `fishpond doctor` 验证，验证过的 dump 是回滚素材；真回滚 = 停服务 → 重建库 → 恢复 dump → 起旧二进制。
3. 迁移本身**不做 down**：旧二进制遇新 schema 以 doctor 的 schema_current 检查为准，不匹配即拒绝服务（fail-closed），这是防止旧代码写坏新表的守门。

## 3. Profile 迁移回滚（F037）

- `ProfileManager::migrate` 每步先写**已验证快照**（`<backup_root>/<profile_id>/<unix_ms>`，含 manifest digest），目的地字节级复核后才换行；换行失败/后续失败时旧树保留——**快照 + 旧树就是回滚工件**，恢复 = 从快照 materialize 回原路径 + `move_profile` 行交换回来。
- 已知残留语义：目的地物化成功但行交换失败 → 目的地有孤儿树（安全方向：多了不缺）；旧树删除失败 → 迁移已提交，孤儿旧树由运维清理。

## 4. 操作面回滚（全部在途可逆）

| 操作 | 回滚 |
|---|---|
| 节点 drain | `undrain_worker` 逐 worker 恢复（容量回归，放置自动补位） |
| worker 死亡/误杀 | 心跳恢复 + `take_over_lost` 自动把租约迁回（代数递增围栏） |
| purge 清扫 | 先 `--dry-run`（rollback 式预演报精确将删数）；事件日志永不清扫，审计底账不丢 |
| 凭据/账号撤销 | revoke 幂等（重放返回原始时刻）；账号删除是终局——靠 §2 备份恢复 |
| soak 压测 | 用独立租户跑（不碰业务租户）；跑完即静态数据，无需回滚 |

## 5. 验证回滚可用性的演练记录

- F039-C（2026-11-27）：真库 936KB custom dump → scratch 库恢复 → `fishpond doctor` healthy（reachable + schema current）→ scratch 清理。命令与输出见 TODO.md F039 行与 deploy/README。
- doctor 三检查（url 来源 / 连通 / 迁移齐平）是任何恢复动作后的验收门。

## 6. 不回滚的东西

- 审计事件日志（events 表）：append-only 底账，任何回滚都不删它——回滚后的世界与底账的差异靠新事件表达，不改历史。
- `fishpond report` 的容量视图、SBOM（sbom/，`scripts/sbom.sh` 重生成）：只读面，无回滚语义。
