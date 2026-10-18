"""错误分类。

对应设计文档 7.2「重试规则」：错误是否可重试由**分类**决定，而不是由调用点各自判断，
这样调度器只需要看一个枚举。
"""

from __future__ import annotations

import enum


class ErrorClass(str, enum.Enum):
    """任务失败分类。"""

    # 可重试：临时网络、Worker 意外退出、短暂存储不可用、可恢复的资源争用
    TRANSIENT_IO = "transient_io"
    WORKER_LOST = "worker_lost"
    STORAGE_UNAVAILABLE = "storage_unavailable"
    RESOURCE_CONTENTION = "resource_contention"

    # 通常不可重试
    INPUT_CORRUPT = "input_corrupt"
    RECIPE_SCHEMA_INVALID = "recipe_schema_invalid"
    PARAM_OUT_OF_RANGE = "param_out_of_range"
    TARGET_UNSUPPORTED = "target_unsupported"
    CAPABILITY_MISSING = "capability_missing"
    # 质量门禁未通过：结果确定，重跑不会有不同结论，所以不可重试。
    QUALITY_GATE_FAILED = "quality_gate_failed"
    # 擦除确认流未通过（残留/闪烁/接缝/一致性超阈值）。与门禁不同，这里的失败**可以**
    # 靠调整参数改善（加大 mask 膨胀、换更强修复后端），因此允许有限次数的自动重试；
    # 次数用尽后按「转人工复核」处理，而不是直接判死。
    QUALITY_RETRYABLE = "quality_retryable"
    CANCELLED = "cancelled"
    #: 资源不存在，或存在但不属于当前用户（M5b 的按用户隔离）。
    #: 两种情况刻意用同一个值：分开会让人能拿 id 探测别人有没有这个资源。
    NOT_FOUND = "not_found"
    INTERNAL = "internal"

    def __str__(self) -> str:  # 避免 Python 3.9 打印成 "ErrorClass.X"
        return self.value

    @property
    def retryable(self) -> bool:
        return self in _RETRYABLE


_RETRYABLE = frozenset(
    {
        ErrorClass.TRANSIENT_IO,
        ErrorClass.WORKER_LOST,
        ErrorClass.STORAGE_UNAVAILABLE,
        ErrorClass.RESOURCE_CONTENTION,
        ErrorClass.QUALITY_RETRYABLE,
    }
)

#: 质量类重试的固定等待（秒）。它重跑是为了换参数，不是为了等外部故障恢复，
#: 所以不走指数退避——等久了只会让「自动重试」变慢。
QUALITY_RETRY_DELAY_SECONDS = 1.0

#: 指数退避基数（秒）与上限，供调度器计算下次可执行时间。
BACKOFF_BASE_SECONDS = 5.0
BACKOFF_MAX_SECONDS = 300.0
MAX_ATTEMPTS = 3


def backoff_delay(attempt: int) -> float:
    """第 `attempt` 次失败后的等待秒数（指数退避，封顶）。"""
    if attempt < 1:
        return 0.0
    return min(BACKOFF_BASE_SECONDS * (2 ** (attempt - 1)), BACKOFF_MAX_SECONDS)


class WorkError(Exception):
    """带错误分类的业务异常。"""

    def __init__(self, error_class: ErrorClass, message: str):
        super().__init__(message)
        self.error_class = error_class
        self.message = message

    @property
    def retryable(self) -> bool:
        return self.error_class.retryable


def invalid_recipe(message: str) -> WorkError:
    return WorkError(ErrorClass.RECIPE_SCHEMA_INVALID, message)


def bad_param(message: str) -> WorkError:
    return WorkError(ErrorClass.PARAM_OUT_OF_RANGE, message)


def unsupported(message: str) -> WorkError:
    return WorkError(ErrorClass.TARGET_UNSUPPORTED, message)


def corrupt_input(message: str) -> WorkError:
    return WorkError(ErrorClass.INPUT_CORRUPT, message)


def quality_retry(message: str) -> WorkError:
    """擦除确认流未通过：交给调度器按「调参数重跑 → 转人工复核」处理。"""
    return WorkError(ErrorClass.QUALITY_RETRYABLE, message)
