# -*- coding: utf-8 -*-
"""并发取号不再误报 D0002 的回归测试。

背景（生产实测，2026-11-21）：
    池子里有 828 个可发账号，单个项目也还有 710+ 库存，但
        串行 12 → 12/12
        并发 5  → 5/5      （恰好 = PROBE_PER_ROUND(8) 以内）
        并发 9  → 8/9
        并发 10 → 8/10
        并发 20 → 8/20
    上限恰好是每轮探测批量 8。根因是 claim_account **先验号再原子占位**：
    并发线程会读到同一批 8 个候选、一起验 token，只有一个人抢得到，
    抢输的线程直接 break 掉整个循环 → 库存明明充足却报「邮箱库存不足」。

本测试锁定修复后的语义：并发 N 个线程必须全部拿到号（N 远大于 8）。
"""
import os
import sys
import threading

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

from app.database import SessionLocal, init_db  # noqa: E402
from app.models import EmailAccount, EmailUsage  # noqa: E402
from app.services import pool_service  # noqa: E402

PROV = "racepool"
PROJECT = "race_proj"
N = 20  # 必须 > PROBE_PER_ROUND(8)，否则测不出这个 bug


@pytest.fixture()
def db():
    init_db()
    s = SessionLocal()

    def _clean():
        s.query(EmailAccount).filter(EmailAccount.provider == PROV).delete(
            synchronize_session=False)
        s.query(EmailUsage).filter(EmailUsage.project_code == PROJECT).delete(
            synchronize_session=False)
        s.commit()

    _clean()
    try:
        yield s
    finally:
        _clean()
        s.close()


def _seed(db, n):
    for i in range(n):
        db.add(EmailAccount(
            email=f"race{i:03d}@fake.test",
            provider=PROV,
            pool_status="available",
            health_status="healthy",
            status="available",
            account_json='{"client_id": "cid", "refresh_token": "rt"}',
        ))
    db.commit()


def test_concurrent_claim_above_probe_batch(db, monkeypatch):
    """20 个线程并发取号，池里 40 个号 → 必须 20/20 成功，不能报无号。"""
    _seed(db, 40)
    # 每个线程用自己的 session（贴近 FastAPI 每请求一个 session 的真实模型）
    monkeypatch.setattr(pool_service, "_probe_account", lambda acc: (True, ""))

    got, errors = [], []
    lock = threading.Lock()
    barrier = threading.Barrier(N)

    def worker():
        s = SessionLocal()
        try:
            barrier.wait(timeout=30)  # 尽量让所有线程在同一瞬间开抢
            res = pool_service.claim_account(s, PROV, PROJECT)
            with lock:
                if res is None:
                    errors.append("no_stock")
                else:
                    got.append(res["email"])
        except Exception as exc:  # noqa: BLE001
            with lock:
                errors.append(f"{type(exc).__name__}: {exc}")
        finally:
            s.close()

    threads = [threading.Thread(target=worker) for _ in range(N)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=60)

    assert not errors, f"并发取号失败 {len(errors)} 个: {errors[:5]}"
    assert len(got) == N, f"应当拿到 {N} 个不同邮箱，实际 {len(got)}"
    assert len(set(got)) == N, "同一个邮箱被发给了多个调用方（丢失更新）"


def test_concurrent_claim_real_stock_exhausted(db, monkeypatch):
    """池里只有 5 个号、20 个线程抢 → 5 个成功、15 个如实报无号（不能超发）。"""
    _seed(db, 5)
    monkeypatch.setattr(pool_service, "_probe_account", lambda acc: (True, ""))

    got, empty = [], []
    lock = threading.Lock()
    barrier = threading.Barrier(N)

    def worker():
        s = SessionLocal()
        try:
            barrier.wait(timeout=30)
            res = pool_service.claim_account(s, PROV, PROJECT)
            with lock:
                (got if res else empty).append(res["email"] if res else None)
        finally:
            s.close()

    threads = [threading.Thread(target=worker) for _ in range(N)]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=60)

    assert len(got) == 5, f"只有 5 个号却发出 {len(got)} 个"
    assert len(set(got)) == 5, "重复发放了同一个邮箱"
    assert len(empty) == N - 5


def test_dead_accounts_do_not_consume_a_round(db, monkeypatch):
    """一整批候选都是死号时，必须继续扫下一批（而不是直接放弃）。"""
    _seed(db, 30)
    state = {"n": 0}

    def fake_probe(acc):
        state["n"] += 1
        # 前 10 个判死，其余健康：需要跨过一批 8 个才能拿到号
        return (False, "risk_control: service abuse mode") if state["n"] <= 10 else (True, "")

    monkeypatch.setattr(pool_service, "_probe_account", fake_probe)
    res = pool_service.claim_account(db, PROV, PROJECT)
    assert res is not None, "一批死号之后应当继续扫，不能直接报无号"
