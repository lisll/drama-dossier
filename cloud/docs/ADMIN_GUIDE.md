# 管理后台操作手册

> 面向**运维与运营**：怎么开管理员账号、登录后在八个视图里看什么、能改什么，
> 以及最常见的两类客诉（「充了钱没到账」「积分不对」）怎么一步步查。
>
> 接口与安全设计见 [`COMMERCE.md`](COMMERCE.md#310-管理后台)；整套服务的部署见
> [`DEPLOYMENT.md`](DEPLOYMENT.md)；签名密钥的轮换见 [`../../docs/KEY_ROTATION.md`](../../docs/KEY_ROTATION.md)。

管理后台是账号与授权服务（`drama_cloud.commerce`）的一部分。它**只有一页静态页面**加一组 JSON 接口：

| 东西 | 位置 | 实现在哪 |
| --- | --- | --- |
| 页面 | `https://<部署地址>/admin` | `commerce/admin_page.py`、`commerce/web/admin.html` |
| 接口 | `https://<部署地址>/api/v1/admin/*` | `commerce/admin_routes.py`、`commerce/admin_views.py`、`commerce/client_errors.py` |

页面与接口刻意分开：页面是静态文件，接口是 JSON，混在一起路由表会读不懂
（`admin_routes.py` 的模块注释）。

## 1. 管理员账号

### 1.1 没有注册入口，只能从命令行创建

网页上**没有**注册入口，也没有「第一个用户自动成为管理员」这类引导。

理由写在 `admins.py` 的模块注释里：网页上开放注册的管理后台，注册入口本身就是攻击面。
所以管理员只能由拿到服务器 shell 的人创建——这本身就是一道边界。

### 1.2 创建

```bash
drama-commerce admin create --email ops@example.com
```

- **密码从标准输入读**，不走命令行参数（`cli.py` 的 `_read_password`）。命令行参数会进 shell 历史与进程列表。
  交互执行时会用 `getpass` 提示两次并要求一致；**非交互（脚本 / CI）时从 stdin 读一行**。
- 密码**至少 12 个字符**（`admins.py:76-78`）。管理员密码比用户密码要求更高：这个账号能动钱与价目。
- 邮箱会先 `strip().lower()` 再入库；表上有 `CHECK (email = lower(btrim(email)) ...)` 兜一层
  （`migrations/0011_admins.sql:13-14`）。
- 同一邮箱重复创建会报 `409 admin_exists`，**不会**静默覆盖已有密码（`admins.py:84-85`）。
- 成功时打印 `{"admin_id": ..., "email": ...}`。

在容器里执行（部署 compose 的写法）：

```bash
docker compose -f compose.deploy.yaml exec commerce \
  drama-commerce admin create --email ops@example.com
```

脚本里带密码（注意 `admin create` 在非交互时只读一行，不要求二次确认）：

```bash
printf '%s\n' "$ADMIN_PASSWORD" | drama-commerce admin create --email ops@example.com
```

### 1.3 列出、停用与恢复

```bash
drama-commerce admin list                    # 全部管理员：id、email、创建时间、是否停用
drama-commerce admin disable --email ops@example.com
drama-commerce admin enable  --email ops@example.com
```

- **停用，不删除**：审计日志里会引用管理员 id，删掉就断了线索。
  表上留的是 `disabled_at` 时间戳，不是删行。
- 停用的同时**立刻吊销该管理员的全部会话**（`revoke_reason = 'admin_disabled'`），
  不等 8 小时自然过期。
- 停用后再登录，返回的仍是「邮箱或密码不正确」——不告诉外人「这个账号存在但被停用了」。
- **恢复**用 `admin enable`：把 `disabled_at` 置空，人就能重新登录。
  为什么需要它：停用是常规运维动作（人休假、临时收回权限），人回来要能恢复；
  没有这条命令就只能直接改库，而改库会绕过审计。
  - 管理员不存在 → 404「没有这个管理员」；
  - 管理员没被停用 → 409「没有被停用」。两种情况要分开，因为运维要做的事不同。

### 1.4 四个子命令都只读一个配置

`admin create` / `admin disable` / `admin enable` / `admin list` 只需要 `DRAMA_COMMERCE_DSN`
（PostgreSQL 连接串），不要求 SMTP、私钥等其它配置齐全（与 `cards`、`credits` 同一类命令）。

## 2. 登录与会话

打开 `/admin`，输入邮箱与密码。

- 登录成功返回管理员令牌，前缀是 **`adm_`**（`admins.py:26`）。用户令牌的前缀是 `at_`，
  两种令牌**互不通用**：拿用户令牌调 `/api/v1/admin/*` 一律 401，拿管理员令牌调用户接口同样过不去
  （`admin_routes.py:45-61`、`tests/commerce/test_admins.py` 的 `test_a_user_token_cannot_act_as_an_admin`
  与 `test_an_admin_token_cannot_act_as_a_user`）。
- 会话有效期 **8 小时**（`admins.py:28`）。比用户会话短——管理后台能动钱和价目，不该长期挂着。
- 令牌只存**摘要**（`bytea`）在库里（`0011_admins.sql:32-37`），数据库泄露也拿不到能用的令牌。
- 页面把令牌存在 **`sessionStorage`**，不是 `localStorage`（`web/admin.html:183-185`、`193`、`272`）：
  关掉标签页就失效，管理后台不该在共用电脑上长期留着登录状态。
- 因此**刷新页面**仍然保持登录（同一标签页），**关标签页或换标签页**要重新登录。这是刻意的行为，不是 bug。
- 接口返回 401 时页面会自动退回登录页并提示「登录已失效，请重新登录」（`web/admin.html:221-227`）。
- 点「退出登录」调 `POST /api/v1/admin/logout` 吊销会话，本地同时清掉 `sessionStorage`。

### 登录限流

| 桶 | 上限 | 窗口 |
| --- | --- | --- |
| 按来源 IP | 20 次 | 10 分钟 |
| 按邮箱 | 8 次 | 10 分钟 |

来源：`admin_routes.py:33-34`（`Rule("admin:login:ip", 20, 600)` / `Rule("admin:login:email", 8, 600)`）。
比用户登录更紧——管理员账号能动钱与价目。被限流时返回 429，并带 `Retry-After`。

计数存在 PostgreSQL 的 `rate_limits` 表里，多个实例共享同一份计数；**被拒绝的请求也算一次**
（`ratelimit.py` 模块注释）。

## 3. 八个视图

左侧导航八项：用户、卡密批次、订单、积分流水、设备、公告、开关与版本、价目与档位。
**前五个是只读**，只有公告、开关与版本策略、价目与档位能改。

只读的五个视图有明确理由：它们是「已经发生的事实」，管理员需要看，但不该在后台随手改。
改用户余额要走 `credits` CLI（有审计与幂等键），改订单状态更不该允许。

### 3.0 价目与档位（`GET /api/v1/admin/prices`、`PUT /api/v1/admin/prices/{subject}`）

总方案要求「价格做成后台可配置」，所以改价不需要重新部署。

- 三个计价对象：`version`（本机处理版本）、`package`（报审包）、`server_version`（服务端处理），
  各自可以改「每集多少积分」与「起扣集数」。不认识的 subject 直接 404——客户端只认这几个。
- **改价只影响之后签发的配方与新下的订单**：配方在签发时把价格写进 `recipes.credits`、
  订单在创建时把金额写进 `payment_orders`，两者都不重算。所以改价不会让用户已买的东西变价。
- 充值档位用 `PUT /api/v1/admin/recharge-tiers/{id}` 做 upsert：既能改已有档位，也能加新档。
  **下架的档位仍会列出来**（`active=false`），否则没法重新上架。
- 档位 id 一旦被历史订单引用就不要再改：改 id 等于把历史订单和档位断开。
- **邀请奖励**用 `GET` / `PUT /api/v1/admin/invite-settings`：邀请人得多少积分、被邀请人得多少、
  每个邀请人最多拿几次。**改配置只影响之后发放的奖励**：已发的在发放那一刻就把数量写进了
  `invite_rewards.credits`，不会跟着变。审计（`admin.invite_settings.set`）同时记改前值与改后值。
- 所有改动都写审计（`admin.price.set` / `admin.tier.upsert` / `admin.invite_settings.set`）。

### 3.1 用户（`GET /api/v1/admin/users`）

| 列 | 字段 | 说明 |
| --- | --- | --- |
| 邮箱 | `email` | 管理员本来就要靠它找人，所以这里返回 |
| 余额 | `balance` | 该用户 `credit_ledger.delta` 的求和（`COALESCE(SUM(delta), 0)`） |
| 使用权 | `entitlement` / `expiresAt` | `none` / `timed` / `lifetime`，加到期时间 |
| 设备 | `deviceCount` | 未解绑的设备数（`unbound_at IS NULL`） |
| 注册时间 | `createdAt` | |

- 支持 `keyword` 按邮箱模糊匹配（`ILIKE '%…%'`），返回里另带 `total`（匹配总数）。
- 已注销的用户（`deleted_at` 非空）不出现。
- 注意：用户邮箱在这里是**明文**，但审计日志里只记邮箱域名（见 6）。

### 3.2 卡密批次（`GET /api/v1/admin/card-batches`）

每批：`name`、`kind`（`timed` / `lifetime`）、`days`、`priceFen`（售价，分；`null` 表示生成时没标价，
**不是免费**——0 才是赠品卡）、`createdBy`、`createdAt`、`revoked`、`total`（总数）、`redeemed`（已兑换）、
`remaining`（未兑换）。售价在 `cards generate --price-yuan` 时记录，**只用于对账盘点，不参与兑换**。

**不返回卡密明文**（`cloud/src/drama_cloud/commerce/admin_views.py:100`）：数据库里只有摘要，明文只在 `cards generate` 导出 CSV
那一刻存在。CSV 丢了找不回来，泄露了用 `cards revoke` 作废整批。

### 3.3 订单（`GET /api/v1/admin/orders`）

每笔：`outTradeNo`、`email`、`amountFen`（分）、`credits`、`channel`、`status`、`tierId`、
`createdAt`、`paidAt`、`expiresAt`。

`status` 可选 `created` / `paid` / `closed` / `refunded`，传别的值直接 422
（`admin_views.py:145-146`）。

**「为什么没到账」最常见的原因就是状态停在 `created`**——用户点了下单但没付款（页面上的提示也这么写）。

### 3.4 积分流水（`GET /api/v1/admin/ledger`）

每行：`key`（幂等键）、`delta`（正负）、`kind`、`ref`、`settledState`、`memo`、`email`、`createdAt`。

- 可按 `email` 精确筛（会先 `strip().lower()`）。
- 账本**只增不改**：余额是由这些行的 `delta` 求和算出来的，没有第二份可被算错的数据
  （`credits.py` 模块注释）。
- `settledState` 只对 `kind = 'hold'`（预扣）有意义，取值 `reserved` / `settled` / `released`。
- 悬挂超过 2 小时的预扣会被自动回收（`HOLD_TTL_SECONDS = 2 * 3600`，`credits.py:32`）：
  原预扣标成 `released`，另加一条正的退还流水。查一次余额就会顺带触发这次清理。

### 3.5 设备（`GET /api/v1/admin/devices`）

每行：`deviceIdPrefix`、`name`、`platform`、`appVersion`、`email`、`boundAt`、`lastSeenAt`、
`unboundAt`、`unbindReason`。

**机器码只给前 12 位**（`cloud/src/drama_cloud/commerce/admin_views.py:231`、`250`：`row["device_id"][:12]`）——它是设备标识，
前 12 位够辨认、不泄露完整标识。字段名就叫 `deviceIdPrefix`，界面上写「机器码前 12 位」。

可按 `email` 精确筛。

### 3.6 公告（可写）

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/v1/admin/announcements` | 全部公告（含已停用），按 `sort_order, created_at DESC` |
| POST | `/api/v1/admin/announcements` | 新建，201 返回 `{id}` |
| PATCH | `/api/v1/admin/announcements/{id}` | 整行更新（**不是**部分更新），找不到 404 |
| DELETE | `/api/v1/admin/announcements/{id}` | 删除，204；找不到 404 |

请求体校验（`admin_views.py:267-272`）：

| 字段 | 约束 |
| --- | --- |
| `title` | 1–120 字符，必填 |
| `body` | 1–2000 字符，必填 |
| `level` | `info` / `warn`，默认 `info` |
| `sort_order` | 0–10000 的整数，默认 100（小的排前面） |
| `active` | 布尔，默认 `true` |

表上还有生效窗口 `starts_at` / `ends_at`（`0012_ops_content.sql:15-22`），但**后台的写接口不收这两个字段**，
新建的公告窗口都是空（等于不限制）。需要定时上下线时目前只能直接改库。

`PATCH` 是**整行替换**：只改一个字段（例如只停用）时必须把 `title` / `body` / `level` / `sort_order`
一并带上，否则会被清空。页面上的「停用 / 启用」按钮为此会先读整行再整体提交
（`web/admin.html` 的 `toggleAnnouncement`）。

桌面端只拉到 **`active = true` 且在生效窗口内**的公告，最多 20 条（`client_config.py:33-43`）。

### 3.7 开关与版本策略（可写）

**功能开关**：

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/v1/admin/flags` | 四个开关的 `key` / `enabled` / `note` / `updatedAt` |
| PUT | `/api/v1/admin/flags/{key}` | 请求体 `{enabled, note}`（`note` 最长 200）；不认识的 key 回 404 |

四个 key 是**白名单字面量**，表上有 `CHECK (key IN ('signup', 'recharge', 'server_processing', 'version_task'))`
（`0012_ops_content.sql:29-30`）。后台造不出客户端看不懂的第五个开关。默认四个全开
（`0012_ops_content.sql:72-76`）。

各 key 关掉之后界面上的变化（`src/main.js` 的 `applyFeatureFlags`）：

| key | 关掉后的界面表现 |
| --- | --- |
| `signup` | 登录页的「注册」标签被隐藏 |
| `recharge` | 积分弹窗里的档位与「充值」按钮被隐藏；**已充的积分照常能用** |
| `server_processing` | 「服务端处理」页顶部显示「服务端处理暂时关闭了。本机处理版本仍然可用；积分与充值不受影响。」 |
| `version_task` | 「处理版本」页顶部显示「本机处理版本暂时关闭了。服务端处理仍然可用；积分与充值不受影响。」 |

**重要：开关只管界面入口。** 它是「界面藏起来」而不是「服务端放行」这一套双保险的一半——
不要把开关当成权限控制或计费开关来用。

**版本策略**（单行表，`id` 固定为 1）：

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/v1/admin/version-policy` | `minSupported`、`latest`、`downloadUrl`、`downloadSha256`、`downloadSig`、`notes`、`updatedAt` |
| PUT | `/api/v1/admin/version-policy` | 整行更新，返回 `{"ok": true}` |

请求体字段与校验（`admin_views.py:405-411`）：

| 字段 | 约束 | 填什么 |
| --- | --- | --- |
| `min_supported` | 1–32 字符，形如 `1.2.3` | 低于它的客户端**会被拦住**：除首页与设置外的功能都进不去 |
| `latest` | 1–32 字符，形如 `1.2.3` | 用于「有新版本可用」提示 |
| `download_url` | ≤ 500 字符，**必须 https** | 升级包地址 |
| `download_sha256` | 空串，或 64 位**小写**十六进制 | 整文件 SHA-256 |
| `download_sig` | ≤ 200 字符 | 升级包的 Ed25519 detached 签名（base64url） |
| `notes` | ≤ 2000 字符 | 更新说明 |

- 版本号只做**形状**校验（按 `.` 切开后每段都是数字，至少两段），不做语义比较：比较在前端与 Rust 侧各有一份，
  这里再写一份只会出现两处规则不一致（`admin_views.py:437-440`、`455-460`）。
- 默认值是 `min_supported = latest = '0.0.0'`、下载地址为空（`0012_ops_content.sql:40-49`、`78-79`）——
  刚上线不拦任何人、也不提示任何人更新。
- **`min_supported` 是「拦截」而不只是「提示」**：桌面端 `updateState` 算出 `force` 之后，
  `L.blockedByVersion` 会把**除首页与设置以外的所有功能模块**换成遮罩
  （`src/logic.js` 的 `blockedByVersion`、`src/main.js` 的 `switchModule`）。
  只弹横幅是不够的——用户点掉提示就继续干活，那「强制更新」等于没做。
  首页与设置必须放行：要在那里看提示、下载并安装更新，兑换卡密也不受影响。
  真正抬高 `min_supported` 之前请确认新版已经发布，否则会把所有用户挡在门外。
- **签名留空，客户端会拒绝安装**。`src-tauri/src/updater.rs` 的 `check_metadata` 要求地址、哈希、签名三样齐全，
  缺任何一项都**不做「降级成只验哈希」**；地址不是 `https://` 开头也直接拒绝。
  也就是说：只填地址和哈希、不填 `download_sig`，用户点「下载并校验升级包」后会看到
  「暂时无法更新：升级包没有签名，拒绝使用」。
- 三样都填对之后，横幅里的按钮才会走通：点「下载并校验升级包」→ Rust 侧下载并校验 →
  通过后横幅变成「升级包已就绪，校验通过」并给「现在安装」按钮 → 点了之后启动系统安装器。

### 3.8 客户端错误（`GET /api/v1/admin/client-errors`）

这一项**不在左侧导航里**（导航只有上面七个入口），但接口可用，要用 curl 或自己拼地址查
（写法见第 7 节）。字段命名与其它视图一致：

每行：`code`、`appVersion`、`platform`、`location`、`occurrences`、`firstSeen`、`lastSeen`，
按 `last_seen` 倒序，`limit` 默认 100，实际夹在 1–200（`cloud/src/drama_cloud/commerce/client_errors.py:96`、`105`）。

- 同一组 `(code, app_version, platform, location)` 只保留一行并累加 `occurrences`，
  不逐条存——同一个错误一天可能上报成百上千次。
- 桌面端**默认不开**上报（设置里的开关默认关闭，见 [`USER_GUIDE.md`](../../docs/USER_GUIDE.md)），
  所以这里的数据量取决于有多少用户主动打开过。
- 用户报障时让他截图：这里**没有**自由文本字段，收不到错误消息、路径或剧名，只有错误码与模块名。

### 3.9 分页上限

所有分页接口的 `limit` 都受 `MAX_PAGE = 200` 限制（`cloud/src/drama_cloud/commerce/admin_views.py:33`），
`Query(ge=1, le=MAX_PAGE)` 在入口就拒掉 `limit=1000000` 这种把库拖死的请求。
`offset` 必须 ≥ 0。默认每页 50（页面上的 `pageSize`）。

## 4. 排查场景一：「用户说充了钱没到账」

按这个顺序查，每一步都能得出「继续往下查」还是「到这一步就结束了」的结论。

### 第一步：看订单状态

后台 → **订单**，或直接按邮箱查接口。找到用户说的那笔单，看 `status`：

| status | 含义 | 下一步 |
| --- | --- | --- |
| `created` | 还没收到支付回调 | **这就是最常见的原因**：用户点了下单但没完成付款，或者付了但回调没到。见第二步 |
| `paid` | 已支付 | 入账应该已经发生，去第三步核对流水 |
| `closed` | 已关闭 | 这笔单不会再入账。让用户重新下单 |
| `refunded` | 已退款 | **只更新了订单状态，账本没动**（`payments/service.py:157-164`）。积分不会自动扣回去，需要按第四步处理 |

`closed` / `refunded` 都来自渠道推来的事件，入账只有 `paid` 一条路。

### 第二步：`created` 的单，怎么判断回调到底来没来

- 客户端上用户可以点「我已完成支付」触发主动查单，但**模拟支付渠道没有渠道侧可查**，
  这一步会直接报「渠道还没配置好」（`cloud/docs/COMMERCE.md` 3.8 节「支付」）。
  部署阶段用的是模拟支付时，只能靠用户回模拟收银台页面确认回调已经发出。
- 真实渠道下：回调地址必须能从公网访问（`DRAMA_COMMERCE_PUBLIC_BASE_URL`），
  否则渠道推不过来。这是「下单成功、回调永远不到」的典型部署问题。
- 回调被拒的两种情况会在审计日志里留下痕迹（用独立事务写，不会被随后的异常回滚掉）：
  `payment.unknown_order`（订单号对不上）与 `payment.amount_mismatch`（回调金额与订单不一致）。
  查到这两条说明回调**来了但被拒**，不是「没来」。
- **不要手改订单状态**：后台没有这个接口，也不该有。已支付但没入账的情形要查是不是账本幂等键挡住了重复入账
  （同一个订单号第二次回调会直接返回「已处理」，`credited: 0`）。

### 第三步：核对积分流水

后台 → **积分流水**，按该用户邮箱筛。一笔成功的充值应该有一行 `kind = grant`、
`ref = <订单号>`、`key = order:<订单号>`、`delta` 等于订单的 `credits`。

- 流水里有、余额看起来没变：让用户点「刷新」（重新查余额），或看是否被后续的 `hold` 预扣掉了。
- 流水里没有：回到第一步。**入账只有 `payments.service.handle_event()` 一条路**，
  渠道实现不许自己改账本，所以流水缺失就等于回调没被接受。

### 第四步：退款与「退积分」

- 订单状态里的 `refunded` 是**渠道侧的事件**。收到它时服务端**只更新订单状态、不动账本**
  （`payments/service.py:157-164`）。
- 也就是说：退款之后，用户手里已经拿到的积分**仍然在账上**。而账本只增不改，
  `credits.grant` 要求金额大于 0（`cloud/src/drama_cloud/commerce/credits.py:120-121`），CLI 的
  `credits grant --amount` 也要求正整数、只用来加积分（`cli.py:215-216`）——**后台与 CLI 都没有「扣回积分」的口子**。
- 结论：**退款流程需要开发介入**，后台能做的只是确认状态。这一点要提前和运营讲清楚。

### 顺带：`reconcile` 不是万能药

用户点「我已完成支付」时客户端调的是订单侧兜底查询，它先看订单状态，已支付就直接返回
`already: true`；否则调渠道查单，查不到就**如实说「还没付」，不猜、不手动置为已付**。
所以「点了好几次还是没到账」通常意味着渠道那边确实没这笔账，要让用户看支付记录。

## 5. 排查场景二：「用户说积分不对」

答案永远在这一条上：**流水的 `delta` 求和 == 用户看到的余额**。账本只增不改，
余额不是存下来的一个字段，而是算出来的（`cloud/src/drama_cloud/commerce/credits.py` 模块注释、
`admin_views.py:59` 的 `SUM(delta)`）。

### 核对步骤

1. 后台 → **用户**，记下该用户的「余额」。
2. 后台 → **积分流水**，按邮箱筛（把 `limit` 调到 200，必要时翻页），把 `delta` 累加。
3. 两个数应当相等。不相等只有两种可能：
   - **流水没看全**：流水接口按 `created_at DESC` 分页，翻页时可能有新行插到前面。
     对不平就先停掉该用户的操作，再从第一页重新数。
   - **库里真不一致**：那说明有人绕过 `credits` 直接写了 `credit_ledger`，或者删过行。
     账本没有更新与删除的接口，正常情况下不可能发生。
4. 数得差不多但用户说「少了几十点」：看有没有 `kind = hold` 且 `settledState = reserved` 的行。
   **预扣期间余额已经减掉了**，而用户往往只记得「我充了多少」。悬挂超过 2 小时的预扣会自动退还
   （原行标 `released`，另加一条正的退还流水），让用户点一次「刷新」即可触发清理；
   **例外**：`ref` 以 `server_task:` 开头（服务端处理）的预扣**不参与自动回收**——任务排队加
   重试很容易超过 2 小时，它的结算与退还由媒体服务在任务终态主动完成（见下一段）。

### 常见误解，可以这样回答用户

| 用户说 | 事实 |
| --- | --- |
| 「我充了 550，怎么只多了 550 又少了 5」 | 充值加 550，之后生成报审包/服务端处理按集预扣了 5。预扣在流水里是负的 `hold` 行 |
| 「处理版本失败了，积分怎么不退」 | 处理版本在本机执行、视频不上传，服务端判断不了成败，所以**不退**。报审包与服务端处理的失败会自动退还预扣 |
| 「有一条预扣一直不动」 | 报审包类的预扣超过 2 小时会自动回收（查一次余额触发）。`server_task:` 开头的是服务端处理任务，**不自动回收**，任务到终态（成功/失败/取消）时由媒体服务结算或退还——真卡住的处置见下一节 |
| 「余额怎么比我算的少 1」 | 让用户按流水逐笔对。价目是服务端的种子数据，后台改不了价（`price_rules` 表） |

### 管理员在这里能做什么、不能做什么

- **能**：看余额、看流水、看到期时间与设备数。
- **不能**：在后台改余额或改价目。后台没有这些接口，是刻意的设计。
  补积分要用命令行 `drama-commerce credits grant --email a@example.com --amount 500 --memo 补偿 --by 你的名字`，
  它走账本、有审计、有幂等键（`admin:<操作人>:<用户>:<金额>:<备注>`，重复执行不会重复加）。

### 服务端处理的预扣卡住了怎么办

服务端处理（`ref` 形如 `server_task:tsk_...`）的预扣**不参与自动回收**（`credits.py` 的
`release_stale_holds` 明确排除这个前缀）。媒体控制面丢了任务（进程崩溃、任务记录没了）时，
那笔 `hold` 会一直停在 `reserved`，用户的余额里就少着这笔钱。定位与处置：

1. 后台 → **积分流水**，按用户筛出 `kind = hold`、`settledState = reserved`、`ref` 以
   `server_task:` 开头的行，记下 `ref` 和 `delta`。
2. 到媒体控制面查这个任务：网页控制台的任务页，或 `GET /api/v1/tasks/<task_id>`
   （`task_id` 就是 `ref` 里冒号后面那串）。
3. **任务还在**（排队中 / 运行中 / 刚到终态）：等它到终态，媒体服务会自动结算或退还，
   **不要动账本**。排队加上失败重试超过 2 小时是正常现象，先确认再动手。
4. **任务真没了**（404，且没有任务记录）：用 `drama-commerce credits grant` 把被扣的数额
   补给用户，**memo 里写清 ref**（例如「补偿：server_task:tsk_... 任务丢失」），事后对账才对得上。
   那行 `hold` 保持 `reserved` 原样——账本只增不改，没有、也不该有手工改行状态的命令。
   局限要如实知道：万一媒体侧后来又恢复并按同一 `ref` 退了一次，流水里会多出一条正的
   `release`（等于用户拿了两次），此时以流水为准向用户说明；没有「扣回」命令，多补的部分
   无法从账本里直接撤回，处理时务必先确认任务确实 404 再补偿。

## 6. 安全边界

### 有什么

| 措施 | 实现在哪 |
| --- | --- |
| 管理员是**独立的表**，不是给 `users` 加 `is_admin` 字段 | `migrations/0011_admins.sql:11-19` |
| 管理员会话、令牌都独立成表，令牌前缀 `adm_` | `migrations/0011_admins.sql:21-38`、`admins.py:26` |
| 令牌只存 SHA-256 摘要 | `migrations/0011_admins.sql:32-37` |
| 会话 8 小时，停用即刻吊销全部会话 | `admins.py:28`、`100-105` |
| 登录失败不区分「邮箱不存在」「密码错」「账号被停用」 | `admins.py:117-121` |
| 邮箱不存在时也做一次同等开销的哈希校验（防时序泄露） | `admins.py:56`、`117-118` |
| 密码哈希并发数有上限（4 个名额，写死在 `AdminService` 里），超出的排队 | `admins.py:54-55` |
| 登录限流：IP 20 次 / 10 分钟，邮箱 8 次 / 10 分钟 | `admin_routes.py:33-34` |
| 令牌只认 `Authorization: Bearer`，不接受网址参数 | `admin_routes.py:55-57` |
| 所有写操作都记审计 | `admin_views.py`（`admin.announcement.*`、`admin.flag.set`、`admin.version_policy.set`） |
| 审计里只记邮箱**域名**，不记管理员邮箱 | `admins.py:178-181` |
| 卡密不返回明文、机器码只给前 12 位 | `admin_views.py:100`、`250` |

管理员密码用的是与用户同一个 `PasswordHasher`（argon2id 默认参数，`admins.py:53`）。
同时进行的哈希数上限是**写死的 4 个名额**（`admins.py:55` 的 `BoundedSemaphore(4)`），超出的请求排队——
注意这一处**不读** `DRAMA_COMMERCE_HASH_CONCURRENCY`（那个配置只作用于用户账号的 `accounts.py:59`）。

### 没什么（已知限制）

- **没有 MFA / 二次验证**。管理员账号的安全性完全依赖密码强度与登录限流。真上线前应补上。
- **没有细粒度角色**：所有管理员权限相同，没有「只读管理员」或「只管公告的管理员」。
- **没有改密码命令**：改密码目前也没有接口，只能重新创建（邮箱会被占用）或改库。
- **会话不能主动列出与踢除**：只有 `logout`（自己的）与 `disable`（某人的全部）。
- **页面没有 CSRF token**：令牌走 `Authorization` 头而不是 Cookie，所以不受典型 CSRF 影响，
  但也意味着任何能读到 `sessionStorage` 的东西（例如恶意扩展）都能直接用这个令牌。
- **令牌在 `sessionStorage` 里是明文**：这是浏览器存储的固有性质，共用电脑上请记得点「退出登录」而不是只关页面。
- **`/docs` 与 `/openapi.json`** 是 FastAPI 自带的接口文档，管理接口也在里面。
  对外部署时可以在反向代理上屏蔽（Caddy 侧的写法见 [`DEPLOYMENT.md`](DEPLOYMENT.md)）。
- **审计日志只在库里**（`audit_log` 表），后台没有查看页面。要查直接跑 SQL 或用现有的运维手段。

## 7. 本地调试（curl）

本地起服务后（`drama-commerce serve --port 8100`），拿令牌：

```bash
TOKEN=$(curl -s -X POST http://127.0.0.1:8100/api/v1/admin/login \
  -H 'Content-Type: application/json' \
  -d '{"email":"ops@example.com","password":"你的密码"}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["token"])')

curl -s http://127.0.0.1:8100/api/v1/admin/me -H "Authorization: Bearer $TOKEN"
curl -s 'http://127.0.0.1:8100/api/v1/admin/users?limit=5' -H "Authorization: Bearer $TOKEN"
curl -s 'http://127.0.0.1:8100/api/v1/admin/orders?status=created&limit=20' -H "Authorization: Bearer $TOKEN"
```

改一个开关：

```bash
curl -s -X PUT http://127.0.0.1:8100/api/v1/admin/flags/server_processing \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"enabled":false,"note":"临时检修"}'
```

改版本策略（注意 `download_sig` 不能省，否则客户端会拒绝安装）：

```bash
curl -s -X PUT http://127.0.0.1:8100/api/v1/admin/version-policy \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"min_supported":"1.0.0","latest":"1.2.0",
       "download_url":"https://example.com/app.exe",
       "download_sha256":"'"$(printf 'a%.0s' {1..64})"'",
       "download_sig":"<base64url 签名>","notes":"修了些问题"}'
```

完整的自动化演练见 `cloud/tests/e2e_m7.py`：建管理员 → 登录 → 改版本策略 → 发公告 → 关开关 →
用户端公开接口跟着变 → 错误上报 → 后台可见 → 用户令牌进不了管理接口 → 停用后会话立刻失效。
