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
