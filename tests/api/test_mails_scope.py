# -*- coding: utf-8 -*-
"""回归测试：计费关闭时，绑定用户的 key 查 /api/v1/mails 不能被误判成无权限。

背景（线上事故）：
  zcode 用 ak_... 查 /api/v1/mails，key 绑了 user_id=1。
  但当时计费关闭、email_orders 表为空 → 归属校验得出"没有任何邮箱"
  → 恒返回 total=0。zcode 的 wait_link 走这个接口，永远拿不到验证链接。
"""
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

from fastapi.testclient import TestClient  # noqa: E402

from app.database import SessionLocal, init_db  # noqa: E402
from app.main import app  # noqa: E402
from app.models import ApiKey, MailRecord, User  # noqa: E402

ADMIN = {"X-Admin-Password": "adminpass"}


@pytest.fixture(scope="module")
def client():
    init_db()
    return TestClient(app)


@pytest.fixture()
def seeded():
    """造一个绑定用户的 key + 一条收件记录。"""
    init_db()
    db = SessionLocal()
    try:
        # 保证 api_keys 有绑定用户的 key（不能让 _verify_api_key 走"没配 key 就放行"）
        if not db.query(User).filter_by(username="probe_bound_user").first():
            db.add(User(username="probe_bound_user", password_hash="x", status=1))
            db.commit()
        u = db.query(User).filter_by(username="probe_bound_user").one()
        key_row = db.query(ApiKey).filter_by(key="ak_probe_bound_key_0001").first()
        if not key_row:
            db.add(ApiKey(key="ak_probe_bound_key_0001", user_id=u.id, enabled=True))
            db.commit()
        if not db.query(MailRecord).filter_by(message_id="probe-mid-linksite").first():
            db.add(MailRecord(
                email="probe_link@example.com",
                project_code="probe_prj",
                message_id="probe-mid-linksite",
                sender="noreply@email.z.ai",
                subject="Verify Your Email",
                body='<a href="https://chat.z.ai/auth/verify_email?token=abc">Verify</a>',
                body_preview="Hello",
                code="",
                received_at=None,
            ))
            db.commit()
        yield {"key": "ak_probe_bound_key_0001", "user_id": u.id,
               "email": "probe_link@example.com"}
    finally:
        db.query(MailRecord).filter_by(message_id="probe-mid-linksite").delete(
            synchronize_session=False)
        db.commit()
        db.close()


def test_bound_key_can_read_mails_when_billing_off(client, seeded):
    """计费关闭时，绑定用户的 key 应能查到邮件（不能被订单归属误杀）。"""
    from app.services import billing_service

    db = SessionLocal()
    try:
        # 前置：确认计费是关闭的（本测试的前提）
        assert billing_service.is_billing_enabled(db) is False, \
            "本测试要求计费关闭；若计费已开启，归属校验应生效，此用例不适用"
    finally:
        db.close()

    r = client.get(f"/api/v1/mails?email={seeded['email']}",
                   headers={"Authorization": f"Bearer {seeded['key']}"})
    assert r.status_code == 200, r.text
    data = r.json()["data"]
    assert data["total"] >= 1, (
        f"绑定用户的 key 查不到自己的邮件（total={data['total']}）——"
        "wait_link 会永远拿不到验证链接"
    )
    items = data["items"]
    assert any("chat.z.ai" in (it.get("body") or "") for it in items)


def test_admin_header_still_works(client, seeded):
    """管理员口令头保持全量可见。"""
    r = client.get(f"/api/v1/mails?email={seeded['email']}", headers=ADMIN)
    assert r.status_code == 200
    assert r.json()["data"]["total"] >= 1


def test_no_auth_rejected(client):
    r = client.get("/api/v1/mails")
    assert r.status_code in (401, 403)
