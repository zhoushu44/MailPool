# -*- coding: utf-8 -*-
"""refresh_token 轮换回写 + 互斥刷新的单元测试。

背景：微软 IMAP scope 刷新会轮换 rt，旧实现把新 rt 留在内存，
库里的 rt 立即作废 → 全组件 invalid_grant → 降级密码 IMAP 撞
"Basic authentication is disabled" → 收码超时。
"""
import os
import sys
import threading
import time

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

from app.providers import outlook  # noqa: E402


class _FakeResp:
    def __init__(self, payload):
        self.payload = payload

    def read(self):
        import json

        return json.dumps(self.payload).encode()


@pytest.fixture()
def _patch_token(monkeypatch):
    """_request_access_token 打桩：每次返回新 rt，并记录调用用的 rt。"""
    calls = []
    state = {"n": 0}

    def fake(rt, cid, scope, proxy=None):
        calls.append(rt)
        state["n"] += 1
        return {"access_token": f"at-{state['n']}", "refresh_token": f"rt-{state['n']}"}

    monkeypatch.setattr(outlook, "_request_access_token", fake)
    return calls


def test_rotation_persisted_via_hook(_patch_token, monkeypatch):
    saved = []
    outlook.set_rt_save_hook(lambda email, rt: saved.append((email, rt)))
    try:
        cred = {"email": "a@x.test", "refresh_token": "rt-0", "client_id": "cid"}
        data = outlook._refresh_token_for(cred, outlook.GRAPH_SCOPE)
        assert data["access_token"] == "at-1"
        assert cred["refresh_token"] == "rt-1"          # 内存同步
        assert saved == [("a@x.test", "rt-1")]          # hook 落库被调
    finally:
        outlook.set_rt_save_hook(None)


def test_no_rotation_no_hook(_patch_token, monkeypatch):
    """Graph scope 桩不返回 rt 时不应触发 hook。"""

    def fake(rt, cid, scope, proxy=None):
        return {"access_token": "at-x"}

    monkeypatch.setattr(outlook, "_request_access_token", fake)
    saved = []
    outlook.set_rt_save_hook(lambda email, rt: saved.append((email, rt)))
    try:
        cred = {"email": "b@x.test", "refresh_token": "rt-0", "client_id": "cid"}
        outlook._refresh_token_for(cred, outlook.GRAPH_SCOPE)
        assert saved == []
        assert cred["refresh_token"] == "rt-0"
    finally:
        outlook.set_rt_save_hook(None)


def test_concurrent_refresh_serialized(_patch_token):
    """同邮箱并发刷新互斥：两个线程串行执行，且第二次用锁内重读的新 rt。"""
    saved = []
    outlook.set_rt_save_hook(lambda email, rt: saved.append(rt))
    try:
        cred = {"email": "c@x.test", "refresh_token": "rt-0", "client_id": "cid"}
        results = []

        def worker():
            results.append(outlook._refresh_token_for(cred, outlook.IMAP_SCOPE)["access_token"])

        t1 = threading.Thread(target=worker)
        t2 = threading.Thread(target=worker)
        t1.start(); time.sleep(0.05); t2.start()
        t1.join(); t2.join()

        assert results == ["at-1", "at-2"]
        # 第二次刷新用的 rt 是第一次轮换出来的 rt-1（锁内重读 cred）
        assert _patch_token == ["rt-0", "rt-1"]
        assert cred["refresh_token"] == "rt-2"
        assert saved == ["rt-1", "rt-2"]
    finally:
        outlook.set_rt_save_hook(None)


def test_imap_no_oauth_is_fatal():
    """无 OAuth 凭据：IMAP 通道直接 fatal，不再撞 basic auth。"""
    from app.providers.base import ProviderError  # noqa: F401  确认可导入

    prov = outlook.OutlookEmailProvider()
    with pytest.raises(Exception) as ei:
        prov._imap_wait_code(
            {"email": "d@x.test", "password": "pw"}, "d@x.test", timeout=5)
    assert "OAuth" in str(ei.value) or "oauth" in str(ei.value)
