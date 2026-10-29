"""对象存储适配层。

设计文档要求「使用短时授权链接上传/下载，避免经由 API 代理大文件」，并建议 S3 兼容对象存储。
本机没有对象存储，这里给出一个**本地文件系统实现**，但保留同一套语义：

- `key` 是不透明对象键，调用方不拼路径；
- 上传/下载都要先换一个**短时签名令牌**（`sign`/`verify`），过期即失效；
- 换实现时只需要替换这个类，控制面与 Worker 的调用点不变。

签名用 HMAC-SHA256，密钥首次运行生成在 `home/secret.key`（0600）。它不是访问控制，
只是把「凭证」与「对象键」解耦，方便以后整体换成真正的预签名 URL。
"""

from __future__ import annotations

import hashlib
import hmac
import os
import secrets
import shutil
import time
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Optional

from .config import Settings
from .errors import ErrorClass, WorkError

CHUNK = 1024 * 1024


@dataclass
class ObjectStat:
    key: str
    size_bytes: int
    sha256: str


class LocalStorage:
    def __init__(self, settings: Settings, ttl_seconds: int = 900):
        self.settings = settings
        self.root = settings.storage_dir
        self.root.mkdir(parents=True, exist_ok=True)
        self.ttl_seconds = ttl_seconds
        self._secret = self._load_secret()

    # ---------- 密钥 ----------

    def _load_secret(self) -> bytes:
        """读取签名 URL 用的 HMAC 密钥；不存在就生成。

        先在同目录以 O_EXCL|0600 写好临时文件并 fsync，再用 `os.link` 放到正式位置：
        - 正式文件要么不存在，要么是完整的 32 字节，别的进程不会读到写了一半的文件；
        - 目标已存在时 link 失败（FileExistsError），不会覆盖另一个进程先写好的密钥；
        - 文件从创建起就是 0600，没有「先按 umask 落盘、再 chmod」的空档。
        """
        path = self.settings.home / "secret.key"
        if not path.exists():
            tmp = path.with_name(f"{path.name}.tmp-{os.getpid()}")
            try:
                fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                with os.fdopen(fd, "wb") as handle:
                    handle.write(secrets.token_bytes(32))
                    handle.flush()
                    os.fsync(handle.fileno())
                try:
                    os.link(tmp, path)
                except FileExistsError:
                    pass  # 另一个进程先放好了，用它的
            finally:
                try:
                    os.unlink(tmp)
                except FileNotFoundError:
                    pass
        secret = path.read_bytes()
        if len(secret) < 32:
            # 正常流程不会出现（正式文件只经 link 原子出现）；出现就是被截断或手改过。
            # 宁可报错也不用弱密钥签 URL。
            raise RuntimeError(f"签名密钥 {path} 长度异常（{len(secret)} 字节），请删除后重启")
        return secret

    # ---------- 键与路径 ----------

    @staticmethod
    def asset_key(asset_id: str, filename: str) -> str:
        return f"assets/{asset_id}/{Path(filename).name}"

    @staticmethod
    def artifact_key(task_id: str, name: str) -> str:
        return f"artifacts/{task_id}/{Path(name).name}"

    def local_path(self, key: str) -> Path:
        """把对象键解析成磁盘路径，并挡住越界访问。"""
        candidate = (self.root / key).resolve()
        if not str(candidate).startswith(str(self.root.resolve()) + os.sep):
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"对象键越界：{key}")
        return candidate

    # ---------- 读写 ----------

    def put_stream(self, key: str, source: BinaryIO) -> ObjectStat:
        target = self.local_path(key)
        target.parent.mkdir(parents=True, exist_ok=True)
        digest = hashlib.sha256()
        size = 0
        try:
            with target.open("wb") as handle:
                while True:
                    block = source.read(CHUNK)
                    if not block:
                        break
                    digest.update(block)
                    size += len(block)
                    handle.write(block)
        except OSError as error:  # 临时 IO 问题按可重试分类
            raise WorkError(ErrorClass.STORAGE_UNAVAILABLE, f"写入对象失败：{error}") from error
        return ObjectStat(key, size, digest.hexdigest())

    def put_file(self, key: str, source: Path) -> ObjectStat:
        with source.open("rb") as handle:
            return self.put_stream(key, handle)

    def open(self, key: str) -> BinaryIO:
        path = self.local_path(key)
        if not path.exists():
            raise WorkError(ErrorClass.STORAGE_UNAVAILABLE, f"对象不存在：{key}")
        return path.open("rb")

    def read_bytes(self, key: str) -> bytes:
        with self.open(key) as handle:
            return handle.read()

    def exists(self, key: str) -> bool:
        return self.local_path(key).exists()

    def size(self, key: str) -> int:
        return self.local_path(key).stat().st_size

    def sha256(self, key: str) -> str:
        digest = hashlib.sha256()
        with self.open(key) as handle:
            while True:
                block = handle.read(CHUNK)
                if not block:
                    break
                digest.update(block)
        return digest.hexdigest()

    def delete(self, key: str) -> None:
        path = self.local_path(key)
        try:
            path.unlink()
        except FileNotFoundError:
            pass

    def delete_tree(self, prefix: str) -> None:
        path = self.local_path(prefix)
        if path.is_dir():
            shutil.rmtree(path, ignore_errors=True)

    # ---------- 短时授权 ----------

    def sign(self, key: str, ttl_seconds: Optional[int] = None) -> str:
        expires = int(time.time()) + (ttl_seconds or self.ttl_seconds)
        payload = f"{key}\n{expires}".encode()
        mac = hmac.new(self._secret, payload, hashlib.sha256).hexdigest()
        return f"{expires}.{mac}"

    # ---------- 自包含签名载荷（上传会话 / 下载授权） ----------

    def sign_payload(self, payload: dict, ttl_seconds: Optional[int] = None) -> str:
        """把一段 JSON 连同过期时间签成一个不透明令牌。

        这样「上传会话」与「下载授权」不需要服务端保存状态：令牌自证内容与有效期。
        换成 S3 预签名 URL 时，这个方法就是替换点。
        """
        import base64
        import json

        expires = int(time.time()) + (ttl_seconds or self.ttl_seconds)
        body = base64.urlsafe_b64encode(
            json.dumps(payload, ensure_ascii=False, sort_keys=True).encode("utf-8")
        ).decode("ascii")
        mac = hmac.new(
            self._secret, f"{body}\n{expires}".encode(), hashlib.sha256
        ).hexdigest()
        return f"{body}.{expires}.{mac}"

    def verify_payload(self, token: str, scope: Optional[str] = None) -> dict:
        """校验并解开签名载荷；过期、篡改、用途不符都会抛错。"""
        import base64
        import json

        try:
            body, expires_text, mac = str(token).split(".", 2)
            expires = int(expires_text)
        except (ValueError, AttributeError) as error:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "令牌格式非法") from error
        expected = hmac.new(
            self._secret, f"{body}\n{expires}".encode(), hashlib.sha256
        ).hexdigest()
        if not hmac.compare_digest(expected, mac):
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "令牌签名无效")
        if expires < int(time.time()):
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "令牌已过期")
        try:
            payload = json.loads(base64.urlsafe_b64decode(body.encode("ascii")).decode("utf-8"))
        except Exception as error:  # 载荷不是合法 base64/JSON
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "令牌载荷无法解析") from error
        if scope is not None and payload.get("scope") != scope:
            raise WorkError(
                ErrorClass.PARAM_OUT_OF_RANGE,
                f"令牌用途不符：期望 {scope}，实际 {payload.get('scope')}",
            )
        return payload

    def verify(self, key: str, token: str) -> None:
        try:
            expires_text, mac = token.split(".", 1)
            expires = int(expires_text)
        except (ValueError, AttributeError) as error:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "授权令牌格式非法") from error
        expected = hmac.new(
            self._secret, f"{key}\n{expires}".encode(), hashlib.sha256
        ).hexdigest()
        if not hmac.compare_digest(expected, mac):
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "授权令牌无效")
        if expires < int(time.time()):
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "授权令牌已过期")
