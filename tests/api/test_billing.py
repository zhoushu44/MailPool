# -*- coding: utf-8 -*-
"""计费体系端到端测试。

覆盖：
  - 金额换算（整数厘）
  - 充值侧（new-api 风格）：epay 签名/验签、回调到账、回调幂等、坏签名拒绝、
    金额不符拒绝、充值卡生成/兑换/不可重复兑、管理员补单幂等
  - 扣费侧（MailNest 风格）：hold → charge → 幂等、hold → release、
    余额不足不发货、超时自动释放、流水类型核对
  - 身份与访问控制：管理员页、客户自助页、登录会话、API Key 绑定用户

数据库路径在**导入 app 包之前**通过 MAILPOOL_DATA 指向临时目录，
不碰生产 data/mailpool.db。

⚠️ 顺序很重要：app.config 在 import 时就把 DATA_DIR 定死了，
所以 MAILPOOL_DATA 必须在本文件 import app.* 之前设好，
否则测试会直接写进生产库。下面的 _assert_isolated 就是防这个的。
"""
import os
import sys
import tempfile

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

# MAILPOOL_DATA 统一在 tests/conftest.py 里设（必须在 import app.* 之前），
# 这里不再重复设 —— 两个模块各设各的会互相覆盖，保险丝会炸。
os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")
os.environ.setdefault("MAILPOOL_SESSION_SECRET", "test-secret-test-secret-test-secret-1234")

ADMIN = {"X-Admin-Password": "adminpass"}

# 仓库根下的生产库绝对不能被碰到
REPO_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
PROD_DB = os.path.join(REPO_ROOT, "data", "mailpool.db").replace("\\", "/").lower()


@pytest.fixture(scope="session", autouse=True)
def _assert_isolated():
    """保险丝：确认 engine 指向临时库，绝不是生产库。

    app.config 在 import 时就把 DATA_DIR 定死了 —— MAILPOOL_DATA 必须在
    import app.* 之前设好，否则测试会直接写进生产 data/mailpool.db。
    """
    from app.config import DB_PATH
    from app.database import engine

    db = str(DB_PATH).replace("\\", "/").lower()
    url = str(engine.url).replace("\\", "/").lower()

    # 必须落在系统临时目录里
    tmp = tempfile.gettempdir().replace("\\", "/").lower()
    assert db.startswith(tmp), f"DB_PATH={DB_PATH} 不在临时目录，拒绝跑测试"
    # 而且不能是生产库
    assert db != PROD_DB, f"DB_PATH 指向生产库 {DB_PATH}，拒绝跑测试"
    assert url.endswith(db), f"engine 指向了 {engine.url}，与 DB_PATH 不一致"


@pytest.fixture(scope="session")
def app_module():
    from app.database import SessionLocal, init_db
    from app.main import app

    init_db()
    return app, SessionLocal


@pytest.fixture()
def env(app_module):
    _, SessionLocal = app_module
    db = SessionLocal()
    try:
        yield db
    finally:
        db.close()


@pytest.fixture()
def client(app_module):
    from fastapi.testclient import TestClient

    app, _ = app_module
    with TestClient(app) as c:
        yield c


@pytest.fixture()
def billing_on(env):
    """打开计费，配好假易支付网关。"""
    from app.services import settings_service

    settings_service.save_billing_config(env, {
        "enabled": "true",
        "default_price": "0.018",
        "min_topup": "1.00",
        "max_topup": "1000.00",
        "epay_url": "https://pay.example.com",
        "epay_pid": "10001",
        "epay_key": "testsecret",
        "site_url": "http://127.0.0.1:18080",
        "hold_timeout": "1200",
    })
    env.commit()
    return env


def _uniq(prefix):
    """用户名唯一，避免测试之间撞车（同一个 session 级数据库）。"""
    import uuid

    return f"{prefix}{uuid.uuid4().hex[:8]}"


def make_user(client, username=None, password=""):
    username = username or _uniq("buyer")
    r = client.post("/api/admin/billing/users",
                    json={"username": username, "password": password}, headers=ADMIN)
    assert r.status_code == 200, r.text
    return r.json()["data"]["id"]


def make_key(client, user_id):
    r = client.post("/api/admin/billing/api-keys", json={"user_id": user_id}, headers=ADMIN)
    assert r.status_code == 200, r.text
    return r.json()["data"]["key"]


def topup(client, user_id, amount="10.00"):
    r = client.post(f"/api/admin/billing/users/{user_id}/balance",
                    json={"amount": amount, "remark": "测试充值"}, headers=ADMIN)
    assert r.status_code == 200, r.text
    return r.json()["data"]


# ── 金额换算 ────────────────────────────────────────────────────


def test_money_scale():
    from app.services.billing_service import fmt_milli, to_milli

    assert to_milli("0.018") == 18
    assert to_milli("1") == 1000
    assert to_milli("0.0005") == 1          # ROUND_HALF_UP，不是银行家舍入
    assert to_milli("0.0015") == 2
    assert fmt_milli(18) == "0.018"
    assert fmt_milli(1000) == "1.000"


# ── 充值侧 ──────────────────────────────────────────────────────


def test_recharge_create_and_notify(client, billing_on):
    from app.services import payment_service

    uid = make_user(client)
    topup(client, uid, "10.00")
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}

    r = client.get("/api/v1/recharge", headers=auth)
    assert r.status_code == 200
    assert r.json()["data"]["balance"] == "10.000"

    r = client.post("/api/v1/recharge/create",
                    json={"amount": "5.00", "pay_type": "alipay"}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    data = r.json()["data"]
    trade_no = data["trade_no"]
    assert data["url"].startswith("https://pay.example.com")
    assert len(data["params"]["sign"]) == 32

    cb = {"pid": "10001", "type": "alipay", "out_trade_no": trade_no,
          "trade_status": "TRADE_SUCCESS", "money": "5.000"}
    cb["sign"] = payment_service.sign_params(cb, "testsecret")

    r = client.post("/api/v1/recharge/notify", data=cb)
    assert r.json()["data"]["success"] is True, r.text
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "15.000"


def test_notify_is_idempotent(client, billing_on):
    from app.services import payment_service

    uid = make_user(client)
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}
    r = client.post("/api/v1/recharge/create",
                    json={"amount": "5.00", "pay_type": "alipay"}, headers=auth)
    trade_no = r.json()["data"]["trade_no"]
    cb = {"pid": "10001", "type": "alipay", "out_trade_no": trade_no,
          "trade_status": "TRADE_SUCCESS", "money": "5.000"}
    cb["sign"] = payment_service.sign_params(cb, "testsecret")

    client.post("/api/v1/recharge/notify", data=cb)
    r = client.post("/api/v1/recharge/notify", data=cb)
    assert "重复" in r.json()["msg"]
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "5.000"


def test_notify_rejects_bad_sign(client, billing_on):
    uid = make_user(client)
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}
    r = client.post("/api/v1/recharge/create",
                    json={"amount": "5.00", "pay_type": "alipay"}, headers=auth)
    trade_no = r.json()["data"]["trade_no"]
    cb = {"pid": "10001", "type": "alipay", "out_trade_no": trade_no,
          "trade_status": "TRADE_SUCCESS", "money": "5.000", "sign": "0" * 32}
    r = client.post("/api/v1/recharge/notify", data=cb)
    assert r.json().get("code") != "00000"
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "0.000"


def test_notify_rejects_amount_mismatch(client, billing_on):
    """金额校验必须排在幂等状态检查之前，否则篡改金额的重复回调能混过。"""
    from app.services import payment_service

    uid = make_user(client)
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}
    r = client.post("/api/v1/recharge/create",
                    json={"amount": "5.00", "pay_type": "alipay"}, headers=auth)
    trade_no = r.json()["data"]["trade_no"]
    cb = {"pid": "10001", "type": "alipay", "out_trade_no": trade_no,
          "trade_status": "TRADE_SUCCESS", "money": "500.000"}
    cb["sign"] = payment_service.sign_params(cb, "testsecret")
    r = client.post("/api/v1/recharge/notify", data=cb)
    assert r.json().get("code") != "00000"
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "0.000"


def test_recharge_amount_bounds(client, billing_on):
    uid = make_user(client)
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}
    for amount in ("0.5", "2000", "1.234"):
        r = client.post("/api/v1/recharge/create",
                        json={"amount": amount, "pay_type": "alipay"}, headers=auth)
        assert r.json()["code"] != "00000", (amount, r.text)


def test_redeem_card(client, billing_on):
    uid = make_user(client)
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}
    r = client.post("/api/admin/billing/cards/generate",
                    json={"amount": "5.00", "count": 2}, headers=ADMIN)
    assert r.status_code == 200
    cards = r.json()["data"]["keys"]
    assert len(cards) == 2

    r = client.post("/api/v1/recharge/redeem-card", json={"card_key": cards[0]}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "5.000"

    r = client.post("/api/v1/recharge/redeem-card", json={"card_key": cards[0]}, headers=auth)
    assert r.json()["code"] != "00000"
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "5.000"


def test_admin_complete_topup_is_idempotent(client, billing_on):
    uid = make_user(client)
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}
    r = client.post("/api/v1/recharge/create",
                    json={"amount": "5.00", "pay_type": "alipay"}, headers=auth)
    trade_no = r.json()["data"]["trade_no"]
    r = client.post("/api/admin/billing/orders/complete", json={"trade_no": trade_no}, headers=ADMIN)
    assert r.status_code == 200, r.text
    client.post("/api/admin/billing/orders/complete", json={"trade_no": trade_no}, headers=ADMIN)
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "5.000"


# ── 扣费侧 ──────────────────────────────────────────────────────


class FakeProv:
    pooled = False

    def __init__(self, cfg):
        self.cfg = cfg
        self._n = 0

    def generate_address(self):
        # 每次给不同邮箱：email_usages 全局唯一，重复会变成"库存不足"
        self._n += 1
        return f"flow{os.getpid()}{id(self)}x{self._n}@fake.test"

    async def wait_mail(self, email, timeout=30, project=None):
        return {"email": email, "code": "123456", "message_id": "m1",
                "provider": "cf_temp", "folder": "inbox", "sender": "a@b.test",
                "subject": "code", "body_preview": "123456", "body": "code 123456",
                "code_source": "auto", "received_at": None}

    def release(self, email):
        return {"status": "released"}


def _make_project(env, code):
    from app.services import mail_service

    mail_service.upsert_project(
        env, code, "计费测试", "cf_temp",
        '{"query":"b.test","mail_keywords":"code","timeout":10}', enabled=1)
    env.commit()
    return code


def test_hold_charge_release(client, env, billing_on, monkeypatch):
    """取号冻结 → 收码扣费 → 幂等，全链路。"""
    from app.models import EmailOrder
    from app.services import billing_service, mail_service

    monkeypatch.setattr(mail_service, "create_provider", lambda pid, cfg: FakeProv(cfg))
    code = _make_project(env, "flowtest001")

    uid = make_user(client)
    topup(client, uid, "1.00")
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}

    # 取号 → hold
    r = client.post("/api/v1/email/temporary/buy",
                    json={"project_code": code}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    email = r.json()["data"]["email"]
    d = client.get("/api/v1/recharge", headers=auth).json()["data"]
    assert d["balance"] == "1.000"          # 没扣
    assert d["frozen_balance"] == "0.018"   # 冻结了
    assert d["available_balance"] == "0.982"

    # 收码 → charge
    r = client.post("/api/v1/email/receive",
                    json={"project_code": code, "email": email, "timeout": 5}, headers=auth)
    assert r.json()["data"]["status"] == "success", r.text
    d = client.get("/api/v1/recharge", headers=auth).json()["data"]
    assert d["balance"] == "0.982"
    assert d["frozen_balance"] == "0.000"

    # 幂等：再收一次不重复扣
    client.post("/api/v1/email/receive",
                json={"project_code": code, "email": email, "timeout": 5}, headers=auth)
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "0.982"

    order = env.query(EmailOrder).filter(EmailOrder.email == email).one()
    assert order.billing_status == "charged"

    # 余额不足 → D0001，且不发号
    billing_service.debit(env, uid, 982, "adjust", "清零", require_available=False)
    env.commit()
    r = client.post("/api/v1/email/temporary/buy",
                    json={"project_code": code}, headers=auth)
    assert r.json()["code"] == "D0001", r.text
    assert r.json().get("data") is None


def test_release_unfreezes_only(client, env, billing_on, monkeypatch):
    from app.services import mail_service

    monkeypatch.setattr(mail_service, "create_provider", lambda pid, cfg: FakeProv(cfg))
    code = _make_project(env, "reltest001")

    uid = make_user(client)
    topup(client, uid, "1.00")
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}

    r = client.post("/api/v1/email/temporary/buy",
                    json={"project_code": code}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    email = r.json()["data"]["email"]
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["frozen_balance"] == "0.018"
    r = client.post("/api/v1/email/release",
                    json={"project_code": code, "email": email}, headers=auth)
    assert r.json()["code"] == "00000", r.text
    d = client.get("/api/v1/recharge", headers=auth).json()["data"]
    assert d["frozen_balance"] == "0.000"
    assert d["balance"] == "1.000"      # 解冻不扣钱


def test_release_expired(client, env):
    """超时自动释放：冻结额解掉，余额不变。"""
    from app.services import billing_service

    uid = make_user(client)
    topup(client, uid, "1.00")
    billing_service.hold(env, user_id=uid, email="exp@fake.test",
                         project_code="p", provider="cf_temp", price=18)
    env.commit()
    user = billing_service.get_user(env, uid)
    assert user.frozen_balance == 18

    count = billing_service.release_expired(env, timeout_seconds=0)
    assert count == 1
    env.refresh(user)
    assert user.frozen_balance == 0
    assert user.balance == 1000


def test_balance_log_types(client, env, billing_on):
    from app.services import billing_service

    uid = make_user(client)
    topup(client, uid, "10.00")
    billing_service.hold(env, user_id=uid, email="l@fake.test",
                         project_code="p", provider="cf_temp", price=18)
    billing_service.charge(env, user_id=uid, email="l@fake.test")
    # release 作用于一个真实存在的 pending 订单
    billing_service.hold(env, user_id=uid, email="r@fake.test",
                         project_code="p", provider="cf_temp", price=18)
    billing_service.release(env, user_id=uid, email="r@fake.test")
    env.commit()
    logs, _ = billing_service.list_balance_logs(env, uid, limit=50)
    types = {l.log_type for l in logs}
    assert {"adjust", "hold", "charge", "release"} <= types, types


def test_billing_disabled_keeps_free_behavior(client, env):
    """计费关闭（默认）→ 免费发放，price 为空串，余额不动。"""
    from app.services import settings_service

    settings_service.save_billing_config(env, {"enabled": "false"})
    env.commit()
    uid = make_user(client)
    topup(client, uid, "1.00")
    key = make_key(client, uid)
    auth = {"Authorization": f"Bearer {key}"}
    r = client.get("/api/product/info")
    tmp = r.json()["data"]["temporary"]
    if tmp:
        assert tmp[0]["price"] == ""
    assert client.get("/api/v1/recharge", headers=auth).json()["data"]["balance"] == "1.000"


# ── 身份与访问控制 ──────────────────────────────────────────────


def test_api_key_must_be_valid(client):
    r = client.get("/api/v1/recharge", headers={"Authorization": "Bearer ak_wrong"})
    assert r.status_code == 401


def test_customer_login_and_scoped_pages(client, env):
    name = _uniq("alice")
    make_user(client, name, "alicepass")
    r = client.get("/recharges", follow_redirects=False)
    assert r.status_code == 303
    assert r.headers["location"] == "/user/login"

    r = client.post("/user/login", data={"username": name, "password": "wrong"},
                    follow_redirects=False)
    assert r.status_code == 200 and "错误" in r.text

    r = client.post("/user/login", data={"username": name, "password": "alicepass"},
                    follow_redirects=False)
    assert r.status_code == 303
    assert client.get("/recharges").status_code == 200
    assert client.get("/balances").status_code == 200

    # 客户不能进管理页
    assert client.get("/billing/users", follow_redirects=False).status_code == 303
    assert client.get("/billing/cards", follow_redirects=False).status_code == 303

    # 管理员口令能进
    assert client.get("/billing/users", headers=ADMIN).status_code == 200
    assert client.get("/billing/cards", headers=ADMIN).status_code == 200


def test_admin_login_does_not_clobber_customer_session(client, env):
    name = _uniq("alice")
    make_user(client, name, "alicepass")
    client.post("/user/login", data={"username": name, "password": "alicepass"},
                follow_redirects=False)
    r = client.post("/login", data={"password": "adminpass"}, follow_redirects=False)
    assert r.status_code == 303
    # 管理员 + 客户双身份共存
    assert client.get("/billing/users").status_code == 200
    assert client.get("/recharges").status_code == 200
