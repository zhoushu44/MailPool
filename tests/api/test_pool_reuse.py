# -*- coding: utf-8 -*-
"""按项目一次性复用语义的测试。

规则（2026-11-20 改）：
  - 同一邮箱同一项目只能用一次（A 用过 → A 再取拿不到）
  - 其他项目不受影响（A 用过 → B 照常能取）
  - 释放回池（不再退役），绑定记录保留
  - 库存按项目各自统计
"""
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

# MAILPOOL_DATA 统一在 tests/conftest.py 里设（必须在 import app.* 之前）
os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

from fastapi.testclient import TestClient  # noqa: E402

from app.database import SessionLocal, init_db  # noqa: E402
from app.main import app  # noqa: E402
from app.services import pool_service  # noqa: E402


ADMIN = {"X-Admin-Password": "adminpass"}


class FakePoolProv:
    """假号池 provider：从固定列表顺序发号，验证 claim 的排除逻辑。"""

    pooled = True
    ephemeral = False
    id = "fakepool"

    def __init__(self, config=None):
        self._config = config or {}
        self._n = 0

    def set_credentials(self, email, cred):
        pass

    def generate_address(self):
        self._n += 1
        return f"pool{self._n:03d}@fake.test"

    async def wait_code(self, email, *, timeout=30, project=None):
        return "123456"

    async def wait_mail(self, email, *, timeout=30, project=None):
        return {"email": email, "code": "123456", "message_id": "m1",
                "sender": "a@b.test", "subject": "s", "body": "code 123456"}

    def release(self, email):
        return None


@pytest.fixture(scope="module", autouse=True)
def _setup():
    init_db()
    from app.services import mail_service

    # 模块级 patch（monkeypatch 是 function 级，这里手动换）
    orig = mail_service.create_provider
    mail_service.create_provider = lambda pid, cfg: FakePoolProv(cfg)
    db = SessionLocal()
    try:
        for code in ("reuse_a", "reuse_b"):
            mail_service.upsert_project(
                db, code, f"项目{code}", "fakepool", "{}", enabled=1)
        for i in range(1, 6):
            rows, _ = pool_service.parse_account_text(
                f"pool{i:03d}@fake.test----pw----cid----tok", "fakepool")
            pool_service.import_accounts(db, "fakepool", rows)
        db.commit()
    finally:
        db.close()
    yield
    mail_service.create_provider = orig


@pytest.fixture(scope="module")
def client():
    return TestClient(app)


def _mk_user(client):
    import uuid

    r = client.post("/api/admin/billing/users", headers=ADMIN,
                    json={"username": "ru" + uuid.uuid4().hex[:8], "password": "pw"})
    return r.json()["data"]["id"]


def _key_for(client, uid):
    r = client.post("/api/admin/billing/api-keys", headers=ADMIN,
                    json={"user_id": uid, "name": "t"})
    return r.json()["data"]["key"]


def test_same_project_blocked_other_project_ok(client):
    """核心语义：A 用过的邮箱 A 拿不到，B 照常拿到。"""
    uid = _mk_user(client)
    key = _key_for(client, uid)
    auth = {"Authorization": f"Bearer {key}"}

    # A 取第一封
    r = client.post("/api/v1/email/temporary/buy",
                    json={"project_code": "reuse_a"}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    email_a = r.json()["data"]["email"]

    # B 取号：不能拿到 A 刚用过的这封（它还在 using），应拿到别的
    r = client.post("/api/v1/email/temporary/buy",
                    json={"project_code": "reuse_b"}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    email_b = r.json()["data"]["email"]
    assert email_b != email_a

    # A 释放 → 回池 available（不再 retired）
    r = client.post("/api/v1/email/release",
                    json={"project_code": "reuse_a", "email": email_a}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    assert r.json()["data"]["status"] == "released", r.text

    db = SessionLocal()
    try:
        from app.models import EmailAccount
        acc = db.query(EmailAccount).filter(EmailAccount.email == email_a).first()
        assert acc.pool_status == "available", acc.pool_status
    finally:
        db.close()

    # B 再取：可以拿到 A 释放回池的那封（B 没用过它）
    r = client.post("/api/v1/email/temporary/buy",
                    json={"project_code": "reuse_b"}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    assert r.json()["data"]["email"] == email_a, (
        f"B 应能取到 A 用过的 {email_a}，实际拿到 {r.json()['data']['email']}"
    )

    # A 再取：拿不到自己用过的 email_a（绑定保留），拿到下一封
    r = client.post("/api/v1/email/temporary/buy",
                    json={"project_code": "reuse_a"}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    assert r.json()["data"]["email"] != email_a


def test_stock_per_project(client):
    """库存按项目算：A 用过的号只从 A 的库存里扣。"""
    db = SessionLocal()
    try:
        from app.models import EmailAccount, EmailUsage

        # 自备环境：5 个号全部回池，清掉两个项目的绑定，只留 pool001→reuse_a
        db.query(EmailUsage).filter(
            EmailUsage.project_code.in_(("reuse_a", "reuse_b"))
        ).delete(synchronize_session=False)
        db.query(EmailAccount).filter(
            EmailAccount.provider == "fakepool"
        ).update({"pool_status": "available"}, synchronize_session=False)
        db.add(EmailUsage(email="pool001@fake.test", project_code="reuse_a"))
        db.commit()

        stock_a = pool_service.count_claimable(db, "fakepool", "reuse_a")
        stock_b = pool_service.count_claimable(db, "fakepool", "reuse_b")
        assert stock_a == 4, stock_a   # A 被扣掉 pool001
        assert stock_b == 5, stock_b   # B 不受影响
    finally:
        db.close()


def test_bind_email_per_project(client):
    """bind_email：同项目冲突，跨项目放行。"""
    db = SessionLocal()
    try:
        ok1, c1 = pool_service.bind_email(db, "bindtest@fake.test", "reuse_a")
        assert ok1, c1
        ok2, c2 = pool_service.bind_email(db, "bindtest@fake.test", "reuse_a")
        assert not ok2 and c2 == "reuse_a"      # 同项目 → 冲突
        ok3, c3 = pool_service.bind_email(db, "bindtest@fake.test", "reuse_b")
        assert ok3, (ok3, c3)                    # 跨项目 → 放行
    finally:
        db.close()
