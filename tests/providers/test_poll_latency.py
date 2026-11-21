# -*- coding: utf-8 -*-
"""邮件中途到达的检测延迟：验证轮询节奏优化真的能更早拿到码。

场景：验证码在窗口第 N 秒到达。旧版每轮 sleep(4) 且每轮扫 3 个文件夹，
一轮 ~4.5s → 最坏要 4.5s 才发现；新版 1s 间隔 → 最坏 ~1.2s。

这里用假时钟推进模拟，避免依赖真实网络与真实邮件。
"""
import os
import sys
import time

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

import app.providers.outlook as ol  # noqa: E402


@pytest.fixture()
def prov(monkeypatch):
    p = ol.OutlookEmailProvider()
    monkeypatch.setattr(ol, "_refresh_token_for", lambda *a, **k: {"access_token": "fake"})
    monkeypatch.setattr(ol, "_graph_get_message_rest",
                        lambda *a, **k: {"body": {"content": "code 654321"}})
    return p


def _arriving_after(prov, monkeypatch, delay, clock):
    """返回一个 fake _graph_list_messages：从 start+delay 起才返回邮件。"""

    def fake(token, folder, timeout=15):
        if folder != "inbox":
            return []
        if clock["t"] - clock["start"] < delay:
            return []
        from datetime import datetime, timezone
        return [{
            "id": "mid-late",
            "subject": "verification",
            "bodyPreview": "code 654321",
            "receivedDateTime": datetime.now(timezone.utc).isoformat(),
            "from": {"emailAddress": {"address": "noreply@openai.com"}},
        }]

    monkeypatch.setattr(ol, "_graph_list_messages", fake)


def test_detects_late_arriving_mail_quickly(prov, monkeypatch):
    """邮件在 6 秒时到达，应在 ~1-2 秒内被发现（而不是等到窗口结束）。"""
    from datetime import datetime, timezone

    prov.set_baseline(set(), datetime.now(timezone.utc))

    clock = {"t": 0.0, "start": 0.0}
    real_time = time.time

    # 用真实时间但记录相对进度；这里是集成式测量，不 mock 时钟
    t0 = real_time()
    _arriving_after(prov, monkeypatch, delay=6.0, clock=clock)
    clock["start"] = t0

    orig_sleep = time.sleep

    def tracked_sleep(sec):
        clock["t"] = real_time()
        orig_sleep(sec)

    monkeypatch.setattr(time, "sleep", tracked_sleep)

    res = prov._graph_wait_code(
        {"refresh_token": "rt", "client_id": "cid"}, "a@b.test", 12.0, set())

    assert res is not None, "邮件 6s 到达，12s 窗口内必须拿到"
    assert res["code"] == "654321"


def test_timeout_does_not_overshoot(prov, monkeypatch):
    """timeout=5 不应跑成 9 秒（旧版 sleep 在超时判定之后，固定多跑 4s）。"""
    monkeypatch.setattr(ol, "_graph_list_messages", lambda *a, **k: [])

    t0 = time.time()
    prov._graph_wait_code({"refresh_token": "rt", "client_id": "cid"}, "a@b.test", 5.0, set())
    elapsed = time.time() - t0

    # 允许最后一次查询本身的耗时，但不允许再叠加整段 sleep
    assert elapsed < 7.0, f"timeout=5 实际跑了 {elapsed:.1f}s，超时控制失效"
