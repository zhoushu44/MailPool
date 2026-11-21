# -*- coding: utf-8 -*-
"""超时占用回收测试。

背景（2026-11-21 生产实况）：49 个 `using` 里有 38 个早过了 15 分钟租约还没回池，
把每个项目的库存都白扣了 —— 因为回收只在 `claim_account` 里顺带跑，没人取号就一直不收。
这里守住两条：
  1. `recover_abandoned_claims` 只收「超过租约」的，没到点的不动；
  2. 定时任务 `_reclaim_expired_claims_job` 真的能独立触发回收（不靠取号）。

数据隔离：用独立 provider（"reclaimpool"）+ 独立邮箱前缀，只清理自己造的行。
"""
import os
import sys
from datetime import datetime, timedelta, timezone

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

from app.database import SessionLocal, init_db  # noqa: E402
from app.models import EmailAccount, EmailUsage  # noqa: E402
from app.services import pool_service  # noqa: E402

PROV = "reclaimpool"


@pytest.fixture()
def db():
    init_db()
    s = SessionLocal()
    _cleanup(s)
    try:
        yield s
    finally:
        _cleanup(s)
        s.close()


def _cleanup(s):
    s.query(EmailAccount).filter(EmailAccount.provider == PROV).delete(synchronize_session=False)
    s.query(EmailUsage).filter(EmailUsage.project_code.like("reclaim_%")).delete(
        synchronize_session=False)
    s.commit()


def _mk_using(s, email, minutes_ago):
    acc = EmailAccount(
        email=email,
        provider=PROV,
        pool_status="using",
        health_status="healthy",
        status="using",
        claimed_at=datetime.now(timezone.utc).replace(tzinfo=None) - timedelta(minutes=minutes_ago),
        account_json="{}",
    )
    s.add(acc)
    s.commit()
    return acc


def test_only_expired_claims_are_recovered(db):
    """超租约的回池，没到点的一动不动。"""
    expired = _mk_using(db, "reclaim_old@fake.test", minutes_ago=20)
    fresh = _mk_using(db, "reclaim_new@fake.test", minutes_ago=2)

    result = pool_service.recover_abandoned_claims(db)

    assert result["recovered"] == 1, result
    db.expire_all()
    assert db.get(EmailAccount, expired.id).pool_status == "available"
    assert db.get(EmailAccount, expired.id).claimed_at is None
    assert db.get(EmailAccount, fresh.id).pool_status == "using"
    assert db.get(EmailAccount, fresh.id).claimed_at is not None


def test_legacy_status_synced_on_recovery(db):
    """回池时旧的 status 字段要跟着 health_status 走，否则 UI 显示对不上。"""
    acc = _mk_using(db, "reclaim_legacy@fake.test", minutes_ago=30)

    pool_service.recover_abandoned_claims(db)

    db.expire_all()
    row = db.get(EmailAccount, acc.id)
    assert row.pool_status == "available"
    assert row.status == "healthy", row.status


def test_scheduled_job_recovers_without_any_claim(db):
    """定时任务本身就能回收 —— 这是本次修复的核心（不依赖有没有人取号）。"""
    import app.main as main_mod

    _mk_using(db, "reclaim_job1@fake.test", minutes_ago=16)
    _mk_using(db, "reclaim_job2@fake.test", minutes_ago=40)
    _mk_using(db, "reclaim_job_fresh@fake.test", minutes_ago=1)

    main_mod._reclaim_expired_claims_job()

    db.expire_all()
    rows = {
        r.email: r.pool_status
        for r in db.query(EmailAccount).filter(EmailAccount.provider == PROV).all()
    }
    assert rows["reclaim_job1@fake.test"] == "available"
    assert rows["reclaim_job2@fake.test"] == "available"
    assert rows["reclaim_job_fresh@fake.test"] == "using"


def test_recovered_stock_visible_to_other_project(db):
    """回收后别的项目库存立刻能看到这个号（按项目一次性语义）。"""
    _mk_using(db, "reclaim_stock@fake.test", minutes_ago=20)
    before = pool_service.count_claimable(db, PROV, "reclaim_proj")

    pool_service.recover_abandoned_claims(db)

    after = pool_service.count_claimable(db, PROV, "reclaim_proj")
    assert after == before + 1, (before, after)
