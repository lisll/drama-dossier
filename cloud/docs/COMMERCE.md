# 账号与授权服务（drama_cloud.commerce）

这个服务管两件事：账号（注册、登录、邮件验证码、令牌），和授权（卡密兑换、使用权、电脑绑定、
可以离线验证的授权凭证）。它和媒体控制面（`drama-cloud`）分开部署：处理任务再重，也不拖慢登录。
数据库用 PostgreSQL，因为卡密兑换、抢设备名额和以后的支付回调都需要可靠的并发事务。

- M1（已完成）：邮件验证码、注册、登录、刷新与吊销令牌、重设密码、修改密码、集中限流、审计日志。
- M2：卡密批量生成与兑换、使用权、电脑绑定与解绑、授权凭证的签发与刷新、管理命令。
- 桌面端接入（钥匙串、本机机器码、本地验签、离线提示）在 M3。
- M4a（本期）：报审包（`POST /packages`）与处理配方（`POST /recipes`、`/recipes/{id}/consume`），
  三种签名用途前缀隔离；处理配方接桌面界面在 M4b。

## 1. 本地开发

```bash
cd cloud
docker compose -f compose.dev.yaml up -d --wait     # PostgreSQL 18.6 + Mailpit v1.31.6，只绑定 127.0.0.1

export DRAMA_COMMERCE_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/drama_commerce'
export DRAMA_COMMERCE_CODE_SECRET='<至少 32 个字符的随机串>'   # 例如 python3 -c 'import secrets;print(secrets.token_urlsafe(48))'
export DRAMA_COMMERCE_SMTP_HOST=127.0.0.1 DRAMA_COMMERCE_SMTP_PORT=1025 DRAMA_COMMERCE_SMTP_SECURITY=plain
export DRAMA_COMMERCE_LICENSE_KEY_FILE="$HOME/.config/drama-commerce/license-dev.pem"

.venv/bin/drama-commerce keygen "$DRAMA_COMMERCE_LICENSE_KEY_FILE"   # 只做一次；打印的 public_key 给桌面端
.venv/bin/drama-commerce migrate          # 建表；重复执行没有副作用
.venv/bin/drama-commerce serve --port 8100

# 另开一个终端，生成 5 张测试用月卡（同样要先 export DRAMA_COMMERCE_DSN）
.venv/bin/drama-commerce cards generate --days 30 --count 5 --out "$HOME/cards-dev.csv"
```

- 验证码邮件发到 Mailpit，不会发出真实邮件。打开 <http://127.0.0.1:8025> 查看。
- 验证码在数据库里存的是用 `CODE_SECRET` 算的 HMAC。换了这个密钥，之前发出的验证码全部失效。
- 停止：`docker compose -f compose.dev.yaml down`；连数据一起删掉加 `-v`。
- compose 里的账号密码是开发专用的固定值，不能用在任何对外环境。
- 私钥和导出的卡密 CSV 都放在仓库目录外面：`.gitignore` 没有排除它们，放进仓库很容易被误提交。

## 2. 配置（环境变量）

| 变量 | 必填 | 说明 |
| --- | --- | --- |
| `DRAMA_COMMERCE_DSN` | 是 | PostgreSQL 连接串。`migrate` 只需要这一项 |
| `DRAMA_COMMERCE_CODE_SECRET` | 是 | 至少 32 个字符；用来算验证码和限流键的 HMAC |
| `DRAMA_COMMERCE_SMTP_HOST` | 是 | 发信服务器 |
| `DRAMA_COMMERCE_SMTP_SECURITY` | 否 | `starttls`（默认）/ `ssl` / `plain`。`plain` 只允许连本机 |
| `DRAMA_COMMERCE_SMTP_PORT` | 否 | 默认按上一项取 587 / 465 / 25 |
| `DRAMA_COMMERCE_SMTP_FROM` | 否 | 发件人，默认 `no-reply@<SMTP_HOST>` |
| `DRAMA_COMMERCE_SMTP_USERNAME` / `_PASSWORD` | 否 | 发信账号；有用户名才登录 |
| `DRAMA_COMMERCE_TRUSTED_PROXIES` | 否 | 前面有几层可信反向代理（0..5，默认 0）。0 表示不读 `X-Forwarded-For` |
| `DRAMA_COMMERCE_PRODUCT_NAME` | 否 | 邮件标题里的产品名，默认「剧备 DramaDossier」 |
| `DRAMA_COMMERCE_TERMS_VERSION` | 否 | 注册时记录用户同意的条款版本 |
| `DRAMA_COMMERCE_POOL_MIN_SIZE` / `_MAX_SIZE` | 否 | 连接池大小，默认 1 / 10 |
| `DRAMA_COMMERCE_HASH_CONCURRENCY` | 否 | 同时进行的密码哈希上限（1..64，默认 4）。每次哈希占 64 MiB，默认最多约 256 MiB |
| `DRAMA_COMMERCE_LICENSE_KEY_FILE` | 是 | 授权签名私钥（Ed25519 PEM，不加密）的文件路径，用 `keygen` 生成。私钥本身不放进环境变量 |
| `DRAMA_COMMERCE_LICENSE_TTL_HOURS` | 否 | 授权凭证有效期，也就是断网后还能用多久（1..720，默认 72） |
| `DRAMA_COMMERCE_LICENSE_REFRESH_HOURS` | 否 | 联网时桌面端每隔多久刷新一次凭证（1..720，默认 1，必须小于上一项） |
| `DRAMA_COMMERCE_DEVICE_LIMIT` | 否 | 每个账号同时绑定几台电脑（1..20，默认 1） |
| `DRAMA_COMMERCE_UNBIND_LIMIT` | 否 | 每个周期最多解绑几次（1..100，默认 3） |
| `DRAMA_COMMERCE_UNBIND_WINDOW_DAYS` | 否 | 解绑次数的统计周期，单位天（1..365，默认 30） |
| `DRAMA_COMMERCE_PDF_FONT` | 否 | 报审包 PDF 用的中文字体（`.otf` / `.ttf` / `.ttc` 文件路径）。不设时按常见系统路径找 Noto Sans CJK SC；**找不到字体服务拒绝启动** |
| `DRAMA_COMMERCE_ARTIFACTS_BIN` | 否 | `dossier-artifacts` 可执行文件路径。不设时按 PATH → `target/release` → `target/debug` 查找（见 `toolchain.py`） |

配置缺失或非法时，服务拒绝启动，报错里只出现变量名，不出现变量值。
`migrate`、`cleanup`、`cards` 只需要 `DRAMA_COMMERCE_DSN`；`keygen`、`keyinfo` 不读任何配置。
报审包与处理配方相关的接口还会用到上面两项：字体缺失时**启动**就失败；`dossier-artifacts`
缺失时启动不失败，但 `POST /packages` 会返回 503 `service_unavailable`。

## 3. 接口

所有接口前缀 `/api/v1`，请求和响应都是 JSON。时间字段是带时区的 ISO 8601 字符串。

### 3.1 账号

| 方法 | 路径 | 认证 | 请求体 | 成功 |
| --- | --- | --- | --- | --- |
| GET | `/health` | 无 | — | 200 `{"ok": true}`（会查一次数据库） |
| POST | `/auth/codes` | 无 | `email`、`purpose`（`register` / `reset_password`） | 202 `{"cooldown_seconds": 60, "expires_in": 600}` |
| POST | `/auth/register` | 无 | `email`、`code`、`password`、`accept_terms: true` | 201 `{"user": …, "tokens": …}` |
| POST | `/auth/login` | 无 | `email`、`password` | 200 `{"user": …, "tokens": …}` |
| POST | `/auth/refresh` | 无 | `refresh_token` | 200 `{"tokens": …}` |
| POST | `/auth/logout` | Bearer | — | 204，吊销当前会话 |
| POST | `/auth/password/reset` | 无 | `email`、`code`、`new_password` | 204，吊销该账号全部会话 |
| POST | `/auth/password/change` | Bearer | `current_password`、`new_password` | 204，只保留当前会话 |
| GET | `/me` | Bearer | — | 200 `{"user": {"id", "email", "created_at"}}` |
| GET | `/me/export` | Bearer | — | 200，个人数据 JSON（见下） |
| POST | `/me/delete` | Bearer | `password` | 204，立即注销并匿名化，不可撤销 |

`tokens` 的结构：

```json
{"access_token": "at_…", "refresh_token": "rt_…", "token_type": "Bearer",
 "expires_in": 900, "refresh_expires_in": 2592000}
```

- 邮箱会去掉首尾空格并转成小写，只接受 ASCII 邮箱。密码 8..128 个字符；登录时不检查长度下限。
- `accept_terms` 必须是 JSON 的 `true`，`"true"`、`1` 都不算。
- 请求体里多出来的字段一律拒绝。

导出（`/me/export`）的结构，`format` 是 `drama-dossier/account-export/v1`：

| 字段 | 内容 |
| --- | --- |
| `account` | `id`、`email`、`created_at`、`terms_version`、`terms_accepted_at` |
| `entitlement` | 同 `/me` 里的使用权 |
| `devices` | 全部绑定记录，含已解绑的：`code`（4 位一组的展示形式）、`name`、`platform`、`app_version`、`bound_at`、`last_seen_at`、`unbound_at`、`unbind_reason` |
| `redemptions` | 兑换记录：卡的 `kind`、`days`、`redeemed_at`，不含卡密 |
| `sessions` | 登录会话：`created_at`、`expires_at`、`revoked_at`、`revoke_reason` |
| `packages` | 报审包记录（**只有元数据**，不含文档内容）：`id`、`created_at`、`signer`、`episode_count` |
| `audit_log` | 本账号发起的操作：`at`、`action`、`subject`、`detail` |
| `exported_at` | 导出时间（数据库时间） |

导出里没有任何摘要、令牌、密码哈希；IP 本来就不存。

注销（`/me/delete`）必须重新输入密码，在一个事务里完成：

- 清空邮箱和密码哈希，记下 `deleted_at`；
- 删除会话（访问令牌、刷新令牌随之删除）、使用权、全部设备绑定、这个邮箱的验证码、报审包记录
  （`packages`）和处理配方记录（`recipes`）——前者含机器码和生成时间，后者绑定机器码，都属于个人数据；
- 审计日志记一条 `user.delete`，只记各项删除的数量。

用户行本身保留：兑换过的卡仍然指向它，不会被别人再兑换一次；审计日志只记账号 ID，邮箱删掉以后对不上具体的人。
注销后同一个邮箱可以重新注册，得到的是一个全新的账号，拿不到旧账号的使用权。
注销过的账号登录返回 `invalid_credentials`，重设密码返回 `email_not_registered`，和邮箱从没注册过一样。

### 3.2 卡密与授权

都要登录（`Authorization: Bearer <访问令牌>`）。

| 方法 | 路径 | 请求体 / 参数 | 成功 |
| --- | --- | --- | --- |
| POST | `/cards/redeem` | `code` | 200 `{"entitlement", "card": {"kind", "days"}, "already_redeemed"}` |
| POST | `/license/activate` | `device_id`、`name`、`platform`、`app_version` | 200 `{"license", "device", "entitlement"}` |
| POST | `/license/refresh` | `device_id`，可选 `app_version` | 200，结构同上 |
| GET | `/devices` | 可选查询参数 `current_device`（本机机器码） | 200 `{"devices": […], "device_limit", "unbind_quota"}` |
| DELETE | `/devices/{id}` | `id` 取自设备列表 | 204 |

`/license/activate` 和 `/license/refresh` 的响应：

```json
{"license": {"token": "<base64url 载荷>.<base64url 签名>", "kid": "9f86d081884c7d65",
             "issued_at": "…", "expires_at": "…", "refresh_after": "…"},
 "device": {"id": "<绑定 ID>", "code": "3F2A-91C0-7B44-E1D8", "name": "办公室电脑", "platform": "windows",
            "app_version": "0.3.0", "bound_at": "…", "last_seen_at": "…", "current": true},
 "entitlement": {"kind": "timed", "active": true, "expires_at": "2026-11-14T08:00:00+00:00"}}
```

- `license.expires_at` 是凭证到期（离线宽限期的终点），`entitlement.expires_at` 是使用权到期，两者不是一回事。
- `entitlement.kind`：`none` 没有使用权；`timed` 按天计，到 `expires_at` 为止；`lifetime` 永久，`expires_at` 是 `null`。
  `active` 是服务端按数据库时间算好的「现在能不能用」。
- `unbind_quota` 形如 `{"limit": 3, "window_days": 30, "remaining": 2}`：本周期还能解绑几次。
- `code`（卡密）不分大小写，空格、短横线和 `DD` 前缀都会被忽略，`DD-ABCD-…` 和 `abcd efgh …` 都能兑换。
  长度不对或含 0、1、I、O 直接返回 422，不查库。
- `device_id`：桌面端算出的机器码摘要，64 位小写十六进制（SHA-256），只认这一种写法。
  `device.code` 是它的前 16 位，转成大写、4 位一组，给人看、报给发卡方用。
- `name` 去掉首尾空白后 1..64 个字符，不能含控制字符、换行、零宽字符这类看不见的字符（全角空格可以）；
  `platform` 只能是 `windows` / `macos` / `linux`；`app_version` 以数字开头，只含字母、数字和 `.+-`，最长 32 个字符。
- `current_device` 只用来在列表里标出本机（`current: true`）。机器码不是凭证，单凭它调不了任何接口。

兑换规则：

- 月卡从「当前到期时间」和「现在」中较晚的那个往后顺延（还没到期就接着续，过期了从现在算）；永久卡把账号变成永久，不再计时。
- 同一个人重复兑换同一张卡：原样返回当前授权，`already_redeemed: true`，不会重复加时间。
- 两个人同时兑换同一张卡：只有一个成功，另一个拿到 `card_used`。
- 已经是永久的账号再兑换任何卡：返回 409 `already_lifetime`，这张卡不算用掉，可以留给别人。

绑定与解绑（名额和次数是默认值，见第 2 节配置）：

- 每个账号同时绑定 1 台电脑。同一台电脑重复调用 `activate` 不占新名额，只更新名称、平台、版本并换发凭证。
- 名额满了返回 409 `device_limit`，响应体里带上已绑定的设备、`device_limit` 和 `unbind_quota`，
  客户端可以直接让用户选一台解绑。
- 没有使用权也能绑定（凭证里 `ent` 是 `none`）；兑换卡密后调一次 `refresh`，新凭证里就是新的使用权。
- 退出登录、重设密码、会话过期都不归还名额：名额跟着电脑走，换电脑必须先解绑。
- `refresh` 只给仍然绑定在这个账号上的电脑换发凭证，否则返回 403 `device_not_bound`。
- 解绑（踢设备）：那台电脑的绑定作废，它最近一次绑定或刷新时用的登录会话一并吊销，联网请求立即失败。
  踢的如果正是发起请求的这台电脑，当前登录也随之失效。
  但它手里已签发的凭证收不回来：那台电脑一直断网的话，本地还能用到凭证的 `exp`（最多 72 小时），这是离线宽限期的代价。
- 每 30 天最多解绑 3 次（滚动计算），用完返回 429 `unbind_limit`，`Retry-After` 给出还要等多少秒。
  只数用户自己发起的解绑（表里预留了后台解绑的标记，目前没有对应的命令）。

### 3.3 授权凭证与本地验签

授权凭证是服务端签名的一段文本，桌面端用内置的公钥在本地验签，断网也能验。

- 格式：`base64url(载荷 JSON) + "." + base64url(签名)`，base64url 不带填充，只认这一种写法。
- 签名：Ed25519，签名对象是 `"drama-dossier/license/v1\n" + 载荷字节`。固定前缀用来隔离用途：
  同一把私钥以后签别的东西，那些签名也不能被挪来冒充授权凭证。
- 载荷是一个 JSON 对象（服务端按键名排序、不带空格输出；验签针对原始字节，不要求这种写法）：

  | 字段 | 含义 |
  | --- | --- |
  | `v` | 版本号，目前是 1 |
  | `kid` | 签名公钥的指纹：公钥 SHA-256 的前 16 位十六进制 |
  | `sub` / `dev` | 账号 ID / 机器码 |
  | `ent` / `ent_until` | 使用权类型；`timed` 时 `ent_until` 是使用权到期的 Unix 秒数，其余情况为 `null` |
  | `iat` / `exp` | 签发 / 凭证到期的 Unix 秒数 |

- 桌面端验签按这个顺序，哪一步不过就停在哪一步，并给出对应的错误码：
  格式 `malformed` → 签名 `bad_signature` → 载荷 `bad_payload` → 机器码 `wrong_device` → 有效期 `expired`。
  先验签名再解析载荷：没通过签名的内容，一个字节都不信。
- 凭证有效只说明「这台电脑属于这个账号」。能不能用付费功能还要看 `ent`：
  `lifetime` 能用；`timed` 要当前时间早于 `ent_until`；`none` 不能用。
- 细则以参考实现 `license_token.verify` 和测试向量 `tests/commerce/license_vectors_v1.json`（覆盖全部错误码）为准，
  桌面端（Rust）必须逐条通过。其中几条容易漏：base64url 解码后再编码要和原文一字不差；签名必须正好 64 字节；
  整数字段必须是 JSON 整数且在 0..2^63-1（`true`、`1.0`、`"1"` 都不算）；`exp` 要晚于 `iat`；
  有重复的键、`NaN` 之类的非法常量一律拒绝，不认识的字段忽略；`kid` 要和实际验过签的公钥一致；
  当前时间等于 `exp` 就算过期。
- 测试向量里的签名种子是公开的，任何人都能用它签名：正式环境绝不能信任那里的公钥。
- 离线宽限期：凭证默认 72 小时有效；联网时桌面端到 `refresh_after`（默认签发后 1 小时）就刷新，
  所以断网那一刻剩下的宽限期接近完整的 72 小时。过了 `exp` 必须联网刷新。
- 离线验证只能相信本机时钟：把系统时间往回调，可以拖长宽限期。这是离线授权本身的限制。
  服务端签发只用数据库时间，改本机时钟影响不到签发出来的 `iat`、`exp`。

### 3.4 错误

响应体固定为 `{"error": <机器码>, "message": <中文说明>}`。客户端按 `error` 分支，`message` 可以直接展示。
参数校验失败（422）还会带 `fields: [{"field", "message"}]`，不回显用户提交的值。
个别错误会带附加字段（见下表），客户端遇到不认识的字段忽略即可。

| HTTP | `error` | 含义 |
| --- | --- | --- |
| 400 | `code_invalid` / `code_expired` / `code_exhausted` | 验证码不对 / 过期 / 输错 5 次已锁定（要重新获取） |
| 400 | `wrong_password` | 修改密码时当前密码不对；注销时密码不对 |
| 401 | `invalid_credentials` | 邮箱或密码不正确（两种情况不区分） |
| 401 | `unauthorized` | 没带令牌、令牌无效或已过期、会话已吊销（包括这台电脑被解绑） |
| 403 | `device_not_bound` | 刷新凭证：这台电脑没绑定到当前账号，或已被解绑；生成报审包、签发 / 消费配方时也是这个含义 |
| 403 | `entitlement_required` | 账号还没有使用权，或者使用权已到期（生成报审包、签发 / 消费配方的共同前提） |
| 403 | `wrong_device` | 消费配方：这个配方不是发给这台电脑的 |
| 404 | `email_not_registered` | 重设密码：邮箱没注册 |
| 404 | `card_not_found` / `device_not_found` | 卡密不存在 / 要解绑的电脑不存在或已解绑 |
| 404 | `recipe_not_found` | 消费配方：找不到这个配方（别人的配方和不存在的配方回同一个错误，不透露配方 ID 是否存在） |
| 409 | `email_taken` | 注册：邮箱已被占用 |
| 409 | `card_used` / `card_revoked` | 卡密已被别人用过 / 所在批次已作废 |
| 409 | `already_lifetime` | 账号已经是永久授权，不需要再兑换（卡不会被用掉） |
| 409 | `device_limit` | 名额满了；附带 `devices`、`device_limit`、`unbind_quota` |
| 409 | `recipe_consumed` | 消费配方：这个配方已经用过了，要重新申请 |
| 410 | `recipe_expired` | 消费配方：这个配方已过期，要重新申请 |
| 413 | `payload_too_large` | 请求体太大：一般接口上限 1 MiB；`POST /packages` 上限 1 MiB + 4 份扫描件（见 3.5）。在解析 JSON 之前就拒绝 |
| 422 | `invalid_request` | 参数校验失败；报审包还包括「文档内容太长压到签章位」这类排版错误，`message` 是 Rust 侧给的中文说明 |
| 422 | `scan_invalid` | 生成报审包：扫描件不是合法的 base64、格式不支持、超过 5 MB、页数与原文档不一致，或者给出了不需要盖章的文档路径 |
| 429 | `rate_limited` | 太频繁；`Retry-After` 响应头给出要等的秒数 |
| 429 | `unbind_limit` | 本周期解绑次数用完；附带 `unbind_quota`，`Retry-After` 给出要等的秒数（可能是好几天） |
| 503 | `mail_unavailable` / `service_unavailable` | 发信失败（可立即重试） / 数据库不可用；报审包还包括服务端缺少 `dossier-artifacts`、工具运行失败或超时、签章服务未实现 |

### 3.5 报审包

| 方法 | 路径 | 请求体 / 参数 | 成功 |
| --- | --- | --- | --- |
| POST | `/packages` | 见下 | 200 `{"package_id", "manifest", "signature", "files"}` |

三个前提缺一不可：**登录**（没带令牌或令牌失效是 401 `unauthorized`，见 3.4）、`device_id` 是当前账号
绑定且未解绑的电脑（否则 403 `device_not_bound`）、账号有**有效使用权**（否则 403 `entitlement_required`）。

请求体（字段 snake_case，多一个不认识的字段一律 422）：

| 字段 | 必填 | 说明与上限 |
| --- | --- | --- |
| `device_id` | 是 | 机器码，64 位小写十六进制 |
| `drama_name` | 是 | 剧名，1..200 个字符（Rust 侧还要求能当文件名用、不含 `/`） |
| `ai_generated` | 是 | 只接受 JSON 的 `true` / `false` |
| `copy_episodes` | 是 | 是否把分集视频复制进包；`false` 时只登记哈希 |
| `cost_sheet` | 否 | `company_name` / `contact` 各 ≤ 200 个字符；`total_investment_wan` 0..1e9；`actor_ratio_percent` 0..100 |
| `synopsis` / `cover` | 否 | `file_name` 1..255 个字符、`size_bytes` 0..2^50、`sha256` 64 位小写十六进制 |
| `episodes` | 是 | 最多 **500** 集；每集含 `file_name`、`size_bytes`、`sha256`、可选 `episode_number`（0..100000）和 `spec` |
| `episodes[].spec` | 是 | `duration_sec` 0..86400；`width` / `height` 0..16384；`video_codec` / `audio_codec` 各 ≤ 32 字符；`fps` 0..1000；`bitrate_bps` 0..1e10 |
| `signer` | 否 | `mock`（默认，占位章）/ `scan`（上传盖章扫描件） |
| `scans` | 否 | `signer: scan` 时：文档路径 → 扫描件（PNG / JPEG / PDF）的 base64，每份 ≤ 7,000,000 个字符（约 5 MB 原始字节），最多 **4** 份 |

整个请求体的上限是 1 MiB + 4 × 7,000,000 字节（`commerce/body_limit.py`）：先看 `Content-Length`，
没有或谎报时边收边数，超了立即返回 413 `payload_too_large`，不把请求读进内存。其余接口一律 1 MiB。

`scans` 的键必须是这次要盖章的文档路径（`04_成本配置表/成本配置表.pdf`、
`05_权属声明/内容权属声明.pdf`），给了别的路径返回 422 `scan_invalid`；
用 `signer: mock` 时不允许带 `scans`（同样 422）。

服务端做的事：检查前提 → 调 `dossier-artifacts render-package` 生成文本文件与排版模型 →
把两份文档画成 A4 PDF 并盖章 → 拼清单 `drama-dossier/package@2`、用 Ed25519 签名
（用途前缀 `package-manifest`）→ 在一个事务里记一行 `packages` 和一条审计日志 → 返回。

返回：

```json
{"package_id": "<UUID>",
 "manifest": "<manifest.json 的原样字符串>",
 "signature": "{\"kid\": \"…\", \"sig\": \"…\"}",
 "files": [{"path": "01_剧集信息/剧名.txt", "content": "<base64>"}]}
```

`manifest` 和 `signature` 是**原样字符串**：签名针对的就是这些字节，桌面端必须把原文写回包里、
不能重新序列化。`files` 里是包内路径 → base64 内容，桌面端逐个重算 SHA-256 与清单比对。

- **视频不上传**：分集、封面、简介只传桌面端算好的文件名、大小和 SHA-256，服务端把它们登记进清单，
  组装时由桌面端重新算一遍对比。服务端**不接收也不保存**任何文档内容。
- 占位章（`signer: mock`）：文件名在 `.pdf` 前加 `_未签署`，清单里 `signature.placeholder = true`。
- 失败时不会留下半成品：`dossier-artifacts` 找不到 / 运行失败 / 超时（30 秒）/ 输出不是 JSON
  一律 503 `service_unavailable`，请求本身不合法（退出码 2）返回 422 `invalid_request`，
  `message` 就是 Rust 侧的中文说明。`dossier-artifacts` 对传给它的请求 JSON 有 1 MiB
  （`1 << 20`）上限，超了同样按请求不合法处理。

限流（每账号，固定时间窗）：

| 接口 | 规则 |
| --- | --- |
| `POST /packages` | 每账号每小时 **30** 次 |
| `POST /recipes` | 每账号每小时 **60** 次 |
| `POST /recipes/{id}/consume` | 每账号每小时 **120** 次 |

### 3.6 处理配方

处理配方（M4a 服务端接口，M4b 才接桌面界面）由服务端签发参数，绑定机器码和任务，**只能消费一次**。

| 方法 | 路径 | 请求体 | 成功 |
| --- | --- | --- | --- |
| POST | `/recipes` | `device_id`、`task`、`platform`、`strength` | 200 `{"recipe_id", "token", "expires_at", "params"}` |
| POST | `/recipes/{recipe_id}/consume` | `device_id` | 200 `{"recipe_id", "consumed": true}` |

- 两个接口都要求登录、这台电脑绑定在当前账号上、使用权有效（与 3.5 同样的三个前提）。
- 请求字段：`device_id` 是 64 位小写十六进制机器码；`task` 是**任务描述的哈希**，64 位小写十六进制；
  `platform` 只能是 `universal`（只生成一套通用版本，见 3.6 的平台规格）；
  `strength` 只能是 `light` / `medium` / `heavy`。

**任务哈希（`task`）**：由桌面端计算后传给服务端；服务端只把它绑进配方并原样回填，**不解析也不校验内容**。
规则（`dossier-license::version_task`）：

- `task = SHA-256( 规范化 JSON )`，JSON 只含这几项，键按字母序、无空白（非 ASCII 字符按 UTF-8 原样输出）：

```json
{"episodes":[{"name":"第01集.mp4","sha256":"…"}],"rights":"<权属材料 sha256>","schema":"version-task@1","strength":"medium","template":"<模板哈希>"}
```

- `episodes`：按文件名升序排列；`name` 是文件本身的名字（不带目录）；`sha256` 是文件内容的 SHA-256（小写十六进制）。
- `rights`：权属材料文件内容的 SHA-256（同上）。没声明权属材料就没有任务哈希，也就申请不了配方。
- `template`：第一层模板 JSON 的哈希。算法：把模板解析成 JSON 对象，按同样的规则规范化（键排序、无空白、UTF-8 原样）后取 SHA-256。
- `strength`：`light` / `medium` / `heavy`。
- 换一集、改模板、换档位、换权属材料，哈希就变，本机验签（`wrong_task`）就不过。
- Python 参考实现（对拍用）在 `cloud/tests/commerce/version_task.py`。

`token` 是 `base64url(载荷 JSON) + "." + base64url(签名)`，签名用途前缀 `recipe`（和授权凭证、报审包清单
互不冒充）。载荷字段与 `dossier-license::recipe` 一致：`v`（版本号，目前 1）、`kid`、`rid`（配方 ID）、
`sub`（账号 ID）、`dev`（机器码）、`task`、`platform`、`strength`、`seed`、`params`、`iat`、`exp`。
桌面端本地验签顺序是 格式 → 签名 → 载荷 → 机器码 → 任务 → 有效期，错误码分别是
`malformed` / `bad_signature` / `bad_payload` / `wrong_device` / `wrong_task` / `expired`。

**有效期**：`RECIPE_TTL_SECONDS = 24 × 3600`，即签发后 **24 小时**内要开工。桌面端判断「当前时间等于
`exp`」即算过期。

**只能消费一次**：由服务端记账。消费时先锁住这一行，再按顺序检查——找不到（别人的配方也是这个结果）
→ `recipe_not_found`（404）→ 机器码不对 `wrong_device`（403）→ 已消费 `recipe_consumed`（409）
→ 已过期 `recipe_expired`（410）→ 再查一次使用权（403 `entitlement_required`）→ 记 `consumed_at`。
第二次消费一定失败；只靠本地记录挡不住重放（把本地记录删掉就能再用一次），所以必须在服务端记账。
`recipes` 表里记着 `seed` 和平台 / 强度，配合 `derive_params` 就能算出同样的参数，处理结果可以复现。

#### 各强度的参数上下限

`params.subtle` 的每一项都在该档的上下限内取值（同一 `seed` 永远算出同样的参数）。
`brightness` / `hue_degrees` / `pitch_semitones` 是**偏移量**：上下限描述幅度，正负号随机。

| 参数 | 含义 | light | medium | heavy |
| --- | --- | --- | --- | --- |
| `grain` | ffmpeg `noise` 滤镜 alls 强度（0–100） | 1–3 | 3–6 | 6–10 |
| `zoom_percent` | 放大再裁回原尺寸的比例（%） | 1.0–1.5 | 1.5–2.2 | 2.2–3.0 |
| `brightness` | `eq` 亮度偏移（-1–1） | 0–0.01 | 0–0.02 | 0–0.03 |
| `saturation` | 饱和度倍数 | 0.98–1.02 | 0.96–1.04 | 0.94–1.06 |
| `hue_degrees` | 色相偏移（度） | 0–1.0 | 0–2.0 | 0–3.0 |
| `tempo` | 整体变速倍数（画面和声音一起） | 0.99–1.01 | 0.98–1.02 | 0.97–1.03 |
| `pitch_semitones` | 变调（半音） | 0–0.1 | 0–0.2 | 0–0.3 |
| `trim_head_frames` | 片头裁几帧 | 1–2 | 2–3 | 3–5 |
| `trim_tail_frames` | 片尾裁几帧 | 1–2 | 2–3 | 3–5 |
| `quality.min_ssim` | 画质损失下限（M4b 检测用） | 0.97 | 0.92 | 0.84 |
| `quality.min_psnr` | 画质损失下限（M4b 检测用） | 38.0 | 35.0 | 32.0 |

#### 平台规格（第一层 `params.platform`）

只生成**一套通用版本**（`platform = "universal"`），生成后由用户自己决定发到哪里：取值是抖音 / 快手 /
视频号三家公开要求里**最严**的值，三家各自的数值见 `platform_specs.py` 的注释（只有视频码率三家不同：
抖音 6000kbps、快手 6000kbps、视频号 5000kbps，取最严的 5000kbps）。

**保守值，待复核**：平台随时会改规则，M4b 动手处理视频前要按官方帮助中心复核一次。

| 平台 | 分辨率 | 帧率 | 视频码率 | 音频码率 | 单集时长上限 | 封装 / 编码 |
| --- | --- | --- | --- | --- | --- | --- |
| `universal` | 1080×1920 | 30 | 5000 kbps | 128 kbps | 900 秒 | mp4 / h264 / aac |

整理日期 **2026-10-15**，来源（`platform_specs.py` 的 `SOURCES`）：

- <https://www.kuaishou.com/help/feedback/2730>
- <https://www.kuaishou.com/help/feedback/4000>
- <https://developers.weixin.qq.com>
- <https://developer.open-douyin.com/docs/resource/zh-CN/dop/develop/openapi/video-management/video-upload/upload-video>

状态：**待复核**——抖音、视频号没有官方原文，其余是二手转述，M4b 前需按官方帮助中心再核一次。

## 4. 安全规则

- **密码**：argon2id，参数用 argon2-cffi 的默认值（RFC 9106 低内存档：t=3、m=64 MiB、p=4）。
  以后调高参数，老用户下次登录时自动升级。数据库里的哈希损坏时按登录失败处理。
  同时进行的哈希数有上限（默认 4 个）：一大波并发登录只会排队，不会把内存撑爆；
  排队太久、拿不到数据库连接的请求返回 503。
- **验证码**：6 位数字。60 秒内不能重发，10 分钟过期，最多试 5 次，用一次就作废，重发后旧码失效。
  数据库只存 HMAC，算的时候带上用途和邮箱。邮件没发出去就删掉这条验证码，不占冷却时间。
- **令牌**：访问令牌 15 分钟；刷新令牌 30 天，只能用一次，每次刷新换一对新令牌，同时顺延会话有效期。
  - 旧刷新令牌被再次使用，就吊销整个会话（不论是被偷了还是客户端并发刷新）。
    **桌面端必须保证同一时刻只发一个刷新请求**，否则会把自己踢下线。
  - 数据库只存令牌的 SHA-256 摘要。
  - 令牌只认 `Authorization: Bearer` 请求头，不接受网址参数：网址会被写进访问日志和浏览器历史。
    媒体控制面的用户令牌同样已经去掉了 `?token=`。
- **吊销**：退出吊销当前会话；重设密码吊销全部会话；修改密码保留当前会话、吊销其他会话。
- **限流**：计数存在 PostgreSQL 里，多个服务实例共用。被拒绝的请求也算一次。

  | 接口 | 规则 |
  | --- | --- |
  | 发验证码 | 每 IP 每小时 20 次；每邮箱每小时 10 次（另有 60 秒冷却） |
  | 注册 | 每 IP 每小时 20 次 |
  | 登录 | 每 IP 每 10 分钟 30 次；每邮箱每 10 分钟 10 次 |
  | 重设密码 | 每 IP 每小时 20 次 |
  | 修改密码 | 每账号每小时 10 次 |
  | 注销账号 | 每账号每小时 5 次（要校验密码，限住次数，偷来的访问令牌也没法靠它猜密码） |
  | 导出个人数据 | 每账号每小时 10 次 |
  | 兑换卡密 | 每 IP 每小时 60 次；每账号每小时 20 次 |
  | 绑定电脑 | 每账号每小时 20 次 |
  | 刷新凭证 | 每账号每小时 120 次 |
  | 设备列表、解绑 | 合计每账号每小时 60 次（解绑另有 30 天 3 次的上限） |
  | 生成报审包 | 每账号每小时 30 次 |
  | 签发处理配方 | 每账号每小时 60 次 |
  | 消费处理配方 | 每账号每小时 120 次 |

  限流键是 IP、邮箱的 HMAC，数据库里不出现明文。卡密有 80 位随机数，靠猜是猜不中的，
  兑换的限流不是为了防猜，而是挡住脚本刷接口。
- **账号是否存在**：注册时邮箱被占用会明确提示（409），重设密码时邮箱没注册也会明确提示（404）。
  注册接口本来就会暴露这一点，在别处隐藏没有意义，提示清楚对用户更友好；靠上面的限流挡批量探测。
  登录失败不区分邮箱不存在还是密码错误，邮箱不存在时也做一次同等开销的哈希校验。
- **卡密**：16 位，字符集去掉了容易看错的 0、1、I、O，每张 80 位随机数。数据库只存
  `SHA-256("drama-dossier/card/v1\n" + 卡密)`，明文只出现在生成时导出的 CSV 里：数据库泄露了也拿不到能兑换的卡密。
  这个摘要算法一改，库里所有没兑换的卡全部失效，所以用测试钉死了。
- **授权签名私钥**：只放在服务器上（权限 600，权限过宽时启动日志会警告），不进仓库，也不进环境变量。
  泄露了，任何人都能给自己签永久授权；丢了，要换新密钥并发布内置新公钥的桌面端（见 5.1）。
- **签名用途隔离**：同一把私钥签三种东西，签名对象前面各加一个固定前缀
  （`drama-dossier/license/v1\n`、`drama-dossier/package-manifest/v1\n`、`drama-dossier/recipe/v1\n`），
  所以授权凭证的签名挪去冒充报审包清单或处理配方都验不过，反过来也一样。
- **报审包只存元数据**：`packages` 表只有 ID、账号、机器码、生成时间、签章方式、集数与
  `manifest.json` 原始字节的 SHA-256；**服务端不保存任何文档内容**（PDF、文本、封面、简介、视频都不存），
  生成完就直接发回桌面端。清单和签名也由桌面端在验签后写进本地包里。
- **审计日志不记原文**：生成报审包只记一条 `package.create`，内容是**剧名的 SHA-256 前 16 位**、
  集数和签章方式；不记剧名、公司名、联系方式原文。签发 / 消费配方记 `recipe.issue`（平台、强度）和
  `recipe.consume`；任务哈希和机器码都不进审计日志的明细。
- **扫描件**：只在请求体里出现，`scan_invalid` 时立即返回；上限 5 MB、像素上限 4000 万（按文件头判断
  类型，不信扩展名），防止「解压炸弹」。不是通用文件上传接口，路径必须落在这次要盖章的文档上。
- **机器码**：桌面端只上传摘要，接口也只收 64 位十六进制，服务端拿不到硬件原始信息。
  机器码是桌面端自己算的，改过的客户端可以冒用别的机器码，服务端分辨不出来：
  名额限制挡的是随手共享账号，挡不住有意破解；解绑次数上限和审计日志用来限制、发现「轮流换电脑」。
- **审计日志**：注册、登录、退出、重设密码、修改密码、注销、刷新令牌重放，以及生成和作废卡密批次、兑换卡密、
  绑定和解绑电脑、生成报审包、签发和消费配方都会记录。只记 ID、计数、卡的类型和天数、电脑平台、
  使用权的前后变化，不记邮箱、令牌、验证码、卡密、授权凭证和 IP。
  卡密批次的操作人记 `--by`，没给就记当前系统用户名。
- 所有时间判断都用数据库的 `now()`，不依赖应用服务器的时钟；授权凭证的签发和到期时间也取数据库时间。

## 5. 运维

- `drama-commerce migrate`：按编号执行 `src/drama_cloud/commerce/migrations/` 下的 SQL，每个迁移一个事务，
  用咨询锁防止两个进程同时迁移。已执行的迁移被改动、或数据库里有程序不认识的版本，都会报错停止。
  新改动写成新的编号文件。
- 服务启动时检查迁移、授权私钥和**中文字体**：数据库没迁移到最新、私钥读不到或不是 Ed25519 私钥、
  字体找不到，都拒绝启动。
- 部署机要装**中文字体**（报审包的两份 PDF 要嵌入简体中文）：Debian / Ubuntu 上装 `fonts-noto-cjk`
  （`apt install fonts-noto-cjk`），或把 `DRAMA_COMMERCE_PDF_FONT` 指向一个 `.otf` / `.ttf` / `.ttc`
  文件。找不到时日志报「找不到报审包 PDF 要用的中文字体」并**拒绝启动**——不生成一份方块字的 PDF。
  `.ttc` 合集里会取简体中文那一款（Noto Sans CJK SC / Noto Sans SC / Source Han Sans SC / CN），
  拆成单独的字体文件交给 fpdf2，进程退出时删掉。
- 报审包还依赖 Rust 侧的 `dossier-artifacts`：用 `DRAMA_COMMERCE_ARTIFACTS_BIN` 指定，
  或让它在 `PATH` / 仓库的 `target/release`、`target/debug` 里能被找到。
  它缺失时服务能启动，但 `POST /packages` 会返回 503 `service_unavailable`。
- `drama-commerce cleanup`：清理过期的验证码、令牌、会话和限流计数，建议每天跑一次（cron 或 systemd timer）。
  审计日志、卡密、电脑绑定、报审包记录和处理配方记录都不在这里清理：解绑次数要从绑定历史里数。
  会话被清掉后，绑定上记的会话变成空，绑定本身不受影响。
- 对外部署：必须在反向代理上终止 HTTPS，并把 `DRAMA_COMMERCE_TRUSTED_PROXIES` 设成实际的代理层数，
  否则限流拿到的都是代理的地址。SMTP 用 `starttls` 或 `ssl`。
- `/docs`、`/openapi.json` 是 FastAPI 自带的接口文档，对外部署时可以在反向代理上屏蔽。

### 5.1 授权签名密钥

```bash
drama-commerce keygen /etc/drama-commerce/license.pem    # 生成私钥（权限 600）；文件已存在就拒绝，绝不覆盖
drama-commerce keyinfo /etc/drama-commerce/license.pem   # 查看已有私钥对应的 kid 和公钥
```

两条命令都打印 `{"kid", "public_key", "path"}`。`public_key` 是 32 字节公钥的 base64url，要内置到桌面端。
私钥文件权限是 600，只有文件属主能读：用运行服务的系统账号执行 `keygen`，或者生成后 `chown` 给它。

验签支持同时信任多把公钥（参考实现的 `trusted_keys` 是列表），桌面端要照样实现，换密钥才不用让所有人掉线：

1. 新密钥用 `keygen` 生成到新路径，先不启用。
2. 发布同时信任新旧两把公钥的桌面端，等用户升级。
3. 把 `DRAMA_COMMERCE_LICENSE_KEY_FILE` 指向新私钥并重启服务。旧凭证最多再有效 `LICENSE_TTL_HOURS`（默认 72 小时）。
4. 下一个桌面端版本去掉旧公钥。

私钥泄露时不能走这个平滑流程：要尽快发布只信任新公钥的桌面端，同时换掉服务端私钥。

### 5.2 卡密

```bash
drama-commerce cards generate --days 30 --count 500 --out 月卡-第一批.csv --name 月卡-第一批 --by 张三
drama-commerce cards generate --lifetime --count 20 --out 永久卡.csv
drama-commerce cards list [--limit 50]                    # 最近的批次：总数、已兑换数、是否作废
drama-commerce cards status DD-XXXX-XXXX-XXXX-XXXX        # unused / redeemed / revoked / not_found
drama-commerce cards revoke --batch <批次 ID> --yes        # 作废这一批里还没兑换的卡密，不能撤销
```

- 只需要 `DRAMA_COMMERCE_DSN`。结果是 JSON，打印到标准输出（`list` 每行一个批次）；提示打印到标准错误。
- 退出码：0 成功；1 `status` 查不到这张卡，或者卡密已入库但 CSV 没能改成正式文件名（报错里给出临时文件路径）；
  2 参数、配置或数据库错误。
- `generate`：一批 1..10000 张，`--days` 1..3650；`--name` 默认按类型和时间生成，`--by` 默认当前系统用户。
  CSV 是 UTF-8（带 BOM，Excel 能直接打开），权限 600，列为 `code, kind, days, batch_name`。
- **CSV 是卡密明文的唯一副本**：丢了找不回来；泄露了用 `revoke` 作废整批。
  目标文件已存在就拒绝生成；写库失败不会留下明文文件。
- `revoke` 不影响已经兑换的卡，已经加上的使用权照常有效。

## 6. 测试

```bash
cd cloud
export COMMERCE_TEST_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres'   # 需要建库权限
export COMMERCE_TEST_MAILPIT='http://127.0.0.1:8025'                                   # 可选：真实 SMTP 端到端
.venv/bin/pytest tests/commerce -q
```

- 每次测试会话新建一个临时库并执行迁移，结束后删掉，不碰开发库。
- 没设 `COMMERCE_TEST_DSN` 时，数据库测试自动跳过，只跑单元测试。
- 测试用低开销的 argon2 参数，只为跑得快；生产用默认参数。授权签名私钥每次测试会话现生成，不进仓库。
- 授权凭证的测试向量 `tests/commerce/license_vectors_v1.json` 由 `test_license_token.py` 里的 `build_vectors()`
  确定性生成并提交进仓库，桌面端用同一份文件对拍。平时测试只核对文件和生成器一致。
  只有真的改了格式或用例，才带 `UPDATE_LICENSE_VECTORS=1` 重新生成，并和桌面端的改动一起提交。
  日常回归和最终验证都不要带这个变量：它会先覆盖文件再比较，等于跳过了这项检查。
