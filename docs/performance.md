# 性能基线（F041，RC 面）

**日期**：2026-11-27
**方法**：全部在本机真库（Postgres + 强制行级安全）上复跑可重复的门禁测试与 CLI 压测；Mock Provider 在环。数字用于**回归对照**，不是容量认证——本机 debug 构建、单机库、无真实上游协议。

## 1. 会话调度面（runtime，1,000 actor）

`cargo test -p fishpond-runtime --test f031_sessions`

- 1,000 actor 全部投递恰一次、零重启：套件 0.03s。
- 毒消息风暴：20 个 fatal 会话永不重启、健康会话零丢失（同套件第二用例）。

## 2. 引擎面（worker，1,000 并发 Mock 发送）

`FISHPOND_TEST_DATABASE_URL=… cargo test -p fishpond-worker --test f031_sessions`（wall 5.9s 含编译）

- 1,000 并发 Mock 发送全成功、零熔断：用例本体 0.07s。
- browser 通道 50 并发踩踏：在 pending 上限处确定分流（2 在飞 + 2 排队放行、46 拒绝、门排空）。

## 3. 模拟稳定性与持续吞吐（F039 soak，72 模拟小时）

`fishpond soak --database-url … --tenant f041-perf-rc --sim-hours 72 --tick-ms 30000 --real-cap-secs 900 --seed 4141`

- **真实耗时 261.4s** 跑完 72 模拟小时（8,640 tick，约 30ms/tick），安全不变量全程零违例。
- 计数：assigns=11,615（另有 4,906 次因容量满被拒）→ 约 **44 放置/秒**（含每 tick 全量不变量复查）；migrated=1,433；drains=722；worker 死亡/复活=673/672；operations 5,841 claim + 5,841 close（成功/失败/超时混合）；contested=0（并发接管竞争零发生）。
- 每秒 sustain 的存储压力约数百条短事务（每 tick 约 15–20 条查询：心跳、复活、接管、放置、租约枚举、不变量复查）。

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
