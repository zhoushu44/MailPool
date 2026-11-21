# -*- coding: utf-8 -*-
"""claim 预检测试：死号（service abuse / 无凭据）不能发出去，且被标记 retired。

数据隔离：用独立的 provider id（"probepool"）+ 独立邮箱前缀，
测试前后只清理自己造的行，不碰共享库里的其它数据。
"""
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

from app.database import SessionLocal, init_db  # noqa: E402
from app.models import EmailAccount, EmailUsage  # noqa: E402
from app.services import pool_service  # noqa: E402

PROV = "probepool"  # 非 outlook → 走真实预检需手动打桩


@pytest.fixture()
def db():
    init_db()
    s = SessionLocal()
    s.query(EmailAccount).filter(EmailAccount.provider == PROV).delete(synchronize_session=False)
    s.query(EmailUsage).filter(EmailUsage.project_code.like("probe_%")).delete(
        synchronize_session=False)
    s.commit()
    try:
        yield s
    finally:
        s.query(EmailAccount).filter(EmailAccount.provider == PROV).delete(synchronize_session=False)
        s.query(EmailUsage).filter(EmailUsage.project_code.like("probe_%")).delete(
            synchronize_session=False)
        s.commit()
        s.close()


def _mk(db, email, provider=PROV, aj=None, health="healthy"):
    acc = EmailAccount(
        email=email,
        provider=provider,
        pool_status="available",
        health_status=health,
        status="available",
        account_json=aj if aj is not None else '{"client_id": "cid", "refresh_token": "rt"}',
    )
    db.add(acc)
    return acc


def test_dead_account_skipped_and_retired(db, monkeypatch):
    """预检报死 → 该号不发，标记 retired/dead，继续取下一个。"""
    _mk(db, "probe_dead@fake.test")
    _mk(db, "probe_alive@fake.test")
    db.commit()
    calls = []

    def fake_probe(acc):
        calls.append(acc.email)
        if acc.email.startswith("probe_dead"):
            return False, "risk_control: service abuse mode"
        return True, ""

    monkeypatch.setattr(pool_service, "_probe_account", fake_probe)
    got = pool_service.claim_account(db, PROV, "probe_x")
    assert got is not None, "应当仍能取到健康号"
    assert got["email"] == "probe_alive@fake.test"
    assert calls[0].startswith("probe_dead")

    dead = db.query(EmailAccount).filter_by(email="probe_dead@fake.test").one()
    assert dead.pool_status == "retired"
    assert dead.health_status == "dead"
    assert "service abuse" in (dead.health_reason or "")


def test_no_oauth_credential_considered_dead(db):
    """outlook 号缺 client_id/refresh_token → 预检判死（provider 是 outlook 才验）。"""
    acc = _mk(db, "probe_noauth@outlook.com", provider="outlook", aj='{"password": "pw"}')
    db.commit()
    alive, reason = pool_service._probe_account(acc)
    assert alive is False
    assert "credential_invalid" in reason


def test_non_outlook_provider_not_probed(db):
    """非 outlook provider 不做 OAuth 预检（否则全部误杀）。"""
    acc = _mk(db, "probe_x@fake.test", aj="{}")
    db.commit()
    alive, reason = pool_service._probe_account(acc)
    assert alive is True
    assert reason == ""


def test_all_dead_returns_none(db, monkeypatch):
    """整池都是死号：返回 None，且全部标记 retired。"""
    for i in range(3):
        _mk(db, f"probe_dead{i}@fake.test")
    db.commit()
    monkeypatch.setattr(pool_service, "_probe_account",
                        lambda acc: (False, "risk_control: service abuse mode"))
    assert pool_service.claim_account(db, PROV, "probe_y") is None
    rows = db.query(EmailAccount).filter(EmailAccount.provider == PROV).all()
    assert all(r.pool_status == "retired" for r in rows)
    assert pool_service.count_claimable(db, PROV, "probe_y") == 0
