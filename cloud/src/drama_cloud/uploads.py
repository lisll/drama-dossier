"""分片上传与断点续传（M5b-2）。

**为什么要分片**：一部剧的素材动辄几个 GB。一次性 PUT 整文件时，
网络一抖就得从头再来，而且服务端要一直占着内存/带宽等整个请求结束。
按块上传后，断在哪就从哪继续。

**设计要点**：

* 分片落在 `home/uploads/<upload_id>/<index>.part`，完成时按序拼接落到对象存储
  的正确位置。临时目录与正式存储分开，清理时各管各的（见 M5b-5）。
* **整文件 sha256 在完成时才校验**：客户端在创建会话时声明它，服务端在拼接时
  一边拼一边算，最后一比。不符就整单作废、删掉分片——绝不把「对不上的素材」
  交给后面的处理流程。
* 分片可重复上传：同一 `index` 再传就覆盖。断点续传就是「补传缺的那几块」，
  不需要额外的「续传会话」概念。
* 完成前不写 `assets` 行：素材表里不该出现「传了一半」的可用素材。
  完成后才在一个事务里建 assets 行并把会话标成 completed。
* 只有 owner 能操作自己的上传会话：别人猜到 upload_id 也不能往里写分片
  （否则可以往你的素材里塞内容）。
"""

from __future__ import annotations

import hashlib
import shutil
from dataclasses import dataclass
from pathlib import Path

from .db import json_dump, json_load, now_iso, transaction
from .errors import ErrorClass, WorkError
from .storage import LocalStorage

#: 分片大小：8 MiB。比读取块（1 MiB）大，减少请求数；又不是大到让重传成本很高。
DEFAULT_CHUNK_SIZE = 8 * 1024 * 1024
#: 单个上传最多允许的分片数。8 MiB × 10000 ≈ 78 GB，够用且能挡住恶意构造。
MAX_CHUNKS = 10000


@dataclass
class UploadSession:
    id: str
    project_id: str
    owner_id: str
    name: str
    filename: str
    declared_size: int
    declared_sha256: str
    chunk_size: int
    received: list
    status: str
    asset_id: str | None


def _row_to_session(row) -> UploadSession:
    return UploadSession(
        id=row["id"],
        project_id=row["project_id"],
        owner_id=row["owner_id"],
        name=row["name"],
        filename=row["filename"],
        declared_size=int(row["declared_size"]),
        declared_sha256=row["declared_sha256"],
        chunk_size=int(row["chunk_size"]),
        received=json_load(row["received_json"], []) or [],
        status=row["status"],
        asset_id=row["asset_id"],
    )


def chunk_count(size: int, chunk_size: int) -> int:
    """总共要传几块。空文件算 0 块（但仍允许完成，产出空素材）。"""
    if size <= 0:
        return 0
    return (size + chunk_size - 1) // chunk_size


def parts_dir(storage: LocalStorage, upload_id: str) -> Path:
    """分片的临时目录。和对象存储根目录分开，避免被当成正式对象。"""
    root = storage.settings.uploads_dir / upload_id
    root.mkdir(parents=True, exist_ok=True)
    return root


def create(
    conn,
    storage: LocalStorage,
    project_id: str,
    owner_id: str,
    name: str,
    filename: str,
    size_bytes: int,
    sha256: str,
    chunk_size: int = DEFAULT_CHUNK_SIZE,
) -> UploadSession:
    """开一个上传会话。不落任何文件，只记一行状态。"""
    if size_bytes < 0:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "文件大小不能为负")
    if chunk_size <= 0:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "分片大小必须大于 0")
    total = chunk_count(size_bytes, chunk_size)
    if total > MAX_CHUNKS:
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"分片数超过上限（{total} > {MAX_CHUNKS}），请调大分片大小",
        )
    upload_id = "up_" + hashlib.sha256(f"{project_id}:{filename}:{now_iso()}".encode()).hexdigest()[:24]
    with transaction(conn):
        conn.execute(
            "INSERT INTO upload_sessions (id, project_id, owner_id, name, filename,"
            " declared_size, declared_sha256, chunk_size, received_json, status,"
            " created_at, updated_at)"
            " VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s,'open',%s,%s)",
            (
                upload_id, project_id, owner_id, name, filename,
                int(size_bytes), sha256, int(chunk_size), json_dump([]), now_iso(), now_iso(),
            ),
        )
    return get(conn, upload_id)


def get(conn, upload_id: str) -> UploadSession:
    row = conn.execute(
        "SELECT * FROM upload_sessions WHERE id = %s", (upload_id,)
    ).fetchone()
    if row is None:
        raise WorkError(ErrorClass.NOT_FOUND, "找不到这个上传会话")
    return _row_to_session(row)


def get_owned(conn, upload_id: str, owner_id: str) -> UploadSession:
    """取自己的会话。别人的会话与不存在的会话回同一个错误。

    不透露「这个 id 是否存在」——否则可以拿它探测别人的上传。
    """
    session = get(conn, upload_id)
    if session.owner_id != owner_id:
        raise WorkError(ErrorClass.NOT_FOUND, "找不到这个上传会话")
    return session


def write_chunk(
    conn,
    storage: LocalStorage,
    session: UploadSession,
    index: int,
    payload: bytes,
) -> UploadSession:
    """写入一块。同一 index 重复写会覆盖（断点续传就是补传缺的块）。"""
    if session.status != "open":
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"这个上传会话已经 {session.status}")
    total = chunk_count(session.declared_size, session.chunk_size)
    if index < 0 or index >= total:
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"分片下标越界：{index}（共 {total} 块，从 0 开始）",
        )
    # 除最后一块外，每块必须正好是 chunk_size；否则拼出来的文件会错位。
    expected = session.chunk_size
    if index == total - 1:
        expected = session.declared_size - session.chunk_size * (total - 1)
    if len(payload) != expected:
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"第 {index} 块大小应为 {expected} 字节，实际 {len(payload)} 字节",
        )

    target = parts_dir(storage, session.id) / f"{index:06d}.part"
    try:
        target.write_bytes(payload)
    except OSError as error:
        raise WorkError(ErrorClass.STORAGE_UNAVAILABLE, f"写入分片失败：{error}") from error

    received = sorted(set(session.received) | {index})
    with transaction(conn):
        conn.execute(
            "UPDATE upload_sessions SET received_json = %s, updated_at = %s WHERE id = %s",
            (json_dump(received), now_iso(), session.id),
        )
    session.received = received
    return session


def missing_chunks(session: UploadSession) -> list:
    """还缺哪些块。断点续传时告诉客户端只补这些。"""
    total = chunk_count(session.declared_size, session.chunk_size)
    have = set(session.received)
    return [index for index in range(total) if index not in have]


def complete(
    conn,
    storage: LocalStorage,
    session: UploadSession,
) -> str:
    """拼接所有分片、校验整文件哈希、落到对象存储、建 assets 行。

    返回 asset_id。任何一步不对都不留下「可用素材」：
    哈希不符会抛错并删掉分片，会话标成 aborted。
    """
    from .storage import LocalStorage as _Storage

    # 素材 id 先定下来，对象键按它算（与 `register_asset` 的布局一致：
    # `assets/<asset_id>/<filename>`），避免「先随便起个键、再改名」。
    asset_id = "as_" + hashlib.sha256(f"{session.id}".encode("utf-8")).hexdigest()[:24]
    object_key = _Storage.asset_key(asset_id, session.filename)
    if session.status != "open":
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"这个上传会话已经 {session.status}")
    missing = missing_chunks(session)
    if missing:
        raise WorkError(
            ErrorClass.PARAM_OUT_OF_RANGE,
            f"还有 {len(missing)} 块没传（缺：{missing[:5]}…），不能完成",
        )

    directory = parts_dir(storage, session.id)
    total = chunk_count(session.declared_size, session.chunk_size)
    digest = hashlib.sha256()
    assembled = directory / "assembled.bin"
    try:
        with assembled.open("wb") as out:
            for index in range(total):
                part = directory / f"{index:06d}.part"
                data = part.read_bytes()
                digest.update(data)
                out.write(data)
    except OSError as error:
        raise WorkError(ErrorClass.STORAGE_UNAVAILABLE, f"拼接分片失败：{error}") from error

    actual_sha = digest.hexdigest()
    actual_size = assembled.stat().st_size if assembled.exists() else 0
    if actual_sha != session.declared_sha256:
        shutil.rmtree(directory, ignore_errors=True)
        with transaction(conn):
            conn.execute(
                "UPDATE upload_sessions SET status = 'aborted', updated_at = %s WHERE id = %s",
                (now_iso(), session.id),
            )
        raise WorkError(
            ErrorClass.INPUT_CORRUPT,
            f"整文件校验和不符（声明 {session.declared_sha256[:12]}…，实际 {actual_sha[:12]}…），"
            "请重新上传",
        )
    if actual_size != session.declared_size:
        shutil.rmtree(directory, ignore_errors=True)
        with transaction(conn):
            conn.execute(
                "UPDATE upload_sessions SET status = 'aborted', updated_at = %s WHERE id = %s",
                (now_iso(), session.id),
            )
        raise WorkError(
            ErrorClass.INPUT_CORRUPT,
            f"整文件大小不符（声明 {session.declared_size}，实际 {actual_size}）",
        )

    stat = storage.put_file(object_key, assembled)
    with transaction(conn):
        conn.execute(
            "INSERT INTO assets (id, project_id, name, storage_key, sha256, size_bytes,"
            " lifecycle, created_at)"
            " VALUES (%s,%s,%s,%s,%s,%s,'ready',%s)",
            (asset_id, session.project_id, session.name, stat.key, stat.sha256,
             stat.size_bytes, now_iso()),
        )
        conn.execute(
            "UPDATE upload_sessions SET status = 'completed', asset_id = %s, updated_at = %s"
            " WHERE id = %s",
            (asset_id, now_iso(), session.id),
        )
    shutil.rmtree(directory, ignore_errors=True)
    return asset_id


def abort(conn, storage: LocalStorage, session: UploadSession) -> None:
    """放弃上传：删分片、标 aborted。已上传的字节不留在磁盘上。"""
    shutil.rmtree(parts_dir(storage, session.id), ignore_errors=True)
    with transaction(conn):
        conn.execute(
            "UPDATE upload_sessions SET status = 'aborted', updated_at = %s WHERE id = %s",
            (now_iso(), session.id),
        )
