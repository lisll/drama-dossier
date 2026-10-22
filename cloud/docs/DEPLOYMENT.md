# 部署手册（单机 / 局域网）

> 面向运维。讲清楚在一台机器上用 Docker Compose 把整套服务拉起来的完整步骤、
> 两套 compose 为什么分开、备份与恢复、更新流程，以及这套部署**明确不做**什么。
>
> 相关文档：管理后台的操作见 [`ADMIN_GUIDE.md`](ADMIN_GUIDE.md)；
> 签名密钥的轮换见 [`../../docs/KEY_ROTATION.md`](../../docs/KEY_ROTATION.md)；
> commerce 的接口与配置见 [`COMMERCE.md`](COMMERCE.md)。

## 1. 这套部署是什么

`cloud/compose.deploy.yaml` 里四个服务：

| 服务 | 镜像 | 端口 | 干什么 |
| --- | --- | --- | --- |
| `postgres` | `postgres:18.6-alpine` | **不映射** | 两个库：`drama_commerce` 与 `drama_media` |
| `commerce` | 本仓库 `cloud/Dockerfile` 构建 | **不映射** | 账号、授权、积分、支付、管理后台（`:8100`） |
| `media` | 同一个镜像 | **不映射** | 媒体控制面 + 它自己的 Web 客户端（`:8000`） |
| `caddy` | `caddy:2.8-alpine` | `80` / `443` | 唯一的对外入口，终结 TLS，按前缀分发 |

只有 `caddy` 暴露端口。另外三个只在 compose 的内部网络里互相可见——这是刻意的：
少一个可以误暴露的入口。

**两个服务共用一个镜像**（`cloud/Dockerfile` 的注释）：`drama_cloud` 这个包同时提供
`drama-commerce` 与 `drama-cloud` 两个入口，拆两个镜像只是把同一个东西装两遍，靠 `command` 区分。

**两个库不是可选的**：commerce 的 `users.id` 是 uuid，媒体侧的 `projects.owner_id` 是 text，
混在一个库里建表会直接撞在外键类型上（`cloud/scripts/initdb.sql`）。第二个库由
`/docker-entrypoint-initdb.d` 里的初始化脚本在**数据卷为空时**建一次。

## 2. 前置条件

- **Docker** 与 **Docker Compose v2**（`docker compose` 子命令形式，不是老的 `docker-compose`）。
  备份脚本用的是 `docker compose exec -T` 与 `docker compose cp`。
- 一台能跑四个容器的机器（同一台机器上装，就是「单机部署」）。
- 一个局域网 IP 或内网域名。后面 Caddy 用它做站点名，桌面端用它拼支付回调地址。
- 端口 80 / 443 可用（或改 `.env` 里的 `HTTP_PORT` / `HTTPS_PORT`）。
- 出网能力：`docker compose build` 要拉基础镜像与 pip 依赖；SMTP 要能连出去（发验证码）。

不需要的东西：真实证书（用自签）、对象存储、Redis、日志聚合。这三样都不在本期范围内。

## 3. 步骤

以下命令都在 `cloud/` 目录下执行。

### 3.1 复制配置模板

```bash
cd cloud
cp .env.example .env
chmod 600 .env      # 里面有密码
```

`.env` **不进仓库**（`.gitignore` 已排除）。模板里没有真实密码，逐项填：

| 变量 | 怎么填 |
| --- | --- |
| `POSTGRES_USER` / `POSTGRES_PASSWORD` | 库账号与密码。密码用 `openssl rand -base64 24` |
| `INTERNAL_SECRET` | 两个服务之间的共享密钥，**必须与 media 侧同一个值**。`openssl rand -base64 32` |
| `CODE_SECRET` | 验证码与限流键的 HMAC 密钥，**至少 32 字符**。`openssl rand -base64 48` |
| `DEPLOY_HOST` | 局域网 IP 或内网域名，例如 `drama.local` |
| `PUBLIC_BASE_URL` | 对外地址，例如 `https://drama.local`。支付回调要靠它 |
| `KEYS_DIR` | 放 `license.pem` 的**宿主机目录**（相对 `cloud/` 或绝对路径） |
| `TLS_DIR` | 放 `cert.pem` / `key.pem` 的宿主机目录 |
| `FONTS_DIR` / `PDF_FONT` | 报审包 PDF 用的中文字体；留空则用镜像里的 Noto CJK |
| `SMTP_HOST` / `SMTP_PORT` / `SMTP_SECURITY` / `SMTP_USERNAME` / `SMTP_PASSWORD` / `SMTP_FROM` | 发验证码的信箱 |
| `MOCK_PAY_SECRET` | 模拟支付的签名密钥。**留空时模拟收银台不可用**（空密钥等于谁都能伪造回调） |
| `LICENSE_KEY_SECONDARY` | 只在密钥轮换过渡期填，平时留空 |
| `ASSET_RETENTION_DAYS` / `UPLOAD_RETENTION_DAYS` / `ARTIFACT_RETENTION_DAYS` | 媒体侧的保留期，默认 30 / 7 / 30 |

`POSTGRES_PASSWORD`、`CODE_SECRET`、`INTERNAL_SECRET`、`SMTP_HOST`、`PUBLIC_BASE_URL`、`KEYS_DIR`、`TLS_DIR`
这七项在编排文件里用的是 `${VAR:?提示}` 语法（`compose.deploy.yaml:27`、`53`、`54`、`55`、`64`、`69`、`113`）：
**没填就直接报错停下**，不会用一个空值把服务拉起来。

### 3.2 生成签名私钥

```bash
mkdir -p keys && chmod 700 keys
docker compose -f compose.deploy.yaml run --rm --no-deps \
  --entrypoint drama-commerce commerce keygen /keys/license.pem
```

- 打印三样东西：`kid`、`public_key`（base64url）、`path`。**`public_key` 要内置/注入到桌面端**，
  否则客户端验不过任何凭证（见 [`KEY_ROTATION.md`](../../docs/KEY_ROTATION.md) 3.1）。
- 私钥文件权限 600，目录以**只读**方式挂进容器（`compose.deploy.yaml:69` 的 `:ro`）：
  容器被攻破也改不了它。
- **必须用上面的容器内命令生成**：容器进程是以 `appuser`（uid 10001）跑的，宿主 root 生成的
  600 私钥它读不到，服务会在启动自检时报 `读不到授权签名私钥（PermissionError）` 后退出。
  已经这样生成了的话：`chown 10001:10001 <私钥>` 再重启。
- 私钥**丢了**要换新钥并发布内置新公钥的桌面端；**泄露了**任何人都能给自己签永久授权（见第 8 节）。

### 3.3 生成自签证书

局域网没有公网域名，申请不到真证书，自签是总方案明确的方案。Caddy 用 `tls` 指令加载本机文件，
不用 ACME（`cloud/Caddyfile` 的头部注释）。

```bash
mkdir -p tls && chmod 700 tls
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -keyout tls/key.pem -out tls/cert.pem \
  -subj "/CN=${DEPLOY_HOST:-drama.local}" \
  -addext "subjectAltName=DNS:${DEPLOY_HOST:-drama.local},IP:127.0.0.1"
chmod 600 tls/key.pem
```

`-subj` 里的 CN 与 SAN 要包含你实际访问用的名字（域名或 IP），否则浏览器会因为
「名字不匹配」报错（比单纯「不受信任」更难解释）。

> 仓库里没有 `make cert` 之类的封装（`compose.deploy.yaml` 头部注释里提到的 `make -C . cert`
> 目前并不存在），所以证书就用上面这条 `openssl` 命令生成。

### 3.4 数据库迁移（已自动化，了解原理即可）

**这一步不能省**：`drama-commerce serve` 启动时会检查迁移，数据库没迁移到最新就**直接拒绝启动**，
报「数据库还没有迁移到最新，缺少 …。请先运行 drama-commerce migrate」
（`cloud/src/drama_cloud/commerce/api.py:97-101`）。

编排里有一个一次性的 `migrate` 服务专门做这件事（`compose.deploy.yaml`）：它跑
`drama-commerce migrate && drama-cloud initdb`，然后退出；`commerce` 与 `media` 都用
`depends_on: condition: service_completed_successfully` 等它**成功退出**才启动。
所以 `up -d --wait` 会自动先迁移、再起服务，不需要手工介入。

> 为什么用 `&&` 而不是 `;`：任何一条迁移失败都要让容器以非零退出，
> 服务才不会带着半套表起来。

> 媒体控制面也可以自己建表（`drama-cloud` 的 `cmd_serve` 里先 `init_db` 再起 uvicorn，
> `cloud/src/drama_cloud/cli.py`）。但让它与 commerce 一起在 `migrate` 里跑完更清楚：
> 两个库都在服务接受请求之前就绪。

如果确实需要手工跑（例如排查迁移失败）：

```bash
docker compose -f compose.deploy.yaml run --rm --no-deps migrate
```

`migrate` 按编号执行 `cloud/src/drama_cloud/commerce/migrations/` 下的 SQL，每个迁移一个事务，
用咨询锁防止两个进程同时迁移；已执行的迁移被改动、或数据库里有程序不认识的版本，都会报错停止。

### 3.5 起整套服务

```bash
docker compose -f compose.deploy.yaml up -d --wait
```

`--wait` 会等到所有**配了健康检查**的服务通过才返回。这套编排里只有 `postgres` 有 `healthcheck`
（`pg_isready`，`compose.deploy.yaml:36-40`）；`commerce` / `media` 通过 `depends_on: service_healthy`
等它 healthy 之后才启动，但它们自己**没有健康检查**，所以 `--wait` 返回不代表两个应用服务已经能干活——
下一步的验收（3.7）才是真的检查。

### 3.6 建管理员

```bash
docker compose -f compose.deploy.yaml exec commerce \
  drama-commerce admin create --email ops@example.com
```

密码从标准输入读（交互时用 `getpass` 提示两次），**至少 12 个字符**。
管理后台在 `https://<DEPLOY_HOST>/admin`，**没有注册入口**——管理员只能这样创建。
详见 [`ADMIN_GUIDE.md`](ADMIN_GUIDE.md)。

### 3.7 验收

```bash
# 1. commerce 活着（走 Caddy 的 HTTPS，自签证书用 -k）
curl -k https://localhost/api/v1/health          # {"ok":true}

# 2. 公开的客户端配置接口（不需要登录，不含用户数据）
curl -k https://localhost/api/v1/client/config

# 3. 内部接口从外面进不来
curl -k -o /dev/null -w '%{http_code}\n' https://localhost/internal/introspect   # 404

# 4. 管理后台页面在
curl -k -o /dev/null -w '%{http_code}\n' https://localhost/admin                  # 200
```

第 3 条是关键：`/internal/*` 在 Caddy 里被显式 `respond 404`（`cloud/Caddyfile:58-62`），
**任何**外部请求都拿不到，不管有没有带密钥。

## 4. 两套 compose 的区别，以及为什么分开

| | `compose.dev.yaml` | `compose.deploy.yaml` |
| --- | --- | --- |
| 给谁用 | 开发者本机 | 部署方（单机 / 局域网） |
| 项目名 | `drama-dossier-dev` | `drama-dossier` |
| 起什么 | 只有 `postgres` 与 `mailpit` | `postgres` + `commerce` + `media` + `caddy` |
| 端口 | 只绑 `127.0.0.1`（5432 / 1025 / 8025） | 只有 `caddy` 暴露 80 / 443 |
| 密码 | 固定的开发值 `drama-dev-only` | 从 `.env` 读，缺了拒绝启动 |
| 邮件 | Mailpit，验证码在 <http://127.0.0.1:8025> 看 | 真实 SMTP |
| 应用进程 | **不容器化**，在本机用 `drama-commerce serve` 跑 | 容器化，`command` 区分两个服务 |
| 数据 | 一次性，`down -v` 就清空 | 落在磁盘卷上，备份脚本能直接打包 |
| TLS | 不需要（本机明文） | Caddy + 自签证书 |

**为什么分开**：

- 开发环境的目标是「改一行代码立刻看到效果」，所以应用不进容器（否则每次改动都要 rebuild），
  且端口只绑回环——开发机上不该有对外的服务；
- 部署环境的目标是「别人能用、密钥不落在仓库里」，所以密码与私钥全部外部注入、
  对外只有一个入口、数据落在卷上；
- 一份 compose 同时满足这两个目标只会两头不讨好：要么开发时每次 rebuild，
  要么部署时把开发用的固定密码和 Mailpit 一起带上去。固定密码上生产是最典型的事故来源。

顺带一个具体差别：部署环境里的 `postgres` 还挂了一个初始化脚本
（`compose.deploy.yaml:33-35`、`cloud/scripts/initdb.sql`），在第一卷为空时多建一个媒体库；
开发环境只有一个库，因为开发时两个服务的库是手工建的。

## 5. Caddy 的路由表

`cloud/Caddyfile` 里所有 `handle` 按**出现顺序**匹配，第一条命中就 stop：

| 前缀 | 去哪 | 说明 |
| --- | --- | --- |
| `/api/v1/admin/*` | `commerce:8100` | 管理后台的接口 |
| `/api/v1/auth/*` | `commerce:8100` | 注册、登录、验证码、令牌 |
| `/api/v1/credits*` | `commerce:8100` | 余额、流水 |
| `/api/v1/payments*` | `commerce:8100` | 下单、查单、渠道回调 |
| `/api/v1/invites` | `commerce:8100` | 邀请码 |
| `/api/v1/cards/*` | `commerce:8100` | 兑换卡密 |
| `/api/v1/license/*` | `commerce:8100` | 授权凭证签发与刷新 |
| `/api/v1/devices*` | `commerce:8100` | 电脑绑定与解绑 |
| `/api/v1/me*` | `commerce:8100` | 个人资料 |
| `/api/v1/client/*` | `commerce:8100` | **公开**的客户端配置与错误上报 |
| `/pay/*` | `commerce:8100` | 模拟收银台页面（部署时可删掉这一段） |
| `/admin` | `commerce:8100` | 管理后台的**页面** |
| `/internal/*` | `respond 404` | 内部接口，**不对外** |
| 其余 | `media:8000` | 媒体控制面与它自己的 Web 客户端 |

其它几条：

- **`admin off` 与 `auto_https off`**（`Caddyfile:12-15`）：这套部署不需要在运行时改 Caddy 配置
  （少一个入口少一分风险），也不需要自动申请证书（局域网没有公网域名）。证书由 `tls /certs/cert.pem /certs/key.pem` 指定。
- **`request_body { max_size 64MB }`**：分片上传是 8 MiB 一块，64 MB 留足余量。
- 明文端口（`:80`）只做一件事：301 跳到 HTTPS（`Caddyfile:76-78`）。

### `/internal/*` 为什么在外部返回 404

内部接口是「给同机房的其他服务调用」的，用共享密钥 `X-Internal-Secret` 认证
（`DRAMA_COMMERCE_INTERNAL_SECRET` 与 `DRAMA_CLOUD_INTERNAL_SECRET` 必须是同一个值）。

两件事要分开看：

1. **应用层**：密钥留空时这些路由**一律 404**，不是「放行」
   （`cloud/src/drama_cloud/commerce/internal_routes.py:11`、`57-60`）。留空若等于不鉴权，
   一次配置遗漏就会让任何人都能拿令牌换用户身份。
2. **网关层**：Caddy 还显式 `respond 404`。因为内部接口的安全模型是「只允许同一容器网络内的调用方」，
   而 Caddy 的 `handle` 是按前缀转发的——不显式拒绝的话，一个带对了密钥的外部请求就能穿过代理。

所以外部看到 404 是**两层都生效**的结果，不是某一层的疏漏。

## 6. 备份

```bash
cd cloud
./scripts/backup.sh                       # 备份到 ./backups/<UTC 时间戳>/
./scripts/backup.sh /path/to/backups      # 指定备份根目录
```

### 备了什么

| 内容 | 形式 |
| --- | --- |
| `drama_commerce` 库 | `pg_dump -Fc`（custom 格式，压缩过，恢复时可以只恢复部分表） |
| `drama_media` 库 | 同上 |
| 媒体数据目录 `/data` | `tar.gz`：素材、产物、分片上传的临时文件 |

外加两个文件：`SHA256SUMS`（每个备份文件的校验和）与 `MANIFEST.txt`（备份时间、库名、格式、
明确写出「未包含」什么）。

脚本里有两个刻意的检查：

- 每个库导出后**立刻确认不是空文件**（`backup.sh:44-50`）——空的 dump 在恢复时才会暴露，那时已经晚了；
- 做完就 `sha256sum ./*`，恢复脚本会拿它校验（`restore.sh:27-31`）。

### **没有**备什么

- **签名私钥**（`license.pem` 与轮换期的次私钥）；
- **`.env`**（里面有库密码、`CODE_SECRET`、`INTERNAL_SECRET`、SMTP 密码、模拟支付密钥）；
- **自签证书与它的私钥**（`cert.pem` / `key.pem`）；
- 源码与镜像（在仓库与镜像仓库里）。

理由写在脚本头部：**一份泄露就全泄露**。把数据库和密钥放在同一个备份包里，
等于给拿到备份的人一把万能钥匙（能签授权、能伪造回调、能进库）。

这三样要**单独保管**：

| 东西 | 怎么管 |
| --- | --- |
| 签名私钥 | 离线介质 / 密钥管理服务，至少两份异地。丢了要发版换公钥（见 [`KEY_ROTATION.md`](../../docs/KEY_ROTATION.md)） |
| `.env` | 纸质或密码管理器的安全条目。恢复时需要它，否则库密码对不上 |
| 自签证书 | 重新生成也行（那样所有客户端都要重新信任一次）；要沿用就单独存 |

### 建议

- 每天跑一次，保留最近 7–30 份；
- 备份完把 `SHA256SUMS` 单独抄一份（防「备份包被改过、校验和也被改过」）；
- **定期演练恢复**，见下一节。

## 7. 恢复

**这是破坏性操作。**会把两个库的现有数据替换成备份里的内容，媒体数据目录也会被覆盖
（`restore.sh` 的头部注释）。

```bash
./scripts/restore.sh backups/20261018T101500Z          # 交互确认
./scripts/restore.sh backups/20261018T101500Z --yes    # 脚本里用
```

脚本做的事，按顺序：

1. **校验完整性**：有 `SHA256SUMS` 就 `sha256sum -c`，不符直接停。没有则警告后继续
   （`restore.sh:27-31`）。为什么先校验：「恢复成功但数据是坏的」比恢复失败更危险。
2. **要求显式确认**：不带 `--yes` 时会打印「即将覆盖以下内容，**现有数据会丢失**」，要求输入 `yes`
   （`restore.sh:41-54`）。
3. **停应用层**：`docker compose stop commerce media`，**保留 `postgres`**——
   恢复期间不该有进程在写库或写数据目录。
4. **恢复两个库**：`pg_restore --clean --if-exists --no-owner --no-acl`。
   `--clean --if-exists` 是先删同名对象再建，避免「表已存在」导致恢复半途而废。
5. **恢复媒体数据**：`docker compose cp` 把压缩包送进容器，再 `rm -rf /data/*` 后解包。
   先清空再解包是刻意的：不清理会留下「备份里没有、但硬盘上还有」的孤儿文件，
   恢复出来的状态和备份时不一致。
6. **起服务**：`docker compose up -d --wait`。

### 恢复后要核对的四件事

脚本最后会把这四条打印出来（`restore.sh:83-87`），逐条走一遍：

1. **commerce 健康检查**：`curl -k https://<host>/api/v1/health` → `{"ok":true}`；
2. **管理后台能登录，用户数与备份前一致**（后台 → 用户，页面上有「共 N 个用户」）；
3. **抽一个用户，核对积分余额与流水求和一致**：后台看「余额」，再去「积分流水」按邮箱筛、把 `delta` 累加，
   两个数应当相等（见 [`ADMIN_GUIDE.md`](ADMIN_GUIDE.md) 第 5 节）；
4. **媒体控制面能列出一个已有素材**：打开媒体侧的 Web 客户端，看素材列表非空（数据卷真的恢复上了）。

另外两条容易漏、但值得一并确认：

- **签名私钥**：`docker compose logs commerce | grep kid`，确认启动日志里的 `kid` 与备份前一致。
  私钥不在备份里，所以「恢复后授权签不出来」几乎一定是私钥目录没挂对（`KEYS_DIR`）。
- **`.env` 里的 `INTERNAL_SECRET`**：如果恢复到了另一台机器而 `.env` 是新写的，
  两个服务的密钥必须一致，否则媒体侧调 commerce 会看到「账号服务的计费接口不可用」并返回 503。

### 演练建议

先在**另一个 compose 项目**上跑（`docker compose -p drama-restore-drill -f compose.deploy.yaml ...`，
或者干脆另一台机器），确认流程可用再对正式实例操作。恢复脚本不会替你判断「这是不是演练」。

## 8. 更新流程

版本策略在管理后台里改（**开关与版本 → 版本策略**），客户端启动时从
`GET /api/v1/client/config` 读到它（公开接口，不需要登录）。

### 步骤

1. **打包并签名**：`cargo tauri build` 产出安装包，然后**用现成命令签名**：

   ```bash
   drama-commerce release sign ./DramaDossier_1.2.0_x64-setup.exe
   ```

   输出 JSON 里有 `size_bytes` / `sha256` / `signature` / `key_id`，直接对应下面的版本策略字段。
   它用**客户端已信任的**签名私钥签出 detached 签名（base64url，签名对象是文件原始字节
   加用途前缀 `drama-dossier/release/v1\n`）。**不要手工算**——最要紧的一步留给手工很容易出错。
2. **把包放到能 https 访问的位置**。下载地址**必须**是 `https://`，客户端会拒绝明文 http
   （`src-tauri/src/updater.rs:83-92`）。
3. **填版本策略**：`latest` 填新版本号；`min_supported` 决定是**拦截**还是只提示；
   `download_url` / `download_sha256` / `download_sig` **三样都要填**。

   > **签名留空，客户端会拒绝安装。** `check_metadata` 要求地址、哈希、签名三样齐全，
   > 缺任何一项都**不做「降级成只验哈希」**——那等于把安全降到只信服务端。
   > 只填地址和哈希时，用户点「下载并校验升级包」后会看到「暂时无法更新：升级包没有签名，拒绝使用」
   > （`updater.rs:96-100`）。

4. **客户端提示与拦截**：启动时会拉配置（`src/main.js` 的 `renderUpdateBanner`）。
   - 低于 `min_supported`：**除首页与设置外的功能全部被遮罩挡住**，横幅红色
     （`src/logic.js` 的 `blockedByVersion`）。这才是「强制更新」——只弹横幅不够，
     用户点掉提示就继续干活了。
   - 只是低于 `latest`：「有新版本可用（当前 x，最新 y）。」黄色，**不拦人**；
   - `min_supported` 还是默认的 `0.0.0` 时任何版本都不低于它，**不提示也不拦**——
     这是刻意的默认值，避免刚上线就把所有人挡住。
   - 横幅里的按钮是「下载并校验升级包」；`download_url` 为空时改为一句「管理员还没有提供下载地址，请联系客服」。

   > **抬高 `min_supported` 前请确认新版已经发布**：它会真的把旧版用户挡在功能之外。
   > 首页与设置始终放行（要在那里下载安装），兑换卡密不受影响。

5. **用户下载并安装**：`updater.rs` 的校验分两层，缺一不可：
   - **大小**（若给了预期大小）与 **SHA-256**：防「下载过程中损坏或被人换了包」；
   - **Ed25519 签名**：防「有人改了服务端的下载地址」。哈希本身来自服务端，只查哈希等于只信服务端一家。

   任何一步不过就删掉文件并如实报错：`HashMismatch`（列出应为/实际的前 12 位）、
   `BadSignature`（「这可能不是官方发布的包，请不要安装」）、`SizeMismatch`、`Download`、`NotConfigured`。

6. **安装交给系统**：程序**不自己替换正在运行的二进制**（Windows 会锁住运行中的 exe、
   macOS 换掉二进制会让签名失效）。最后一步是把安装包交给系统（启动安装器 / 打开 dmg），
   **重启由安装器完成**。所以「安装」这一步需要用户自己点安装器，不是全自动静默升级。

### 什么时候用它、什么时候别用

- `min_supported` 决定「红色横幅 vs 黄色横幅」：低于它的客户端会一直显示「这个版本已停止支持」，
  否则只在落后于 `latest` 时给一条黄色提示。
- **低于 `min_supported` 的客户端还会被功能遮罩拦住**：除首页与设置外的模块全部换成
  「请下载新版」的遮罩页（`src-tauri` 的闸门 + `src/logic.js` 的 `blockedByVersion`），
  只在设置页里下载、安装新版。
- **遮罩只对带这段代码的版本有效**（M7 之后构建的安装包）。更早的客户端没有拦截逻辑，
  抬高 `min_supported` 拦不住它们，只能靠红横幅提示——如实告知，不要以为改了策略就万事大吉。
- 只是想提示「有新版本」就只改 `latest`。
- 回退版本策略时把 `latest` / `min_supported` 改回原值即可，用户下次启动拉到新策略后横幅就恢复正常
  （客户端每次启动都拉一次配置）。

## 9. 已知限制

这套部署是「够用的最小部署」，不是生产级方案。明确不做的事：

- **自签证书，浏览器与命令行工具都会警告**。用 `curl` 要 `-k`；用浏览器要手动「继续访问」。
  桌面端拼回调地址用的是 `PUBLIC_BASE_URL`，如果它指向自签域名，客户端侧也可能需要信任该证书。
  **这套部署不适合直接暴露到公网**。
- **单机部署**：所有服务在一台机器上。没有多副本、没有负载均衡、没有高可用。
  机器挂了就是全挂；`restart: unless-stopped` 只能处理进程级故障。
- **无日志聚合**：日志在各自容器的 stdout，靠 `docker compose logs <服务>` 看，没有集中检索、没有轮转配置、
  没有告警。排查历史问题只能靠当时的 `docker compose logs` 输出或数据库里的 `audit_log` 表。
- **数据库没有备份自动化**：`scripts/backup.sh` 要自己挂 cron 或 systemd timer，仓库里没有定时任务。
- **只有 postgres 写了健康检查**：编排里 `commerce` 与 `media` 都没有 `healthcheck`
  （`compose.deploy.yaml:36-40` 只有 postgres 那一段），所以 `up -d --wait` 只保证容器起来了，
  不保证服务真的能干活。验收要自己 `curl /api/v1/health`。两个服务其实都有这个端点
  （commerce 在 `commerce/api.py:147-149`，媒体控制面在 `api.py:304`），但没有接进 compose。
- **没有资源限制**：compose 里没写 `deploy.resources` / `mem_limit`。视频转码会吃满 CPU，
  和数据库抢资源是可能的。
- **管理后台没有 MFA**：只有邮箱 + 密码，会话 8 小时，令牌存浏览器 `sessionStorage`。
  见 [`ADMIN_GUIDE.md`](ADMIN_GUIDE.md) 第 6 节。
- **`/docs` 与 `/openapi.json` 仍然可访问**：FastAPI 自带的接口文档，把管理接口也列了出来。
  Caddy 侧目前没有屏蔽它们，需要的话在 `Caddyfile` 里加一条 `respond 404`。改完要 `docker compose restart caddy`。
- **邮件发不出去就只能看日志**：没有「重发验证码」的后台入口，SMTP 配错时用户拿不到验证码。
  排查看 `docker compose logs commerce`。

## 10. 常用运维命令

```bash
# 状态与日志
docker compose -f compose.deploy.yaml ps
docker compose -f compose.deploy.yaml logs -f commerce
docker compose -f compose.deploy.yaml logs --tail 200 media

# 进容器（commerce 里有 drama-commerce 命令）
docker compose -f compose.deploy.yaml exec commerce sh

# 清理（媒体控制面侧：过期产物 / 素材 / 分片上传 / 过期租约）
docker compose -f compose.deploy.yaml exec media drama-cloud cleanup

# 清理（commerce 侧：过期验证码、令牌、会话、限流计数；建议每天跑）
docker compose -f compose.deploy.yaml exec commerce drama-commerce cleanup

# 补积分（走账本、有审计、有幂等键）
docker compose -f compose.deploy.yaml exec commerce \
  drama-commerce credits grant --email a@example.com --amount 500 --memo 补偿 --by 张三

# 回收悬挂超过 2 小时的预扣（也可以交给 cleanup 定时跑）
docker compose -f compose.deploy.yaml exec commerce drama-commerce credits expire

# 备份
./scripts/backup.sh /path/to/backups
```

`cleanup` 不清理的东西：审计日志、卡密、电脑绑定、报审包记录、处理配方记录、积分账本、
支付订单与邀请记录（解绑次数要从绑定历史里数）。
