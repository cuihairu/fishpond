# 性能基线（F041，RC 面）

**日期**：2026-11-27
**方法**：全部在本机真库（Postgres + 强制行级安全）上复跑可重复的门禁测试与 CLI 压测；Mock Provider 在环。数字用于**回归对照**，不是容量认证——本机 debug 构建、单机库、无真实上游协议。

## 1. 会话调度面（runtime）

`cargo test -p fishpond-runtime --test f031_sessions`（1,000 actor）与 `cargo test -p fishpond-runtime --test f042_cluster`（10,000 actor，F042）

- 1,000 actor 全部投递恰一次、零重启：套件 0.03s；毒消息风暴 20 fatal 零蔓延。
- **10,000 actor × 10 条消息 = 100,000 次投递恰一次、零重启，加 200 fatal 毒风暴零蔓延：两用例合计 1.12s**（F042 集群面）。

## 2. 引擎面（worker，1,000 并发 Mock 发送）

`FISHPOND_TEST_DATABASE_URL=… cargo test -p fishpond-worker --test f031_sessions`（wall 5.9s 含编译）

- 1,000 并发 Mock 发送全成功、零熔断：用例本体 0.07s。
- browser 通道 50 并发踩踏：在 pending 上限处确定分流（2 在飞 + 2 排队放行、46 拒绝、门排空）。

## 3. 模拟稳定性与持续吞吐（F039 soak，72 模拟小时）

`fishpond soak --database-url … --tenant f041-perf-rc --sim-hours 72 --tick-ms 30000 --real-cap-secs 900 --seed 4141`

- **真实耗时 261.4s** 跑完 72 模拟小时（8,640 tick，约 30ms/tick），安全不变量全程零违例。
- 计数：assigns=11,615（另有 4,906 次因容量满被拒）→ 约 **44 放置/秒**（含每 tick 全量不变量复查）；migrated=1,433；drains=722；worker 死亡/复活=673/672；operations 5,841 claim + 5,841 close（成功/失败/超时混合）；contested=0（并发接管竞争零发生）。
- 每秒 sustain 的存储压力约数百条短事务（每 tick 约 15–20 条查询：心跳、复活、接管、放置、租约枚举、不变量复查）。

## 3b. 集群规模包络（F042，3 节点 × 4 worker 集群形状）

**10,000 账号**（`--accounts 10000 --capacity 850`，共 10,200 槽位，tick 60s，term 120s）：

- 真实时间上限（3600s）内完成 **5/60 tick**，violations=0；assigns=26,800/拒 3,200、drains=1、deaths=1、ops=2/2。
- **每 tick 实测约 860s、每次放置链路约 170ms**（对比 §3 的 1k 账号：每 tick 2.2s、每次放置约 9ms）——放置搜索随租约表行数**超线性**变贵：账号 ≥ 数千行后，RLS 策略求值 + 顺序扫描成为主导成本。
- 结论如实记录：10k 是当前放置搜索的**实测包络上界附近**；不变量路径本身是 O(1) 查询（每 tick 一次全量快照 + 内存聚合），不随规模变贵。优化方向在 scheduler 的候选扫描（F013 既有机制，其改动属独立任务），不在 soak harness。

**3,000 账号对照**（同集群形状，capacity 260，tick 60s，真实上限 2700s）：

- 上限内完成 **21/60 tick**，violations=0；assigns=30,900/拒 3,160、drains=1、deaths=1/revives=1、ops=12/12、unassigned=0、contested=0。
- 每 tick 实测约 **134s、每次放置链路约 44ms**。

**规模曲线（同一集群形状、同一 churn 模型）**：

| 账号数 | 每 tick 真实耗时 | 每次放置链路 | 违例 |
|---|---|---|---|
| 1,000（F039，tick 30s） | ~2.2s | ~9ms | 0 |
| 3,000（F042，tick 60s） | ~134s | ~44ms | 0 |
| 10,000（F042，tick 60s） | ~860s | ~170ms | 0 |

成本随账号数**超线性**增长（每 3× 账号，每放置成本 ×5–×19）：租约表到数千行后，放置搜索里的候选扫描（RLS 策略求值 + 顺序扫描）成为主导。两次 10k/3k 运行都被真实时间上限截断（上限机制按设计生效，截断在报告里显式声明，不伪装成完整视界）；所有完成的 tick 无一违例。

## 4. 已知边界（如实记录）

- Mock 引擎瞬时完成：压的是调度器/租约/操作/门控的真实路径，**不是真实上游时延**。
- debug 构建：release 构建的放置与引擎吞吐会显著更高；本基线的作用是同方法回归对照。
- 单机 Postgres（本机 unix socket 级别时延）；多节点部署面的坐标开销未单独剖析。
- 模拟时钟压缩了故障间隔：72h soak 证明的是**状态机与不变量在极端 churn 下不破**，不是墙钟时延分布。
- 未做：火焰图/剖析、release 容量认证、真实上游压测（红线：真实连接未授权）。

## 5. 复跑方法

```sh
# 1-3 全部可复跑；soak 需要先建租户（ soak 不代管供给）：
sudo -u postgres psql -d fishpond_test -c \
  "INSERT INTO tenants (id, name, created_at) VALUES ('f041-perf-rc','f041 perf rc',1700000000000)"
FISHPOND_DATABASE_URL=… fishpond soak --tenant f041-perf-rc --sim-hours 72 \
  --tick-ms 30000 --real-cap-secs 900 --seed 4141
```

同 seed 同参数 ⇒ 同计数序列（xorshift64\* 决定论）；真实耗时随机器浮动。
