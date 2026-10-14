"""长任务进度跟随（Tier 3）：JSONL 增量读 + 边跑边报。

这一层解决的是「工具已经知道跑到第几帧了，界面却只有一个静止的 RUNNING」：
擦除工具按行追加进度文件，Worker 用一个后台线程按字节偏移增量读，把每条事件
落进任务进度。
"""

from __future__ import annotations

import json
import sys
import threading
import time

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.modules.ffmpeg import read_progress_events, run_streaming


def _event(seq: int, done: int, total: int) -> str:
    return json.dumps({"seq": seq, "kind": "progress", "unitDone": done, "unitTotal": total})


def test_reader_advances_only_past_complete_lines(tmp_path):
    """半行不能当事件上报，也不能让偏移跳过它——长任务随时可能在半行处被杀。"""
    path = tmp_path / "progress.jsonl"
    path.write_text(_event(1, 1, 10) + "\n" + '{"seq":2,"kind":"pro', encoding="utf-8")

    events, offset = read_progress_events(path, 0)
    assert [event["seq"] for event in events] == [1]

    path.write_text(path.read_text(encoding="utf-8") + 'gress"}\n', encoding="utf-8")
    events, _ = read_progress_events(path, offset)
    assert [event["seq"] for event in events] == [2]


def test_reader_skips_broken_lines_and_missing_files(tmp_path):
    path = tmp_path / "progress.jsonl"
    path.write_text("不是 JSON\n" + _event(7, 1, 2) + "\n", encoding="utf-8")
    events, _ = read_progress_events(path, 0)
    assert [event["seq"] for event in events] == [7]

    events, offset = read_progress_events(tmp_path / "missing.jsonl", 0)
    assert events == [] and offset == 0


def test_run_streaming_without_subscription_is_plain_run(tmp_path):
    path = tmp_path / "progress.jsonl"
    result = run_streaming(
        [sys.executable, "-c", "print('hi')"], progress_path=path, on_progress=None, timeout=30
    )
    assert result.returncode == 0 and result.stdout.strip() == b"hi"
    assert not path.exists(), "没订阅就不该假装产出了进度"


def test_run_streaming_reports_every_event_once(tmp_path):
    path = tmp_path / "progress.jsonl"
    seen: list = []
    script = (
        "import json,sys\n"
        "p=sys.argv[1]\n"
        "for index in range(3):\n"
        "    open(p,'a').write(json.dumps("
        "{'seq':index+1,'unitDone':index+1,'unitTotal':3})+'\\n')\n"
    )
    result = run_streaming(
        [sys.executable, "-c", script, str(path)],
        progress_path=path,
        on_progress=seen.append,
        timeout=30,
        poll=0.05,
    )
    assert result.returncode == 0
    assert [event["seq"] for event in seen] == [1, 2, 3]


def test_events_arrive_while_the_child_is_still_running(tmp_path):
    """关键区别：进度必须在子进程结束**之前**就上报，否则等于跑完才读。"""
    path = tmp_path / "progress.jsonl"
    seen: list = []
    script = (
        "import json,sys,time\n"
        "p=sys.argv[1]\n"
        "open(p,'a').write(json.dumps({'seq':1,'unitDone':1,'unitTotal':2})+'\\n')\n"
        "time.sleep(2.0)\n"
        "open(p,'a').write(json.dumps({'seq':2,'unitDone':2,'unitTotal':2})+'\\n')\n"
    )
    thread = threading.Thread(
        target=lambda: run_streaming(
            [sys.executable, "-c", script, str(path)],
            progress_path=path,
            on_progress=seen.append,
            timeout=30,
            poll=0.05,
        ),
        daemon=True,
    )
    thread.start()
    deadline = time.monotonic() + 1.5
    while time.monotonic() < deadline and not seen:
        time.sleep(0.05)
    assert [event["seq"] for event in seen] == [1], "第一条事件没有在子进程存活期间上报"
    thread.join(timeout=15)


def test_run_streaming_timeout_is_a_classified_error(tmp_path):
    path = tmp_path / "progress.jsonl"
    with pytest.raises(WorkError) as error:
        run_streaming(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            progress_path=path,
            on_progress=lambda event: None,
            timeout=0.5,
            poll=0.05,
        )
    assert error.value.error_class is ErrorClass.RESOURCE_CONTENTION
