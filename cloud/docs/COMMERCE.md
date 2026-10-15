# 账号与授权服务（drama_cloud.commerce）

这个服务管两件事：账号（注册、登录、邮件验证码、令牌），和授权（卡密兑换、使用权、电脑绑定、
可以离线验证的授权凭证）。它和媒体控制面（`drama-cloud`）分开部署：处理任务再重，也不拖慢登录。
数据库用 PostgreSQL，因为卡密兑换、抢设备名额和以后的支付回调都需要可靠的并发事务。

- M1（已完成）：邮件验证码、注册、登录、刷新与吊销令牌、重设密码、修改密码、集中限流、审计日志。
- M2（本期）：卡密批量生成与兑换、使用权、电脑绑定与解绑、授权凭证的签发与刷新、管理命令。
- 桌面端接入（钥匙串、本机机器码、本地验签、离线提示）在 M3。

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

配置缺失或非法时，服务拒绝启动，报错里只出现变量名，不出现变量值。
`migrate`、`cleanup`、`cards` 只需要 `DRAMA_COMMERCE_DSN`；`keygen`、`keyinfo` 不读任何配置。

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

`tokens` 的结构：

```json
{"access_token": "at_…", "refresh_token": "rt_…", "token_type": "Bearer",
 "expires_in": 900, "refresh_expires_in": 2592000}
```

- 邮箱会去掉首尾空格并转成小写，只接受 ASCII 邮箱。密码 8..128 个字符；登录时不检查长度下限。
- `accept_terms` 必须是 JSON 的 `true`，`"true"`、`1` 都不算。
- 请求体里多出来的字段一律拒绝。

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
| 400 | `wrong_password` | 修改密码时当前密码不对 |
| 401 | `invalid_credentials` | 邮箱或密码不正确（两种情况不区分） |
| 401 | `unauthorized` | 没带令牌、令牌无效或已过期、会话已吊销（包括这台电脑被解绑） |
| 403 | `device_not_bound` | 刷新凭证：这台电脑没绑定到当前账号，或已被解绑 |
| 404 | `email_not_registered` | 重设密码：邮箱没注册 |
| 404 | `card_not_found` / `device_not_found` | 卡密不存在 / 要解绑的电脑不存在或已解绑 |
| 409 | `email_taken` | 注册：邮箱已被占用 |
| 409 | `card_used` / `card_revoked` | 卡密已被别人用过 / 所在批次已作废 |
| 409 | `already_lifetime` | 账号已经是永久授权，不需要再兑换（卡不会被用掉） |
| 409 | `device_limit` | 名额满了；附带 `devices`、`device_limit`、`unbind_quota` |
| 422 | `invalid_request` | 参数校验失败 |
| 429 | `rate_limited` | 太频繁；`Retry-After` 响应头给出要等的秒数 |
| 429 | `unbind_limit` | 本周期解绑次数用完；附带 `unbind_quota`，`Retry-After` 给出要等的秒数（可能是好几天） |
| 503 | `mail_unavailable` / `service_unavailable` | 发信失败（可立即重试） / 数据库不可用 |

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
  | 兑换卡密 | 每 IP 每小时 60 次；每账号每小时 20 次 |
  | 绑定电脑 | 每账号每小时 20 次 |
  | 刷新凭证 | 每账号每小时 120 次 |
  | 设备列表、解绑 | 合计每账号每小时 60 次（解绑另有 30 天 3 次的上限） |

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
- **机器码**：桌面端只上传摘要，接口也只收 64 位十六进制，服务端拿不到硬件原始信息。
  机器码是桌面端自己算的，改过的客户端可以冒用别的机器码，服务端分辨不出来：
  名额限制挡的是随手共享账号，挡不住有意破解；解绑次数上限和审计日志用来限制、发现「轮流换电脑」。
- **审计日志**：注册、登录、退出、重设密码、修改密码、刷新令牌重放，以及生成和作废卡密批次、兑换卡密、
  绑定和解绑电脑都会记录。只记 ID、计数、卡的类型和天数、电脑平台、使用权的前后变化，
  不记邮箱、令牌、验证码、卡密、授权凭证和 IP。
  卡密批次的操作人记 `--by`，没给就记当前系统用户名。
- 所有时间判断都用数据库的 `now()`，不依赖应用服务器的时钟；授权凭证的签发和到期时间也取数据库时间。

## 5. 运维

- `drama-commerce migrate`：按编号执行 `src/drama_cloud/commerce/migrations/` 下的 SQL，每个迁移一个事务，
  用咨询锁防止两个进程同时迁移。已执行的迁移被改动、或数据库里有程序不认识的版本，都会报错停止。
  新改动写成新的编号文件。
- 服务启动时检查迁移和授权私钥：数据库没迁移到最新、私钥读不到或不是 Ed25519 私钥，都拒绝启动。
- `drama-commerce cleanup`：清理过期的验证码、令牌、会话和限流计数，建议每天跑一次（cron 或 systemd timer）。
  审计日志、卡密、电脑绑定都不在这里清理：解绑次数要从绑定历史里数。
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
