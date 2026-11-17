# -*- coding: utf-8 -*-
"""计费管理 API 的补充用例（删用户 / 幂等 / 边界）。"""
import os
import sys
import tempfile

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

# MAILPOOL_DATA 统一在 tests/conftest.py 里设（必须在 import app.* 之前）
os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

from fastapi.testclient import TestClient  # noqa: E402

from app.database import init_db  # noqa: E402
from app.main import app  # noqa: E402

ADMIN = {"X-Admin-Password": "adminpass"}


@pytest.fixture(scope="module", autouse=True)
def _db():
    init_db()
    yield


@pytest.fixture(scope="module")
def client():
    return TestClient(app)


def _mk_user(client, name="deluser", amount="0"):
    r = client.post("/api/admin/billing/users", headers=ADMIN,
                     json={"username": name, "password": "pw"})
    assert r.status_code == 200, r.text
    uid = r.json()["data"]["id"]
    if amount != "0":
        r2 = client.post(f"/api/admin/billing/users/{uid}/balance", headers=ADMIN,
                         json={"amount": amount, "remark": "t"})
        assert r2.status_code == 200, r2.text
    return uid


def test_delete_user_removes_everything(client):
    uid = _mk_user(client, "delfull", "5.00")
    # 给他发个 Key
    r = client.post("/api/admin/billing/api-keys", headers=ADMIN,
                    json={"user_id": uid, "name": "k"})
    assert r.status_code == 200, r.text
    assert r.json()["data"]["key"].startswith("ak_")

    # 有余额时不给 force → 400
    r = client.delete(f"/api/admin/billing/users/{uid}", headers=ADMIN)
    assert r.status_code == 400, r.text
    assert "余额" in r.json()["detail"]

    # force=true → 删掉
    r = client.delete(f"/api/admin/billing/users/{uid}?force=true", headers=ADMIN)
    assert r.status_code == 200, r.text

    # 连带清理干净：Key 没了、流水没了
    keys = client.get("/api/admin/billing/api-keys", headers=ADMIN).json()["data"]["keys"]
    assert all(k["user_id"] != uid for k in keys), keys
    logs = client.get(f"/api/admin/billing/users/{uid}/logs", headers=ADMIN)
    assert logs.status_code in (200, 404)


def test_delete_user_without_balance(client):
    uid = _mk_user(client, "delclean", "0")
    r = client.delete(f"/api/admin/billing/users/{uid}", headers=ADMIN)
    assert r.status_code == 200, r.text


def test_delete_missing_user_404(client):
    r = client.delete("/api/admin/billing/users/999999", headers=ADMIN)
    assert r.status_code == 404, r.text


def test_duplicate_username_rejected(client):
    _mk_user(client, "dupuser", "0")
    r = client.post("/api/admin/billing/users", headers=ADMIN,
                     json={"username": "dupuser", "password": "pw"})
    assert r.status_code == 400, r.text
    assert "已存在" in r.json()["detail"]


def test_adjust_balance_zero_rejected(client):
    uid = _mk_user(client, "zerouser", "0")
    r = client.post(f"/api/admin/billing/users/{uid}/balance", headers=ADMIN,
                    json={"amount": 0, "remark": "x"})
    assert r.status_code == 400, r.text


def test_recharge_amount_two_decimal_limit(client):
    """充值金额最多 2 位小数 —— 3 位小数要拒绝。"""
    from app.database import SessionLocal
    from app.services import payment_service, settings_service

    uid = _mk_user(client, "decuser", "0")
    db = SessionLocal()
    try:
        # 配上网关（否则先被"未配置网关"挡掉）
        settings_service.save_billing_config(db, {
            "epay_url": "https://pay.example.com",
            "epay_pid": "1001",
            "epay_key": "secret",
        })

        # 3 位小数（厘不是 10 的倍数）→ 拒绝
        with pytest.raises(payment_service.PaymentError) as ei:
            payment_service.create_recharge_order(db, uid, 1234, "alipay", "127.0.0.1")
        assert "2 位小数" in str(ei.value)

        # 2 位小数（厘是 10 的倍数）→ 通过
        order = payment_service.create_recharge_order(db, uid, 1000, "alipay", "127.0.0.1")
        assert order["trade_no"]
        assert order["url"].endswith("/submit.php")
        assert order["params"]["money"] == "1.000"
        assert order["params"]["out_trade_no"] == order["trade_no"]

        # 越界金额也要拦
        with pytest.raises(payment_service.PaymentError) as ei2:
            payment_service.create_recharge_order(db, uid, 100, "alipay", "127.0.0.1")
        assert "不能低于" in str(ei2.value)

        with pytest.raises(payment_service.PaymentError) as ei3:
            payment_service.create_recharge_order(db, uid, 10_000_000, "alipay", "127.0.0.1")
        assert "不能超过" in str(ei3.value)

        # 非法支付方式
        with pytest.raises(payment_service.PaymentError) as ei4:
            payment_service.create_recharge_order(db, uid, 1000, "paypal", "127.0.0.1")
        assert "支付方式" in str(ei4.value)
    finally:
        db.close()
