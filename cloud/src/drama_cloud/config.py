"""运行配置与目录布局。

所有路径都从 `DRAMA_CLOUD_HOME` 派生，默认落在仓库的 `cloud/var/` 下，便于本地学习时
一眼看到数据库、素材与产物分别在什么地方。
"""

from __future__ import annotations

import os
import shlex
from dataclasses import dataclass
from pathlib import Path
from typing import Optional, Tuple


def _default_home() -> Path:
    # config.py 位于 cloud/src/drama_cloud/，向上三级即 cloud/
    return Path(__file__).resolve().parents[2] / "var"


@dataclass(frozen=True)
class Settings:
    home: Path
    ffmpeg: str = "ffmpeg"
    ffprobe: str = "ffprobe"
    #: Worker 心跳间隔与租约时长（秒）。租约过期由调度器重新分配任务。
    heartbeat_seconds: float = 5.0
    lease_seconds: float = 30.0
    #: 单次任务允许的执行时长上限（秒），超时按 Worker 失联处理。
    task_timeout_seconds: float = 1800.0
    #: 并发配额；0 表示按 CPU 核数自动推算。
    max_concurrency: int = 0
    #: 产物保留天数；0 表示不自动清理。
    artifact_retention_days: int = 0
    #: Worker 声明的能力集，用于配方兼容性检查。
    capabilities: tuple = ()
    #: 擦除工具（Rust 侧 `dossier-inpaint`）路径；留空时按 PATH → 仓库 target/ 自动查找。
    #: 属于**部署配置**而不是配方参数：配方不能指定可执行文件（§10 不执行配方内任意命令）。
    inpaint_bin: Optional[str] = None
    #: 模型 worker 的解释器/可执行文件（如 python3）与固定参数（worker 脚本、检测后端等）。
    #: 同样只由部署方声明；配方只能选择「用哪个后端」，不能指定命令。
    inpaint_worker_program: Optional[str] = None
    inpaint_worker_args: Tuple[str, ...] = ()
    #: 后期工具链（Rust 侧 `dossier-media`）路径；留空时按 PATH → 仓库 target/ 自动查找。
    media_bin: Optional[str] = None
    #: 产物戳记校验工具（Rust 侧 `dossier-artifacts`）路径；解析规则同上。
    artifacts_bin: Optional[str] = None
    #: 运行时组件工具（Rust 侧 `dossier-runtime`）路径；解析规则同上。
    runtime_bin: Optional[str] = None
    #: 运行时组件清单（Tier 1 的 `RuntimeManifest`）。给了它，能力路由就会去核对
    #: 「模型/权重到底装齐了没有」，而不是只看 `inpaint_worker_program` 有没有配。
    #: RuntimeRoles 的本地安装根目录与可选 manifest。组件只从 manifest 声明的 HTTPS 地址下载。
    runtime_root: Optional[Path] = None
    #: 运行时清单路径同时供工具链能力探测与 RuntimeManager 使用。
    runtime_manifest: Optional[str] = None

    @property
    def db_path(self) -> Path:
        return self.home / "drama_cloud.sqlite3"

    @property
    def storage_dir(self) -> Path:
        """输入素材、中间产物与最终产物的根目录。"""
        return self.home / "storage"

    @property
    def work_dir(self) -> Path:
        """任务执行时的临时工作目录。"""
        return self.home / "work"

    @property
    def exports_dir(self) -> Path:
        return self.home / "exports"

    def ensure_dirs(self) -> None:
        for path in (self.home, self.storage_dir, self.work_dir, self.exports_dir):
            path.mkdir(parents=True, exist_ok=True)

    @property
    def effective_concurrency(self) -> int:
        if self.max_concurrency > 0:
            return self.max_concurrency
        return max(1, (os.cpu_count() or 2) // 4)


def load_settings(**overrides) -> Settings:
    home = Path(os.environ.get("DRAMA_CLOUD_HOME") or _default_home()).expanduser()
    raw_worker_args = os.environ.get("DRAMA_CLOUD_INPAINT_WORKER_ARGS", "")
    raw_capabilities = os.environ.get("DRAMA_CLOUD_CAPABILITIES", "")
    base = {
        "home": home,
        "ffmpeg": os.environ.get("DRAMA_CLOUD_FFMPEG", "ffmpeg"),
        "ffprobe": os.environ.get("DRAMA_CLOUD_FFPROBE", "ffprobe"),
        "inpaint_bin": os.environ.get("DRAMA_CLOUD_INPAINT_BIN") or None,
        "inpaint_worker_program": os.environ.get("DRAMA_CLOUD_INPAINT_WORKER") or None,
        "media_bin": os.environ.get("DRAMA_CLOUD_MEDIA_BIN") or None,
        "artifacts_bin": os.environ.get("DRAMA_CLOUD_ARTIFACTS_BIN") or None,
        "runtime_bin": os.environ.get("DRAMA_CLOUD_RUNTIME_BIN") or None,
        "runtime_manifest": os.environ.get("DRAMA_CLOUD_RUNTIME_MANIFEST") or None,
        # shlex 而不是 split：worker 脚本路径可能带空格，配引号即可。
        "inpaint_worker_args": tuple(shlex.split(raw_worker_args)) if raw_worker_args else (),
        "runtime_root": Path(os.environ.get("DRAMA_CLOUD_RUNTIME_ROOT") or (home / "runtime")),
        # 部署方额外声明的能力（如只有 GPU 机器才有的 `video_inpaint_model`）。
        "capabilities": tuple(
            item for item in shlex.split(raw_capabilities.replace(",", " ")) if item
        ),
    }
    base.update({k: v for k, v in overrides.items() if v is not None})
    settings = Settings(**base)
    settings.ensure_dirs()
    return settings
