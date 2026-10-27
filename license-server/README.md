# 剧备卡密授权服务（license-server）

负责三件事：
- 兑换卡密：一张卡只能用一次，绑定一台电脑；
- 给电脑签发授权：使用 Ed25519 离线凭证，客户端不联网也能验证；
- 提供网页管理后台：生成、查询、作废卡密，管理已授权的电脑，查看操作日志。

授权协议、客户端如何验权、上线前要检查的项目，见 [docs/LICENSING.md](../docs/LICENSING.md)。

## 公网暴露的内容

| 路径 | 访问限制 | 用途 |
|---|---|---|
| `POST /v1/activate`、`POST /v1/refresh`、`GET /v1/health` | 任何人可访问，但只能做兑换和续签 | 客户端使用 |
| `/admin`、`/admin/api/*` | 仅 **IP 白名单**内的地址可见，且需要**管理员密码**登录 | 管理员使用 |
| 其他路径 | 一律返回 404 | — |

FastAPI 自带的 `/docs`、`/redoc`、`/openapi.json` 已全部关闭。

## 部署（单台 Ubuntu/Debian VPS）

```bash
# 0. 以 root 执行：配置防火墙（只放行 SSH 的实际端口与 80/443）、SSH 仅允许密钥登录、fail2ban、自动安全更新，并创建服务账号
#    执行前务必确认已经能用 SSH 密钥登录。脚本会检查：至少一个 authorized_keys 里真的有公钥，
#    并用 sshd -T 读出 SSH 实际端口；任一检查不通过就停止，不会改动防火墙
bash deploy/setup-firewall.sh

# 1. 安装（依赖按哈希锁定）
install -d /opt/dd-license && cp -r . /opt/dd-license/ && cd /opt/dd-license
python3 -m venv .venv
.venv/bin/pip install --require-hashes --no-deps -r deploy/requirements.lock
.venv/bin/pip install --no-deps .

# 2. 配置
cp deploy/env.example /etc/dd-license/env      # 修改其中的 DD_LICENSE_ADMIN_ALLOW_IPS
chown root:ddlicense /etc/dd-license/env && chmod 640 /etc/dd-license/env
set -a; . /etc/dd-license/env; set +a

# 3. 建库、生成签名密钥、创建管理员
sudo -u ddlicense -E .venv/bin/license-admin init-db
.venv/bin/license-admin keygen --out /etc/dd-license/key.pem   # 会打印公钥，交给开发者写入客户端
chown root:ddlicense /etc/dd-license/key.pem && chmod 640 /etc/dd-license/key.pem
sudo -u ddlicense -E .venv/bin/license-admin admin create --username 你的用户名

# 4. 启动
cp deploy/dd-license.service /etc/systemd/system/ && systemctl daemon-reload
systemctl enable --now dd-license
cp deploy/Caddyfile /etc/caddy/Caddyfile        # 改成你的域名和白名单 IP
systemctl reload caddy

# 5. 安全自检，所有项都应显示「通过」
sudo -u ddlicense -E .venv/bin/license-admin doctor
```

`deploy/requirements.lock` 是在 Python 3.12、x86_64 环境下生成的。如果目标机的 Python 版本或 CPU 架构不同，需要在目标机上重新生成：先用 `pip download` 下载 `pyproject.toml` 中锁定的版本，再对下载到的每个 wheel 文件记录 `sha256sum`。

**只能运行 1 个 worker 进程**。原因有两个：限流状态保存在进程内，不能跨进程共享；SQLite 也只适合单个写进程。

## 日常运维

| 操作 | 做法 |
|---|---|
| 生成卡密 | 后台 →「生成卡密」。明文**只显示这一次**，生成后要立刻下载 CSV 保存 |
| 查一张卡 | 后台 →「卡密查询」 |
| 作废未售出的卡 | 后台 →「批次」→ 作废整批；或在「卡密查询」中作废单张 |
| 退款或盗刷 | 后台 →「已授权电脑」→ 作废。该电脑下次联网时立即锁定，一直离线最多还能使用 7 天 |
| 用户换电脑或重装系统 | **不支持迁移**。授权与电脑固定绑定；重装系统或换了硬件、导致机器码变化时，需要重新兑换一张卡 |
| 忘记密码或被锁定 | 在服务器上执行 `license-admin admin passwd`，或 `license-admin admin unlock` |
| 备份 | 每天执行一次 `sqlite3 license.sqlite3 ".backup backup.sqlite3"`，再用 `age` 加密后存到异地。私钥 `key.pem` 另外离线保存一份 |
| 清理审计日志 | 审计表会一直增长。每月执行一次 `sudo -u ddlicense -E .venv/bin/license-admin prune-audit --days 180`，删除 180 天之前的记录并回收空间（最少保留 30 天，填更小的天数会被拒绝）。放进 `/etc/cron.d/dd-license` 时要先加载配置：`0 4 1 * * ddlicense /bin/sh -c 'set -a; . /etc/dd-license/env; exec /opt/dd-license/.venv/bin/license-admin prune-audit --days 180'` |

## 私钥：最重要的一份文件

- 私钥丢失：已经发出的凭证无法再续签，所有用户在 7 天内会被锁定。
- 私钥泄露：别人可以伪造任意授权。

两种情况都要按 [docs/LICENSING.md](../docs/LICENSING.md)「密钥轮换应急」处理。

## 开发与测试

```bash
python3 -m venv .venv && .venv/bin/pip install -e '.[dev]'
.venv/bin/pytest -q
.venv/bin/python -m tests.vectors      # 修改协议后重新生成与 Rust 共用的测试向量，并一起提交
```

本机试运行：

```bash
export DD_LICENSE_HOME=/tmp/dd DD_LICENSE_ADMIN_ALLOW_IPS=127.0.0.1/32
.venv/bin/license-admin init-db && .venv/bin/license-admin keygen
.venv/bin/license-admin admin create --username dev
.venv/bin/license-admin serve --port 8200
# 浏览器打开 http://127.0.0.1:8200/admin
```
