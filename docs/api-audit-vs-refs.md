# 出站 API 对照审查（FISHPOND-API-AUDIT-002）

**日期**：2026-11-28
**方法**：静态只读对照。逐 fishpond 出站调用点 → refs/ 对应参照实现 → 逐 API 五问（端点/headers/请求体/响应解析/频控退避）→ 一致/偏差/缺失，全部判定带证据文件:行指针。
**证据缩写**：`A/` = refs/XianyuAutoAgent（GPL-3.0）；`ar-f/` = refs/xianyu-auto-reply-fix（AGPL-3.0）；`GJ/` = refs/xianyu-guanjia（MIT）；`GP/` = refs/xianyu-pilot-pro（Apache-2.0）；`Y/` = refs/XianYuApis（无许可证）。GPL/AGPL/无许可证三仓**只读研究，禁止复制代码与协议逆向产物**（docs/reference-audit.md F001 定）；本次仅记录公开协议行为面。

**纪律**：只审不改。HIGH/MED 项列设计缺口清单（§4），实现须待授权红线解除（F032/F033 BLOCKED），修复另派单。

---

## §1 审查结论总览

### 1.1 fishpond 真实出站调用点：零（红线完好）

| 审计证据 | 说明 |
|---|---|
| `crates/provider-official/src/lib.rs` 仅 1 行 doc 注释（"gated on explicit authorization, disabled by default"） | F032 未动工，官方 API 面为零 |
| 全仓 grep `goofish\|h5api\|mtop\|idlemessage\|wss://` → 仅命中 `refs/` 与 `docs/reference-audit.md` | 代码零破口 |
| 全仓无 cookie/token 形常量（`_m_h5_tk`/`unb`/`sk_` 等） | 无硬编码凭据 |
| `telemetry/src/redact.rs`：`mask_url_userinfo / mask_bearer / mask_key_values` 统一脱敏层 | 日志/指标不泄凭据 |
| `browser-worker/src/remote-page-provider.ts`（F044）仅连操作员配置受控 origin（构造期校验 http/https、禁内嵌凭据、target 单路径段） | 唯一出站 fetch 面受控 |
| `events/src/ssrf.rs` webhook 出口 fail-closed 校验（内网/环回/链路本地/元数据地址全拒） | 出站 webhook 面受控 |

### 1.2 审查产出

真实调用点为零 → 本审查输出三条：

1. **平台协议行为矩阵**（§3）：五参照仓实测端点/headers/请求体/响应解析/频控，作为 F032/F033 设计输入
2. **fishpond 语义面差距清单**（§2.3 + §4）：mock 模型 vs 平台实测行为，标 HIGH/MED
3. **参照间矛盾裁定**（§4.1）：以多数/最新为准

---

## §2 fishpond 平台语义面（对照基准）

### 2.1 Provider trait（crates/provider-sdk/src/provider.rs:92）

| 方法 | 语义 | 幂等 |
|---|---|---|
| `authorize(ctx)` | 建会话 → session_ref + expires_at（provider.rs:110） | 每 account 幂等 |
| `send_message(ctx, {conversation, text})` | 发消息（:113）；空文本拒（provider.rs:66-68） | `(tenant, account, idempotency_key)` dedup |
| `list_conversations(ctx)` | 列对话，租户+账号过滤（:120） | 读 |
| `list_messages(ctx, conversation)` | 列消息，conversation 归属校验（:123） | 读 |
| `list_products(ctx)` | 列商品，按租户过滤（:130） | 读 |

错误类（provider-sdk/result.rs）→ 路由决策（router/src/classify.rs:35）：Invalid/Unsupported=CallerFault 不计熔断；Transient/RateLimited/Timeout/Auth/Internal=UpstreamFault 计熔断。`RateLimited` 自带 `retry_after_ms`（result.rs:85）。

### 2.2 出站保护面（已有机制）

| 组件 | 策略 | 位置 |
|---|---|---|
| 熔断 | failure_threshold + cooldown_ms，冷却后单探测并带 retry_after 提示 | router/src/breaker.rs:30-211 |
| 重启退避 | 线性 `backoff × attempt` 封顶 max_backoff | runtime/src/supervisor.rs:27-58 |
| 操作重试 | attempt 截止期模型：到期 attempt 记 TimedOut，操作回 PENDING 等新 attempt（无次数上限的无限重试面，CAS 防竞争） | operations/src/reconcile.rs:15-20, 207 |
| webhook 出口 | fail-closed SSRF 校验 | events/src/ssrf.rs:139-193 |

### 2.3 mock 语义 vs 平台实测（差距总表）

| 面 | fishpond/mock 现状 | 平台实测（五参照汇总） | 判定 |
|---|---|---|---|
| 会话生命周期 | authorize 1h TTL，无自动续期（platform.rs:30） | token 刷新节奏：A 3600s（A:main.py:39）；ar-f/GJ 72000s + 60s 去重（config.py:97，ar-f:7387）；GP 分层——keepalive 30min / `_m_h5_tk` 随机 1.5-2.5h / WS token 随机 10-14h（GP:cookie_token_refresher.py:48-57）；ar-f 另有 `loginuser.get` keepalive（ar-f:7425） | **HIGH 缺口**：须实现过期感知+自动刷新+分层节奏 |
| 消息幂等 | `(tenant,account,key)` dedup（platform.rs:339-358） | 平台无统一幂等键；**但**确认发货有幂等语义（`ORDER_ALREADY_DELIVERY` 视为成功，GP:xianyu_api_service.py:1039-1102）；GP 客户端 send dedup TTL 60s 容量 500（GP:ws_client.py:42-74） | **方向一致**：fishpond 幂等键是 GP send-dedup 的超集；发货面幂等须按平台语义特判 |
| 错误分类 | 固定 error code（`authorize_fault`/`write_rejected`/`connection_lost` 等） | 平台 ret 字符串：成功 `SUCCESS`/`SUCCESS::调用成功`；令牌 `TOKEN_EXPIRED`/`令牌过期`/`FAIL_SYS_SESSION_EXPIRED`；风控 `RGV587_ERROR`/`被挤爆啦`/`punish`/`FAIL_SYS_USER_VALIDATE`→需验证码；GP 映射成 CAPTCHA_NEEDED/XIANYU_API_REJECTED/RiskControl/AuthExpired（GP:406-432, :200-247） | **HIGH 缺口**：ret→ErrorClass 映射表须自研（RGV587→RateLimited+retry_after、SESSION_EXPIRED→Auth、USER_VALIDATE→新验证码分支） |
| WS 事件流 | 无 WS 面；Capability::StreamEvents 已声明但 mock 为进程内 event log | `wss://wss-goofish.dingtalk.com/` lwp 协议：`/reg`→`/r/SyncStatus/ackDiff`→心跳 `/!`（15s 全参照一致）→入站 syncPushPackage（数字键结构 + MessagePack/AES 解码）→ACK `{code:200, headers:{mid,sid}}`（A:168-189, ar-f:13490-13544, GJ:6115-6169, GP:ws_protocol.py:1220-1301） | **HIGH 缺口**：StreamEvents 实时面未实现 |
| 消息模型 | `Message{text, direction, sent_at}` | 入站数字键 `message["1"]["5"]`=createTime、`["1"]["10"]`=reminder（itemId 从 reminderUrl 提取）、`["1"]["2"]`=cid、`["3"]`=订单态（A:384-451, GP:ws_protocol.py:699-909）；出站 `contentType` 1=text/2=image/8=商品会话/11=通知/26=卡片更新（GP:415-635） | **HIGH 缺口**：Message domain 须扩展类型/附件/订单事件维度 |
| 发消息 payload | `{conversation, text}` | WS send 多层嵌套：`uuid/cid=<chat>@goofish/conversationType/contentType:101+custom{type,data=base64(json)}/actualReceivers=[对端@goofish, 自己@goofish]`；**actualReceivers 缺自己→code=500**（GP:ws_protocol.py:1309-1373） | **HIGH 缺口**：须构造完整 lwp payload（含自身 receiver） |
| create_conversation | trait 无此方法 | `/r/SingleChatConversation/create`（Y:104-124, ar-f:13395, GJ:5694, GP） | **HIGH 缺口** |
| 历史消息分页 | `list_messages` 无游标 | `/r/MessageManager/listUserMessages` + `hasMore/nextCursor`（Y:80-97）；GP `/r/Conversation/listNewestPagination`（GP:803） | **HIGH 缺口**：cursor 分页 |
| 频控/退避 | 熔断+线性退避，无平台校准值 | 参照谱系：A 固定 0.5s 重试≤2-4；ar-f 重连 `min(3n,15)/min(10n,60)/min(5n,30)`、连续 5 败熔断重登；GP 最成熟——重连指数 5s→300s ×2、发货重试≤4、页间 sleep U(0.5,1.5)、RGV587 IP 风控熔断自适应 300/1800/3600s、验证码solver 5min 抑制 | **MED 缺口**：fishpond 策略骨架更完整但缺平台校准参数 |
| 登录态保持 | authorize 任意续期 | QR 轮询 3s/120s（Y）或 0.8s/300s（GP:xianyu_qr_login.py）；hasLogin.do 保活 + Set-Cookie 合并策略（风险响应丢 Set-Cookie，GJ:1602-1646；ar-f protected-merge + PROTECTED_SESSION_COOKIE_FIELDS ar-f:7054-7220） | **MED 缺口**：cookie 合并/保护策略未建模 |
| webhook 出口 | fail-closed SSRF（validate_endpoint） | GP 同样 fail-closed：`PinnedHttpsTarget` DNS 钉扎 + 对端验证 + 拒重定向（GP:outbound_network.py:25-58） | **一致（设计收敛）**：跨项目独立收敛到同一防线，fishpond 方向正确 |

---

## §3 参照矩阵：逐 API 家族 × 五问

### 3.1 Token / 会话保持

| API | fishpond | 参照（证据） | 五问要点 |
|---|---|---|---|
| `POST h5api.m.goofish.com/h5/mtop.taobao.idlemessage.pc.login.token/1.0/` | 未实现（stub） | 全五仓一致（A:XianyuApis.py:195；Y:335；ar-f:7614；GJ:1492；GP:ws_token.py:544） | ①端点一致 ②headers：`content-type: application/x-www-form-urlencoded` + cookie + Chrome UA（A:172-187，GP:571-595 全 client-hint）③body `{"appKey":"444e9908a51d1cb236a27862abc769c9","deviceId":<id>}`（GP 另加 `appName:"xianyu", ttid:"pc_xianyu"` :755-760）④响应取 `data.accessToken`（ar-f:7665；GJ:1651）+ Set-Cookie 合并 ⑤重试：令牌过期→刷新→单次重试（GP:_post_mtop_with_token_retry:775-812；GJ 内联≤2 次 :9666-9698） |
| `POST .../loginuser.get/1.0/` | 未实现 | ar-f keepalive（:7425-7497，60s 轮询+夜间窗）；GP（:2770）；Y（:388） | body `{}`；轻量保活 |
| `POST passport.goofish.com/newlogin/hasLogin.do` | 未实现 | A:122（重登路径）；GP:cookie_token_refresher.py:473-641（30min 保活） | form：`hid=<unb>, _csrf_token=<XSRF-TOKEN>, hsiz=<cookie2>, deviceId=<cna>, appName=xianyu, fromSite=77`；响应 `data.loginResult=="success"`；失效→cookie_status=0 + WS 停 |
| `mtop.taobao.idle.user.hasLogin/1.0/` | 未实现 | GP:561-572（mtop 版登录态查询） | — |
| `_m_h5_tk` 三步引导 | 未实现 | GP:refresh_m_h5_tk:1042-1164（GET → 空 token POST → 真 token POST @ `mtop.gaia.nodejs.gaia.idle.data.gw.v2.index.get`）；Y:build_cookies.py:63-78 同型 | 空 sign 请求诱发发 token |

### 3.2 QR 登录

| API | fishpond | 参照 | 五问要点 |
|---|---|---|---|
| `GET passport.goofish.com/mini_login.htm` | 未实现 | Y:117；ar-f/GJ qr_login.py:35/32；GP:33 | 收 XSRF-TOKEN/_tb_token_ |
| `GET .../newlogin/qrcode/generate.do` | 未实现 | Y:143；GP:34 | 读 `content.data.codeContent/t/ck` |
| `POST .../newlogin/qrcode/query.do` | 未实现 | Y:209-216；GP:35 | 轮询 `qrCodeStatus` NEW/SCANNED/CONFIRMED/EXPIRED；节奏 3s/120s（Y:96）或 0.8s/300s（GP） |
| `POST .../login_token/login.do` | 未实现 | Y:246；ar-f | `token, subFlow=DIALOG_CHECK_LOGIN_RPC, nextCode=0018, confirm=true` |

### 3.3 商品 / 发布 / 上传

| API | fishpond | 参照 | 五问要点 |
|---|---|---|---|
| `mtop.taobao.idle.pc.detail/1.0/` | 未实现 | A:291；Y:454；ar-f:10018；GJ:2693+；GP:xianyu_goods_sync.py:1256（全五仓） | body `{"itemId":...}`（GP 加 `userId`）；响应 `data.itemDO`（A:496）；重试 A≤3/ar-f≤4/GJ≤2（跳过 VALIDATE/RGV587） |
| `mtop.idle.web.xyh.item.list/1.0/` | 未实现（对应 list_products） | ar-f:18207；GJ:9308；GP:1201 | body `{pageNumber, pageSize, needGroupInfo, userId}`；解析 `data.cardList[].cardData`（id/title/priceInfo/itemStatus/picInfo）；**GP 页间 sleep U(0.5,1.5)** |
| `mtop.taobao.idle.item.polish/1.0/`（+`mtop.idle.item.polish` 备用） | 未实现 | ar-f:79/:121；GJ:10104（`IDLEITEM_POLISH_AGAIN` 视为成功） | 擦亮商品 |
| `mtop.taobao.idle.item.downshelf/2.0/` | 未实现 | GJ:9519；GP（另有 `mtop.alibaba.idle.seller.pc.item.offline`） | 下架；token ret→刷新重试≤3 |
| `mtop.alibaba.idle.seller.pc.item.delete/info.update` | 未实现 | GP:xianyu_goods_sync.py:1900+ | 卖家台头（seller.goofish.com + `idle_site_biz_code: COMMONPRO`） |
| `mtop.idle.pc.idleitem.publish/1.0/` | 未实现 | Y:744；GJ:11958；GP:2422 | 最重 payload：itemTextDTO/itemPriceDTO/itemCatDTO/itemAddrDTO/imageInfoDOList/itemSkuList/uniqueCode/sourceId/publishScene=pcMainPublish；**GP 双层序列化**（内层 JSON 字符串再签名，fish_shop_publish.py:524-536） |
| `mtop.idle.pc.backend.idleitem.publish/edit/editdetail/1.0` | 未实现 | GP:fish_shop_publish.py:40-53 | 鱼塘 PC 后台版发布三件套 |
| `mtop.taobao.idle.kgraph.property.recommend/2.0/` | 未实现 | Y:526；GP:auto_category.py:228-310（GJ 用 1.0 版:11699） | 类目预测；`imageInfos[]`+`uniqueCode`；解析 `categoryPredictResult` |
| `mtop.taobao.idle.local.poi.get/1.0/` | 未实现 | Y:573；ar-f:696；GJ:11598 | 经纬度→默认发货地址 |
| `POST stream-upload.goofish.com/api/upload.api?floderId=0&appkey=xy_chat` | 未实现 | Y:426；ar-f/GJ image_uploader.py:20；GP:auto_category.py:28-34 | multipart `file`；读 `object.url`/`object.pix`；唯一非 h5api 主机 |
| `mtop.taobao.idlemtopsearch.pc.search/1.0/` | 未实现 | GJ:item_search.py:945；GP:332；GP 爬虫面走浏览器拦截（goofishSearch.ts:38） | 搜索 |

### 3.4 订单 / 物流 / 退款

| API | fishpond | 参照 | 五问要点 |
|---|---|---|---|
| `mtop.taobao.idle.trade.merchant.sold.get/1.0/` | 未实现 | ar-f:order_history_sync.py:13；GP:575-599 | 卖家订单列表；GP data `{pageNumber, rowsPerPage, queryCode:"ALL", ...}`，解析 `nextPage`（"true" 字符串）/`totalCount`/`lastEndRow` |
| 订单列表名探测 | 未实现 | GJ:xy_order_sync.py:18-26（6 卖家+5 买家候选名轮询，记住赢家） | **平台 API 名不稳定**——多候选探测是现行做法 |
| `mtop.idle.web.trade.order.detail/1.0/` | 未实现 | ar-f/GJ 走 **Playwright 网络嗅探**（ar-f:order_detail_fetcher.py:703；GJ:155），非直连 POST | 页面 `goofish.com/order-detail?orderId=...&role=seller` 拦 XHR |
| `mtop.taobao.idle.logistic.consign.dummy/1.0/` | 未实现 | ar-f:161；GJ:112；GP:1039-1102 | 确认发货 `{orderId, tradeText:"", picList:[], newUnconsign:true}`；重试≤4；**`ORDER_ALREADY_DELIVERY` 幂等成功** |
| `mtop.idle.groupon.activity.seller.freeshipping/1.0/` | 未实现 | ar-f:117；GJ:58；GP:1118-1246 | 议价免运费；`bizOrderId/itemId/buyerId` 去 `@goofish` 后缀；GP 有不重试错误清单 |
| `mtop.taobao.idle.merchant.refund.*`（list/agree.refund/service.record/detail）+ `trade.merchant.full.info` | 未实现 | GP:refund_service.py:50-57 | 退款面（queryType:"refund"，解析 items/nextPage/lastEndRow） |
| 改价（4-5 个候选 API 名） | 未实现 | GJ:10827-10867（adjustprice 两 payload + adjust.price + adjust.order.price + modifyprice 顺序试，首个 SUCCESS 定） | 同上——API 名不稳定 |

### 3.5 IM / 消息面

| API | fishpond | 参照 | 五问要点 |
|---|---|---|---|
| `mtop.taobao.idlemessage.pc.user.query/4.0` | 未实现 | ar-f:15896；GP:442-558 | 会话对方用户信息；GP 加 `spm_cnt=a21ybx.im.0.0` + 全 client-hint |
| `mtop.idle.trade.pc.message.headinfo/1.0` | 未实现 | ar-f:15910 | `{itemId, sessionId, sessionType}` |
| `mtop.taobao.idlemessage.pc.blacklist.query/1.0` | 未实现 | ar-f:15929 | 拉黑查询 |
| `mtop.taobao.idlemessage.pc.session.sync/3.0/` | 未实现 | GJ:xy_chat_sync.py:91（fetch_num=50） | HTTP 会话同步（WS 之外的对账面） |
| `mtop.taobao.idle.rate.create/4.0` + `merchant.rate.list/1.0` | 未实现 | ar-f:rate_service.py:22-25；GP:59-61 | 评价面；seller.goofish.com referer |

### 3.6 WebSocket 通道（五参照一致，细节有差）

| 面 | fishpond | 参照 | 要点 |
|---|---|---|---|
| URL | 未实现 | `wss://wss-goofish.dingtalk.com/`（全部） | 升级头：Cookie（须含匹配 `_m_h5_tk`，GP:846-901 不匹配则静默丢消息）+ Chrome UA + Origin www.goofish.com |
| `/reg` | 未实现 | A:168-182；ar-f:13490；GJ:6115；GP:build_reg_message:1220 | headers：`cache-header: app-key token ua wv`、`app-key: 444e9908...`、`token=<accessToken>`、`dt:j`、`wv:im:3,au:3,sy:6`、`sync:0,0;0;0;`、`did`、`mid`；GP code 401=失败 |
| `/r/SyncStatus/ackDiff` | 未实现 | A:185；ar-f:13507；GJ:6132；GP:1249 | `{pipeline:"sync", tooLong2Tag:"PNM,1", highPts:0, pts:<now_ms×1000>, seq:0}`；GP：空 ackDiff→睡 3s→下轮 |
| 心跳 `/!` | 未实现 | 全部 15s（A timeout 5s；ar-f/GJ 30s；GP 带读超时） | ar-f 按 mid 关联心跳响应 + 流看门狗（假在线检测，grace=max(4×hb,120)s） |
| 入站 syncPushPackage | 未实现 | A:205-216；ar-f:16593；GJ:8340；GP:parse_sync_package:199 | `body.syncPushPackage.data[0]` base64 → JSON/MessagePack/AES 解码（4 策略）；数字键字段 "1"=sender "2"=sId "3"=pnmId "5"=messageTime "6"=content "10"=reminder |
| ACK | 未实现 | A:362+655（双发）；ar-f:13526；GJ:8314；GP:ACK-first mid 匹配:1827 | `{code:200, headers:{mid:<入站mid>, sid:<入站sid>}}` |
| 发送 `/r/MessageSend/sendByReceiverScope` | send_message 概念对应 | A:112；ar-f:13425；GJ:5848；GP:1309-1373 | `contentType:101` 信封 + `custom{type:1, data:base64(JSON{contentType:1,text})}`；**actualReceivers 必含自己**（缺→500）；GP dedup TTL 60s 容量 500 + 空闲>30s 预检 |
| 历史 `/r/MessageManager/listUserMessages` | list_messages 概念对应 | Y:42；ar-f:13596；GJ:5982；GP:821 | `[cid@goofish, false, <ts>, limit, false]`；分页 `hasMore==1`+`nextCursor` |
| 会话列表 `/r/Conversation/listNewestPagination` | 未实现 | ar-f:13733；GP:803 | `[ts, limit]` |
| 建会话 `/r/SingleChatConversation/create` | trait 无 | Y:104；ar-f:13395；GJ:5694；GP | `pairFirst/pairSecond@goofish, bizType:"1", extension.itemId` |
| 其他路由 | 未实现 | GP `/s/para, /s/sync, /s/vulcan`；ar-f `/s/vulcan` | 服务端推送流 |

### 3.7 频控 / 风控 / 退避

| 策略 | fishpond | 参照谱系 | 判定 |
|---|---|---|---|
| 通用重试 | 熔断+线性退避+attempt 截止期 | A 固定 `sleep(0.5)`≤2-4 次；ar-f `min(3n,15)/min(10n,60)/min(5n,30)` 连续 5 败→重登；GP 重连指数 5s→300s×2、发货≤4、鱼塘页退避 `0.3×attempt+U(0,0.3)` 并发信号量 8 | **MED**：骨架已有，校准参数缺 |
| 风控响应 | RateLimited 类+retry_after 已备 | `RGV587_ERROR`/`被挤爆啦`/`punish`/`FAIL_SYS_USER_VALIDATE`；GP 建 IP 风控熔断（自适应 300/1800/3600s 窗）+ 验证码 solver 队列（5min 抑制）+ x5sec 恢复链 | **HIGH**：映射表+验证码分支未建 |
| 发送节奏 | 无 | A 人机打字模拟 0-1s+0.1-0.3s/char cap 10s；GP 发送 dedup + 空闲预检 | **MED** |
| anti-content 类头 | 无 | **全五仓均无 x-mini-wua/x-sgext/anti-content 生成**；仅 GP 有 tfstk（bootstrap）+ x5sec（验证码恢复）链 | 一致：现行参照都绕不开验证码，只能恢复不能规避 |

---

## §4 设计缺口清单（F032/F033 输入，非修复单）

| 级 | 缺口 | 对照证据 |
|---|---|---|
| HIGH | Token 过期感知+分层自动刷新（mh5tk/WS token/keepalive 各自节奏 + Set-Cookie 合并保护策略） | GP:cookie_token_refresher.py:48-57；ar-f:7054-7220；GJ:1602-1646 |
| HIGH | WS 事件流面（lwp 协议：reg/ackDiff/心跳/syncPushPackage/ACK）——Capability::StreamEvents 落地 | §3.6 全部 |
| HIGH | ret 字符串→ErrorClass 映射（含 RGV587→RateLimited+retry_after、USER_VALIDATE→验证码分支、SESSION_EXPIRED→Auth） | GP:406-432；ar-f config；A:204 |
| HIGH | 消息模型扩展（contentType 1/2/8/11/26、reminder→itemId 提取、订单事件、数字键解码） | GP:ws_protocol.py:415-909；A:384-451 |
| HIGH | 发送 payload 构造（uuid/cid/actualReceivers 含自己/contentType:101 信封） | GP:1309-1373 |
| HIGH | create_conversation 能力 + list 消息 cursor 分页 | Y:104；ar-f:13733；GP:803 |
| HIGH | 验证码/x5sec 恢复链（参照只能恢复不能规避——授权时须明确此边界） | GP:ws_token.py:285-409 |
| MED | 频控校准参数（重连指数 5s→300s、页间 U(0.5,1.5)、并发信号量、IP 风控自适应窗） | GP 各处 |
| MED | 发货幂等特判（`ORDER_ALREADY_DELIVERY`=成功）+ 订单 API 名不稳定的多候选探测 | GP:1039-1102；GJ:10827 |
| MED | 卖家台头区分（seller.goofish.com + `idle_site_biz_code: COMMONPRO` 独立 header 组） | GP:620-633 |
| MED | device_id/mid/uuid 标识生成（device_id=uuid4+unb；mid=`<rand><ts> 0`） | A/utils/xianyu_utils.py:30-58 |

### §4.1 参照间矛盾裁定

| 矛盾点 | 各参照 | 裁定（多数/最新为准） |
|---|---|---|
| UA 版本 | A=133；ar-f=133（WS/默认）+139（token，GJ 亦 139）；GP=120/138（seller）/146（WS） | 无单一真值——平台按 UA 分级风控；实现时以**最新可用 + 可配置**为准 |
| kgraph 版本 | Y/GP=2.0；GJ=1.0 | **2.0**（多数+更新） |
| item.detail params | A 缺 `accountSite/spm_pre/log_id`；Y/GP 全含 | 全含（Y/GP 一致） |
| ACK 次数 | A 每 sync 双发；其余单发 | 单发（多数） |
| 解码 | A 纯 Python MessagePack；Y Node execjs；GP Python 4 策略 | GP 多策略最稳；纯 Python 优先（少运行时依赖） |
| cookie 持久化 | A 静态 .env；Y live jar；ar-f/GJ DB+保护合并；GP 加密入库+Redis x5sec | DB+保护合并是底线（A 静态法会被服务端轮换打断） |
| WS RPC 路由集 | 基础六路一致；GP 多 `/s/para,/s/sync`；ar-f 多 `/s/vulcan,/r/Conversation/listNewestPagination` | 超集记录，按需实现 |
| 心跳超时 | A=5s；ar-f/GJ=30s；GP 读超时 | 30s（多数；A 的 5s 过紧） |
| token 刷新节奏 | A=3600s；ar-f/GJ=72000s；GP=随机 10-14h | 不可照抄——GP 随机化最抗风控，节奏须可配 |

---

## §5 判定汇总

| 维度 | 判定 |
|---|---|
| fishpond 真实出站调用点 | **零**——红线完好；provider-official 仅 stub；全仓无平台端点/凭据常量 |
| Provider trait 覆盖 | 5 方法语义有映射面（authorize/send/list_conversations/list_messages/list_products ↔ token/WS send/listUserMessages/item.list/xyh.list）；**缺 create_conversation、实时 StreamEvents、cursor 分页** |
| sign/端点正确性 | 五参照 sign 公式完全一致 `md5(token & t & 34839810 & data)`，appKey 内外双层（网关 `34839810` / IM `444e9908...`）——F032 实现时照此双层结构 |
| 错误映射 | **未建立**（HIGH §4） |
| 会话生命周期 | **不匹配**（HIGH §4） |
| 幂等模型 | **方向一致**：GP send dedup（TTL 60s/500 容量）验证了 fishpond 幂等键设计；发货面须按 `ORDER_ALREADY_DELIVERY` 特判 |
| 频控退避 | 骨架更完整（熔断+attempt 截止期），校准参数缺（MED） |
| anti-content/风控 | 参照均无法生成 anti-content 类头，只能做恢复链——**授权时须向用户明示此天花板** |
| 修复清单 | **无代码修复项**（无真实调用点可修）；§4 全部为 F032/F033 设计输入，授权解除后按修复单执行 |

---

## §6 方法与局限

1. **静态只读**：未运行任何真实账号操作；Mock 是唯一 CI 目标未破坏
2. **许可证边界**：GPL/AGPL/无许可证仓只读研究，本文件仅记协议行为面+证据指针，无代码复制
3. **协议漂移**：闲鱼协议未公开且多 API 名不稳定（GJ 订单名 11 候选探测、GJ 改价 5 候选）——任何实现都要带多候选探测与降级
4. **风控天花板**：五参照无一能生成 anti-content/x-mini-wua；只能 x5sec/验证码恢复——F032/F033 授权评审须含此边界
5. **覆盖声明**：A/Y/ ar-f/GJ/GP 五仓出站面全提取（合计 ~60 端点 + WS 协议）；非平台出站（AI 提供商、通知推送、更新器）不在审查范围
