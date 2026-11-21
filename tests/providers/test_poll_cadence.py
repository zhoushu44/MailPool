# -*- coding: utf-8 -*-
"""收码轮询节奏的回归测试。

背景（2026-11-21 实测）：
    旧版每轮扫 inbox/junkemail/deleteditems 三个文件夹 + 固定 sleep(4)。
    12 秒窗口内只能跑 2.9 轮；并发 25 时单轮被拖到 4~5 秒，轮次更少，
    邮件晚到几秒就错过（客户端报"同一个邮箱轮询 11 次还没拿到码"）。

    实测 90% 的验证码只落在 inbox。改成 inbox 每轮都查、
    垃圾箱/已删除每 5 轮兜底扫一次、轮间 sleep 4s → 1s，
    同样窗口内轮次 2.9 → 10.7。

这些测试锁住"节奏"，避免以后又改回每轮扫 3 个文件夹。
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
    """构造 provider，并把 token 刷新与真实网络全部短路。"""
    p = ol.OutlookEmailProvider()
    monkeypatch.setattr(ol, "_refresh_token_for", lambda *a, **k: {"access_token": "fake"})
    return p


def _counting_list(monkeypatch, calls):
    """替换 _graph_list_messages：记录被查的 folder，返回空收件箱。"""

    def fake(token, folder, timeout=15):
        calls.append((folder, time.time()))
        return []

    monkeypatch.setattr(ol, "_graph_list_messages", fake)


def test_inbox_polled_every_round_side_folders_every_5th(prov, monkeypatch):
    """inbox 每轮都查；junkemail / deleteditems 只在第 1、6、11… 轮查。"""
    calls = []
    _counting_list(monkeypatch, calls)
    prov._graph_wait_code({"refresh_token": "rt", "client_id": "cid"},
                          "a@b.test", 3.0, set())

    folders = [f for f, _ in calls]
    inbox_n = folders.count("inbox")
    side_n = sum(1 for f in folders if f != "inbox")

    assert inbox_n >= 2, f"3 秒窗口内 inbox 应被查多次，实际 {inbox_n} 次"
    # 侧文件夹（垃圾箱+已删除）每 5 轮一次，必须明显少于 inbox
    assert side_n < inbox_n, f"侧文件夹不应每轮都查：inbox={inbox_n} side={side_n}"
    assert set(folders) == {"inbox", "junkemail", "deleteditems"}, (
        f"三个文件夹最终都要覆盖，实际 {set(folders)}"
    )


def test_more_rounds_than_old_strategy(prov, monkeypatch):
    """12 秒窗口内轮询轮次应显著多于旧版的 ~3 轮。"""
    calls = []
    _counting_list(monkeypatch, calls)

    t0 = time.time()
    prov._graph_wait_code({"refresh_token": "rt", "client_id": "cid"},
                          "a@b.test", 12.0, set())
    elapsed = time.time() - t0

    inbox_n = sum(1 for f, _ in calls if f == "inbox")
    # 旧版 12s 只能跑约 3 轮；新版应在 8 轮以上
    assert inbox_n >= 8, f"12s 内 inbox 轮次太少（{inbox_n}），优化被回退了？"
    # 同时不能明显超时（旧版会 +4s，因为 sleep 在超时判定之后）
    assert elapsed < 14.0, f"耗时 {elapsed:.1f}s 超出窗口太多"


def test_returns_immediately_on_hit(prov, monkeypatch):
    """命中邮件应立即返回，不等到窗口结束。"""
    from datetime import datetime, timezone

    now = datetime.now(timezone.utc)
    prov.set_baseline(set(), now)

    def fake(token, folder, timeout=15):
        if folder != "inbox":
            return []
        return [{
            "id": "mid-1",
            "subject": "Your verification code",
            "bodyPreview": "Your code is 123456",
            "receivedDateTime": datetime.now(timezone.utc).isoformat(),
            "from": {"emailAddress": {"address": "noreply@openai.com"}},
        }]

    monkeypatch.setattr(ol, "_graph_list_messages", fake)
    monkeypatch.setattr(ol, "_graph_get_message_rest",
                        lambda *a, **k: {"body": {"content": "<p>Your code is 123456</p>"}})

    t0 = time.time()
    mail = prov._graph_wait_code({"refresh_token": "rt", "client_id": "cid"},
                                 "a@b.test", 12.0, set())
    elapsed = time.time() - t0

    assert mail is not None, "命中邮件却返回 None"
    assert mail["code"] == "123456", mail
    assert elapsed < 3.0, f"命中后应立即返回，实际耗时 {elapsed:.1f}s"
