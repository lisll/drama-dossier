"""卡种 → 时长的唯一配置表。

生成卡密时会把天数复制进 cards 表，之后改这里不影响已售出的卡。
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Plan:
    code: str
    label: str
    days: int | None  # None 表示永久


PLANS: dict[str, Plan] = {
    "trial": Plan("trial", "试用卡", 3),
    "month": Plan("month", "月卡", 30),
    "quarter": Plan("quarter", "季卡", 90),
    "year": Plan("year", "年卡", 365),
    "lifetime": Plan("lifetime", "永久卡", None),
}


def get_plan(code: str) -> Plan:
    try:
        return PLANS[code]
    except KeyError:
        raise ValueError(f"未知卡种：{code}") from None
