#!/usr/bin/env bash
# 防火墙与 SSH 加固（Ubuntu/Debian，需 root）。执行前请确认已能用 SSH 密钥登录，
# 否则关闭密码登录后会把自己锁在外面。
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "请用 root 执行" >&2
  exit 1
fi

# 至少要有一个「非空」的 authorized_keys：空文件照样会让密钥登录失败
has_key=0
for f in /root/.ssh/authorized_keys /home/*/.ssh/authorized_keys; do
  if [ -f "$f" ] && grep -qE '^(ssh-|ecdsa-|sk-)' "$f"; then
    has_key=1
    break
  fi
done
if [ "$has_key" -ne 1 ]; then
  echo "没有找到任何包含公钥的 authorized_keys，拒绝关闭密码登录（会把自己锁在外面）" >&2
  exit 1
fi

# SSH 实际监听的端口以 sshd 的生效配置为准，不能假设是 22
ssh_ports=$(sshd -T 2>/dev/null | awk '$1 == "port" { print $2 }' | sort -u)
if [ -z "$ssh_ports" ]; then
  echo "无法从 sshd -T 读取 SSH 端口，停止（请先确认 sshd 配置能正常解析）" >&2
  exit 1
fi

apt-get update -q
apt-get install -y -q ufw fail2ban unattended-upgrades

echo "即将放行的端口：SSH $(echo "$ssh_ports" | tr '\n' ' ')、80、443；其余入站全部拒绝"
ufw default deny incoming
ufw default allow outgoing
for p in $ssh_ports; do
  ufw allow "$p/tcp"
done
ufw allow 80/tcp
ufw allow 443/tcp
ufw --force enable

install -d -m 0755 /etc/ssh/sshd_config.d
cat > /etc/ssh/sshd_config.d/90-dd-license.conf <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
MaxAuthTries 3
EOF
sshd -t && systemctl reload ssh || systemctl reload sshd

systemctl enable --now fail2ban
dpkg-reconfigure -f noninteractive unattended-upgrades

# 服务账号与目录
id ddlicense >/dev/null 2>&1 || useradd --system --home /var/lib/dd-license --shell /usr/sbin/nologin ddlicense
install -d -o ddlicense -g ddlicense -m 0700 /var/lib/dd-license
install -d -o root -g ddlicense -m 0750 /etc/dd-license

echo "完成：ufw 只开 SSH($(echo "$ssh_ports" | tr '\n' ' '))/80/443，SSH 仅密钥登录，fail2ban 与自动安全更新已开启。"
