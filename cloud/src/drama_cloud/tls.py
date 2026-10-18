"""启动时的传输安全校验（M5b-6）。

总方案要求「只走 HTTPS」。本机开发用自签证书，所以给两个开关：
`--tls-cert` / `--tls-key`。**这里最容易埋的坑不是「忘了开 TLS」，
而是「以为开了其实没开」**：

* 只给证书、没给私钥（或反过来）；
* 给了路径但文件不存在、或读不了；
* 部署方明确要求 TLS（`DRAMA_CLOUD_REQUIRE_TLS=1`），却一个参数都没给。

三种情况都必须**启动失败**，而不是悄悄退回明文。这个模块只做判断，
不碰 uvicorn，所以能直接单测。
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Optional


class TlsConfigError(RuntimeError):
    """TLS 配置不成立。消息里只出现路径与原因，不出现文件内容。"""


@dataclass(frozen=True)
class TlsConfig:
    """起 https 需要的两个文件路径。"""

    cert: Path
    key: Path

    def uvicorn_kwargs(self) -> dict:
        return {"ssl_certfile": str(self.cert), "ssl_keyfile": str(self.key)}


def resolve_tls(
    cert: Optional[str],
    key: Optional[str],
    *,
    required: bool = False,
) -> Optional[TlsConfig]:
    """算出 TLS 配置。

    * 两个都给了 → 校验文件可读，返回配置；
    * 两个都没给 → `required` 为真则报错，否则返回 None（明文，本机开发）；
    * **只给了一个** → 一律报错。这种「半开」状态最危险：
      看起来配了 TLS，实际上服务仍然在明文跑。
    """
    has_cert = bool(cert and cert.strip())
    has_key = bool(key and key.strip())

    if has_cert != has_key:
        missing = "--tls-key" if has_cert else "--tls-cert"
        raise TlsConfigError(
            f"TLS 参数不完整：给了 {'--tls-cert' if has_cert else '--tls-key'} "
            f"但缺少 {missing}。要么两个都给，要么都不给（本机开发用明文）。"
        )

    if not has_cert:
        if required:
            raise TlsConfigError(
                "已要求必须使用 TLS（DRAMA_CLOUD_REQUIRE_TLS=1），但没有提供 "
                "--tls-cert / --tls-key。拒绝以明文启动。"
            )
        return None

    cert_path = Path(cert).expanduser()
    key_path = Path(key).expanduser()
    for label, path in (("证书", cert_path), ("私钥", key_path)):
        if not path.exists():
            raise TlsConfigError(f"TLS {label}文件不存在：{path}")
        if not path.is_file():
            raise TlsConfigError(f"TLS {label}路径不是文件：{path}")
        try:
            # 只检查可读，不读内容：内容不该进日志，也不需要在这里解析。
            with path.open("rb"):
                pass
        except OSError as error:
            raise TlsConfigError(f"TLS {label}文件无法读取：{path}（{error.strerror}）") from error
    return TlsConfig(cert=cert_path, key=key_path)


def require_tls_from_env(value: Optional[str]) -> bool:
    """`DRAMA_CLOUD_REQUIRE_TLS` 的解析。

    只认明确的真值（1/true/yes/on，不区分大小写）。写错成 "ture" 之类时
    **不**当作要求 TLS：那种情况下用户以为开了、实际没有，更危险的是反过来
    （以为没开、其实开了会让人以为配置无效）。这里选择「不是明确真值就当没要求」，
    但把非法值当错误报出来更好——所以未知值一律报错。
    """
    if value is None or not str(value).strip():
        return False
    normalized = str(value).strip().lower()
    if normalized in {"1", "true", "yes", "on"}:
        return True
    if normalized in {"0", "false", "no", "off"}:
        return False
    raise TlsConfigError(
        f"DRAMA_CLOUD_REQUIRE_TLS 的取值无法识别：{value!r}。"
        "用 1/0、true/false、yes/no、on/off 之一。"
    )
