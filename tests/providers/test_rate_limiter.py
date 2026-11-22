# -*- coding: utf-8 -*-
"""出口限流（client_id 级令牌桶）+ 429 不降级的回归测试。

背景（2026-11-22 并发实测）：
    并发 15 = 100% 成功（聚合 ~10 rps）；并发 20 = 60% 成功、收码 230s。
    根因：所有 Graph 轮询从本机直连且共用 client_id，聚合 ~13 rps 撞微软
    client_id+IP 级限流；旧代码收到 429/5xx 直接 raise → 该账号 Graph 通道
    当场死亡 → 整批挤进 IMAP 降级 → 雪崩。

这些测试锁住三件事：
    1. 令牌桶按 client_id 分组、突发+稳态语义正确；
    2. 429/503 只退避不 raise（Graph 通道不许死）；
    3. access_token 进程内缓存生效，重复 wait_code 不再打 login 端点。
"""
import os
import sys
import time
import urllib.error
from datetime import datetime, timedelta, timezone

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

import app.providers.outlook as ol  # noqa: E402


def _http_error(code, headers=None):
    return urllib.error.HTTPError("https://graph.microsoft.com/test", code, "err", headers or {}, None)


# ── 令牌桶 ────────────────────────────────────────────────────
class TestTokenBucket:
    def test_burst_then_rate(self):
        b = ol._TokenBucket(rate=10, burst=3)
        t0 = time.monotonic()
        assert b.acquire(0) is True
        assert b.acquire(0) is True
        assert b.acquire(0) is True          # burst 恰好用完
        assert b.acquire(0.01) is False      # 立刻拿不到
        assert b.acquire(0.3) is True        # 等_rate 补一个（~0.1s）
        assert time.monotonic() - t0 >= 0.09

    def test_buckets_keyed_by_kind_and_client_id(self):
        assert ol._bucket_for("graph", "cid-a") is ol._bucket_for("graph", "CID-A")  # 大小写不敏感
        assert ol._bucket_for("graph", "cid-a") is not ol._bucket_for("graph", "cid-b")
        assert ol._bucket_for("token", "cid-a") is not ol._bucket_for("graph", "cid-a")

    def test_penalize_drains(self):
        b = ol._TokenBucket(rate=1, burst=5)
        b.penalize()
        assert b.acquire(0.05) is False     # 桶清空后短窗口内补不回一个令牌


# ── Retry-After 解析 ─────────────────────────────────────────
class TestRetryAfter:
    def test_seconds_value(self):
        assert ol._retry_after_seconds(_http_error(429, {"Retry-After": "7"})) == 7.0

    def test_missing_header(self):
        assert ol._retry_after_seconds(_http_error(429)) == 0.0

    def test_http_date(self):
        when = (datetime.now(timezone.utc) + timedelta(seconds=30)).strftime(
            "%a, %d %b %Y %H:%M:%S GMT")
        v = ol._retry_after_seconds(_http_error(429, {"Retry-After": when}))
        assert 25.0 <= v <= 31.0


# ── 轮询循环：429 不许打死 Graph 通道 ────────────────────────
class TestPollUnderThrottle:
    @pytest.fixture()
    def prov(self, monkeypatch):
        p = ol.OutlookEmailProvider()
        monkeypatch.setattr(ol, "_refresh_token_for",
                            lambda *a, **k: {"access_token": "fake"})
        return p

    def test_429_backs_off_without_raising(self, prov, monkeypatch):
        """429 应歇 Retry-After 后继续轮询，而不是 raise 出去降级 IMAP。"""
        sleeps: list[float] = []
        monkeypatch.setattr(ol.time, "sleep", lambda s: sleeps.append(s))

        def throttled(token, folder, timeout=15):
            raise _http_error(429, {"Retry-After": "2"})

        monkeypatch.setattr(ol, "_graph_list_messages", throttled)
        result = prov._graph_wait_code(
            {"refresh_token": "rt", "client_id": "c429"}, "x@y.test", 4.0, set())
        assert result is None                     # 没拿到码，但通道没死
        assert sleeps, "429 后应当有退避等待"
        # 至少一次真正的 Retry-After 退避（≥2s）；靠近期限被截断成小值是正常的
        assert max(sleeps) >= 1.9, sleeps
        assert all(0 <= s <= 10.1 for s in sleeps), sleeps

    def test_hard_500_falls_to_imap_after_3_rounds(self, prov, monkeypatch):
        """连续 3 整轮 5xx 才抛 ProviderError 降级 IMAP（单次抖动不许降级）。"""
        monkeypatch.setattr(ol.time, "sleep", lambda s: None)

        def broken(token, folder, timeout=15):
            raise _http_error(500)

        monkeypatch.setattr(ol, "_graph_list_messages", broken)
        with pytest.raises(ol.ProviderError):
            prov._graph_wait_code(
                {"refresh_token": "rt", "client_id": "c500"}, "x@y.test", 30.0, set())

    def test_single_500_round_does_not_degrade(self, prov, monkeypatch):
        """单轮 500 + 后续正常：不该降级，也不该把窗口烧光。"""
        monkeypatch.setattr(ol.time, "sleep", lambda s: None)
        state = {"n": 0}

        def flaky(token, folder, timeout=15):
            state["n"] += 1
            if state["n"] == 1:
                raise _http_error(500)
            return []

        monkeypatch.setattr(ol, "_graph_list_messages", flaky)
        result = prov._graph_wait_code(
            {"refresh_token": "rt", "client_id": "c503"}, "x@y.test", 2.0, set())
        assert result is None
        assert state["n"] >= 2


# ── access_token 缓存 ─────────────────────────────────────────
class TestTokenCache:
    def test_wait_code_reuses_cached_token(self, monkeypatch):
        p = ol.OutlookEmailProvider()
        calls = {"refresh": 0}

        def fake_refresh(*a, **k):
            calls["refresh"] += 1
            return {"access_token": "fresh", "expires_in": 3600}

        monkeypatch.setattr(ol, "_refresh_token_for", fake_refresh)
        tokens: list[str] = []

        def spy(token, folder, timeout=15):
            tokens.append(token)
            return []

        monkeypatch.setattr(ol, "_graph_list_messages", spy)
        cred = {"refresh_token": "rt", "client_id": "cc"}

        p._graph_wait_code(cred, "cache@test", 0.5, set())   # 第一次：刷新 1 次
        p._graph_wait_code(cred, "cache@test", 0.5, set())   # 第二次：应复用缓存
        assert calls["refresh"] == 1, "重复 wait_code 不应重复刷新 token"
        assert tokens and all(t == "fresh" for t in tokens), tokens

        ol._invalidate_token("cache@test", ol.GRAPH_SCOPE)
        p._graph_wait_code(cred, "cache@test", 0.5, set())   # 失效后重新刷新
        assert calls["refresh"] == 2

    def test_cache_ttl_margin(self):
        ol._cache_token("ttl@test", ol.GRAPH_SCOPE, "tok", 3600)
        with ol._token_cache_guard:
            _, exp = ol._token_cache[f"{ol.GRAPH_SCOPE}|ttl@test"]
        remaining = exp - time.time()
        assert 3200 <= remaining <= 3305, remaining  # 3600 - 300s 提前量

    def test_bad_expires_in_falls_back(self):
        ol._cache_token("bad@test", ol.GRAPH_SCOPE, "tok", "abc")  # 字符串垃圾值
        assert ol._cached_token("bad@test", ol.GRAPH_SCOPE) == "tok"


# ── AIMD 自适应速率 ───────────────────────────────────────────
class TestAimd:
    def test_grows_after_good_run(self):
        b = ol._TokenBucket(8, 12, min_rate=3, max_rate=14)
        for _ in range(ol._AIMD_GOOD_RUN):
            b.report("ok")
        assert b.rate == 8.5

    def test_throttle_backs_off_multiplicatively(self):
        b = ol._TokenBucket(8, 12, min_rate=3, max_rate=14)
        b.report("throttle")
        assert b.rate == 5.6          # 8 × 0.7

    def test_slow_is_gentler(self):
        b = ol._TokenBucket(10, 12, min_rate=3, max_rate=14)
        b.report("slow")
        assert b.rate == 9.0          # 10 × 0.9

    def test_bounds_clamp(self):
        b = ol._TokenBucket(8, 12, min_rate=3, max_rate=14)
        for _ in range(50):
            b.report("throttle")
        assert b.rate == 3
        for _ in range(5000):
            b.report("ok")
        assert b.rate == 14


# ── 双车道：IMAP 优先，Graph 兜底 ─────────────────────────────
class TestDualLane:
    CRED = {"client_id": "c", "refresh_token": "r"}

    def test_imap_first_for_unknown_account(self, monkeypatch):
        """未标记的号：IMAP 优先，Graph 不该被调用。"""
        p = ol.OutlookEmailProvider()
        calls = []
        monkeypatch.setattr(p, "_imap_wait_code",
                            lambda *a, **k: calls.append("imap") or None)
        monkeypatch.setattr(p, "_graph_wait_code",
                            lambda *a, **k: calls.append("graph") or None)
        result = p._wait_mail_sync("u@t.test", dict(self.CRED), 2.0, set())
        assert result is None
        assert calls == ["imap"], calls

    def test_no_scope_goes_straight_to_graph(self, monkeypatch):
        """已标记无 IMAP scope（约 30% 池子）：直接 Graph 兜底，不碰 IMAP。"""
        p = ol.OutlookEmailProvider()
        with ol._imap_no_scope_guard:
            ol._imap_no_scope.add("ns@t.test")
        calls = []
        try:
            monkeypatch.setattr(p, "_imap_wait_code",
                                lambda *a, **k: calls.append("imap") or None)
            monkeypatch.setattr(p, "_graph_wait_code",
                                lambda *a, **k: calls.append("graph") or None)
            p._wait_mail_sync("ns@t.test", dict(self.CRED), 2.0, set())
            assert calls == ["graph"], calls
        finally:
            with ol._imap_no_scope_guard:
                ol._imap_no_scope.discard("ns@t.test")

    def test_imap_failure_marks_and_falls_back_to_graph(self, monkeypatch):
        """IMAP 失败：标记无 scope + 用剩余预算走 Graph 兜底，不许判死。"""
        p = ol.OutlookEmailProvider()
        calls = []

        def imap_fail(*a, **k):
            calls.append("imap")
            raise RuntimeError("token 获取失败 (scope=IMAP)")

        monkeypatch.setattr(p, "_imap_wait_code", imap_fail)
        monkeypatch.setattr(p, "_graph_wait_code",
                            lambda *a, **k: calls.append("graph") or None)
        try:
            p._wait_mail_sync("fb@t.test", dict(self.CRED), 2.0, set())
            assert calls == ["imap", "graph"], calls
            with ol._imap_no_scope_guard:
                assert "fb@t.test" in ol._imap_no_scope, "失败号应被标记无 IMAP scope"
        finally:
            with ol._imap_no_scope_guard:
                ol._imap_no_scope.discard("fb@t.test")

    def test_no_oauth_raises_fatal_via_imap(self):
        """无 OAuth 凭据：IMAP 通道直接 fatal（微软已禁 basic auth，保持原语义）。"""
        p = ol.OutlookEmailProvider()
        with pytest.raises(ol.ProviderError):
            p._wait_mail_sync("x@t.test", {}, 2.0, set())


# ── IMAP 持久连接 ─────────────────────────────────────────────
class TestImapPersistentConn:
    def test_single_connection_across_rounds(self, monkeypatch):
        """轮询全程应复用一条连接（NOOP 保活），不再每轮 TLS 重连。"""
        p = ol.OutlookEmailProvider()
        monkeypatch.setattr(ol, "_refresh_token_for",
                            lambda *a, **k: {"access_token": "fake"})

        class FakeIMAP:
            instances = 0

            def __init__(self, host, port, timeout=None):
                FakeIMAP.instances += 1
                self.noops = 0

            def authenticate(self, mech, fn):
                return ("OK", [b""])

            def list(self):
                return ("OK", [b'(\\HasNoChildren) "/" INBOX', b'(\\HasNoChildren) "/" Junk'])

            def select(self, folder, readonly=False):
                return ("OK", [b"0"])

            def search(self, charset, criteria):
                return ("OK", [b""])     # 空收件箱

            def noop(self):
                self.noops += 1
                return ("OK", [b""])

            def logout(self):
                return ("BYE", None)

        monkeypatch.setattr(ol.imaplib, "IMAP4_SSL", FakeIMAP)
        result = p._imap_wait_code(
            {"client_id": "c", "refresh_token": "r"}, "persist@t.test", 4.5, set())
        assert result is None
        assert FakeIMAP.instances == 1, f"应只建 1 条连接，实际 {FakeIMAP.instances}"

    def test_lane_semaphore_released(self, monkeypatch):
        """车道信号量必须归还：连跑两次不会把配额烧穿。"""
        p = ol.OutlookEmailProvider()
        monkeypatch.setattr(ol, "_refresh_token_for",
                            lambda *a, **k: {"access_token": "fake"})
        monkeypatch.setattr(ol.imaplib, "IMAP4_SSL",
                            lambda *a, **k: (_ for _ in ()).throw(OSError("refused")))
        # 连接全失败也会走完 _cleanup 归还车道
        for i in range(ol._IMAP_MAX_CONNS + 2):
            try:
                p._imap_wait_code({"client_id": "c", "refresh_token": "r"},
                                  f"s{i}@t.test", 0.3, set())
            except ol.ProviderError:
                pass
        # 若信号量没归还，这里 acquire 立刻拿满后会超时
        assert ol._imap_lane.acquire(timeout=0.1), "车道信号量泄漏"
        ol._imap_lane.release()
