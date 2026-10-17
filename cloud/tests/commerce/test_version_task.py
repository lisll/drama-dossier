"""version-task@1 任务哈希：Python 参考实现对拍共享向量（Rust 侧 crates/dossier-license/tests/version_task.rs）。

向量文件 crates/dossier-license/tests/version_task_vectors.json 由本文件生成并提交进仓库；
改规则（schema、字段、规范化方式）时两边一起改，再用 UPDATE_VERSION_TASK_VECTORS=1 重新生成。
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).parent))

from version_task import TASK_SCHEMA, canonical_json, template_hash, version_task_hash  # noqa: E402

VECTORS_PATH = Path(__file__).resolve().parents[3] / "crates/dossier-license/tests/version_task_vectors.json"

EPISODES = [
    ("第01集.mp4", "1" * 64),
    ("第02集.mp4", "2" * 64),
]
RIGHTS = "3" * 64
TEMPLATE = {"片头": {"路径": "intro.mp4", "时长": 12}, "变速": 1.02}

# 30 集：文件名升序，用来验证多集时的顺序稳定（两边都按传入顺序写进 JSON）。
MANY_EPISODES = [(f"第{index:02d}集.mp4", f"{index:064x}") for index in range(1, 31)]

# 嵌套对象 / 数组 / null / 布尔 / 整数 / 浮点 / 空对象 / 空数组 / 非 ASCII 键和值都齐一份。
RICH_TEMPLATE = {
    "片头": {
        "路径": "intro.mp4",
        "时长": 12,
        "叠加": None,
        "淡入": True,
        "淡出": False,
        "音量": [1, 0.5, 0.0],
    },
    "片尾": {"路径": "outro.mp4", "时长": 3.5},
    "变速": 1.02,
    "空对象": {},
    "空数组": [],
    "非 ASCII 键": "值 🎬",
    "层级": [[1, 2], [3, [4, {"深": "层"}]], []],
}

# 浮点的规范化是两边最容易不一致的地方：整数值带 .0、最短往返表示。
# 注意：Rust 侧 python_float_repr 只接受 1e-4 ≤ |值| < 1e16，否则报 bad_number
# （Python 那边会用科学计数法，两边无法对齐），所以 1e-7、1e16 这类值不进向量，
# 改在 crates/dossier-license/tests/version_task.rs 里断言被拒绝。
FLOAT_TEMPLATE = {
    "零": 0,
    "零点五": 0.5,
    "一点零": 1.0,
    "负二点五": -2.5,
    "下界": 0.0001,
    "上界": 123456789.5,
    "小数": 1234.5678,
    "十分之一": 0.1,
    "整数": 100.0,
    "长尾": 0.30000000000000004,
}

# 同一份内容的两份键顺序不同的写法（嵌套层级也打乱），规范化后哈希必须相等。
SHUFFLED_TEMPLATE_A = {"片头": {"路径": "intro.mp4", "时长": 12}, "变速": 1.02, "字幕": [1, 2]}
SHUFFLED_TEMPLATE_B = {"字幕": [1, 2], "变速": 1.02, "片头": {"时长": 12, "路径": "intro.mp4"}}


def _case(name: str, episodes, rights: str, template, strength: str) -> dict:
    return {
        "name": name,
        "episodes": [list(item) for item in episodes],
        "rights": rights,
        "template": template,
        "strength": strength,
        "task": version_task_hash(episodes, rights, template, strength),
    }


def build_vectors() -> list:
    return [
        _case("basic", EPISODES, RIGHTS, TEMPLATE, "medium"),
        _case("empty_episodes", [], "a" * 64, {}, "light"),
        _case("unicode_keys", [("épisode one.mp4", "0" * 64)], "b" * 64,
              {"字幕": {"字体": "苹方", "颜色": "#FFFFFF", "引号\"与\\": 1}}, "heavy"),
        # 分集名里的空白、引号、换行：两边都要转义成一样的字节。
        # 反斜杠在 Rust 侧是非法分集名（bad_name），所以不进向量，Rust 测试里单独断言拒绝。
        _case("episode_name_spaces", [("第 01 集 修正 版 .mp4", "c" * 64)], "d" * 64,
              {"片头": "intro.mp4"}, "medium"),
        _case("episode_name_quote", [('第"01"集.mp4', "e" * 64)], "f" * 64,
              {"片头": "intro.mp4"}, "medium"),
        _case("episode_name_newline", [("第01集\n修正.mp4", "9" * 64)], "1" * 64,
              {"片头": "intro.mp4"}, "medium"),
        # 四字节 UTF-8（U+1D11E 音乐符号、U+1F3AC 场记板）。
        _case("episode_name_four_byte_utf8", [("第01集𝄞🎬.mp4", "2" * 64)], "4" * 64,
              {"片头": "intro.mp4"}, "medium"),
        # 全角标点。
        _case("episode_name_fullwidth_punct", [("第01集（修）.mp4", "5" * 64)], "6" * 64,
              {"片头": "intro.mp4"}, "medium"),
        # 0 集已由 empty_episodes 覆盖，这里补 30 集验证顺序稳定。
        _case("many_episodes", MANY_EPISODES, "7" * 64, {"片头": "intro.mp4"}, "heavy"),
        # 模板 JSON 的各种值类型凑齐。
        _case("template_rich_types", [("第01集.mp4", "8" * 64)], "a1" * 32, RICH_TEMPLATE, "medium"),
        # 浮点规范化：整数值带 .0、最短往返表示。
        _case("template_floats", [("第01集.mp4", "9a" * 32)], "b1" * 32, FLOAT_TEMPLATE, "light"),
        # 同一份内容的两种键顺序，task 哈希必须相等（Rust 测试里断言）。
        _case("template_key_order_a", [("第01集.mp4", "c1" * 32)], "d1" * 32, SHUFFLED_TEMPLATE_A, "medium"),
        _case("template_key_order_b", [("第01集.mp4", "c1" * 32)], "d1" * 32, SHUFFLED_TEMPLATE_B, "medium"),
    ]


def test_task_schema_value():
    assert TASK_SCHEMA == "version-task@1"


def test_vectors_file_matches_python():
    """共享向量必须和 Python 参考实现逐条一致。"""
    vectors = json.loads(VECTORS_PATH.read_text(encoding="utf-8"))
    assert [case["name"] for case in vectors] == [case["name"] for case in build_vectors()]
    for vector, rebuilt in zip(vectors, build_vectors()):
        assert vector == rebuilt, f"用例 {vector['name']}：和参考实现不一致，用 UPDATE_VERSION_TASK_VECTORS=1 重新生成"


def test_canonical_json_is_compact_and_sorted():
    assert canonical_json({"b": 1, "a": [2, 3]}) == b'{"a":[2,3],"b":1}'
    # 非ASCII字符按 UTF-8 原样输出，不转义成 \\uXXXX。
    assert canonical_json({"字体": "苹方"}) == '{"字体":"苹方"}'.encode("utf-8")
    # 控制字符转义。
    assert canonical_json({"k": "a\nb"}) == b'{"k":"a\\nb"}'


def test_template_hash_ignores_spacing_and_key_order():
    assert template_hash(json.loads('{"b":1,"a":[2,3]}')) == template_hash(
        json.loads('{ "a" : [2,3] ,\n "b":1 }'))
    assert template_hash(json.loads('{"b":1,"a":[2,3]}')) != template_hash(json.loads('{"b":2,"a":[2,3]}'))


def test_hash_changes_with_every_input():
    base = version_task_hash(EPISODES, RIGHTS, TEMPLATE, "medium")
    assert version_task_hash(EPISODES, RIGHTS, TEMPLATE, "light") != base  # 换档位
    assert version_task_hash(EPISODES[:1], RIGHTS, TEMPLATE, "medium") != base  # 换一集
    assert version_task_hash(EPISODES, "4" * 64, TEMPLATE, "medium") != base  # 换权属材料
    assert version_task_hash(EPISODES, RIGHTS, {"片头": {"路径": "别的.mp4", "时长": 12},
                                                 "变速": 1.02}, "medium") != base  # 改模板


def test_regenerate_vectors():
    """UPDATE_VERSION_TASK_VECTORS=1 时重写向量文件；否则跳过（普通跑只做对拍）。"""
    if os.environ.get("UPDATE_VERSION_TASK_VECTORS") != "1":
        pytest.skip("设置 UPDATE_VERSION_TASK_VECTORS=1 才重新生成向量")
    VECTORS_PATH.write_text(
        json.dumps(build_vectors(), ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
