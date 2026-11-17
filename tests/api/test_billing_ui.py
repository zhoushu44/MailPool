# -*- coding: utf-8 -*-
"""后台 UI 交互测试（Playwright）。

自己起一个 uvicorn 子进程（**独立临时数据库**），不碰生产 data/mailpool.db。

跑法：
    python -m pytest tests/api/test_billing_ui.py -q

需要 playwright + chromium。测的是真页面：建用户、签 Key、调余额、
查流水、生成充值卡、查卡密、保存配置、客户登录、客户兑换充值卡。
"""
import os
import re
import socket
import subprocess
import sys
import tempfile
import time

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

playwright_api = pytest.importorskip("playwright.sync_api")

ADMIN = "adminpass"
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@pytest.fixture(scope="module")
def server():
    """起一个独立的后台服务，用临时数据库。"""
    data_dir = tempfile.mkdtemp(prefix="mailpool_billing_ui_")
    port = _free_port()
    env = dict(os.environ)
    env["MAILPOOL_DATA"] = data_dir
    env["MAILPOOL_ADMIN_PASSWORD"] = ADMIN
    env["MAILPOOL_SESSION_SECRET"] = "test-secret-test-secret-test-secret-1234"
    env["MAILPOOL_PORT"] = str(port)
    env["PYTHONIOENCODING"] = "utf-8"

    proc = subprocess.Popen(
        [sys.executable, "-m", "uvicorn", "app.main:app",
         "--host", "127.0.0.1", "--port", str(port), "--log-level", "warning"],
        cwd=REPO, env=env,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, errors="replace",
    )
    base = f"http://127.0.0.1:{port}"
    for _ in range(80):
        if proc.poll() is not None:
            out = proc.stdout.read() if proc.stdout else ""
            pytest.fail(f"服务启动失败：\n{out}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.4):
                break
        except OSError:
            time.sleep(0.25)
    else:
        proc.terminate()
        pytest.fail("服务启动超时")

    try:
        yield base
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


@pytest.fixture(scope="module")
def browser():
    with playwright_api.sync_playwright() as p:
        b = p.chromium.launch()
        yield b
        b.close()


@pytest.fixture(scope="module")
def admin_page(browser, server):
    ctx = browser.new_context(viewport={"width": 1440, "height": 1000})
    ctx.set_extra_http_headers({"X-Admin-Password": ADMIN})
    page = ctx.new_page()
    page.goto(f"{server}/login", wait_until="networkidle")
    yield page
    ctx.close()


def _wait(page, ms=1200):
    page.wait_for_timeout(ms)


def test_users_page_full_flow(admin_page, server):
    page = admin_page
    uname = "uitest" + os.urandom(3).hex()

    page.goto(f"{server}/billing/users", wait_until="networkidle")
    page.fill("#mn-new-username", uname)
    page.fill("#mn-new-password", "uipass")
    page.click("button:has-text('创建')")
    _wait(page)
    assert uname in page.inner_text("body")

    # 拿用户 ID
    page.reload(wait_until="networkidle")
    row = page.locator(f"tr:has-text('{uname}')").first
    uid = row.locator("td").first.inner_text().strip()
    assert uid.isdigit()

    # 签发 API Key
    page.fill("#mn-key-user", uid)
    page.fill("#mn-key-name", "ui-test")
    page.click("button:has-text('签发 Key')")
    _wait(page)
    out = page.inner_text("#mn-key-output")
    assert "ak_" in out

    # 调整余额（confirm/prompt 弹窗）
    admin_page.on("dialog", lambda d: d.accept("12.50"))
    row = page.locator(f"tr:has-text('{uname}')").first
    row.locator("button:has-text('调整余额')").click()
    _wait(page)
    assert "12.500" in page.inner_text("body")

    # 看流水
    row = page.locator(f"tr:has-text('{uname}')").first
    row.locator("button:has-text('流水')").click()
    _wait(page, 1000)
    assert "adjust" in page.inner_text("#mn-user-logs")


def test_cards_page_generate_and_query(admin_page, server):
    page = admin_page
    page.goto(f"{server}/billing/cards", wait_until="networkidle")
    page.fill("#mn-card-amount", "8.00")
    page.fill("#mn-card-count", "3")
    page.click("button:has-text('生成')")
    _wait(page)

    # 卡密在 textarea 的 value 里
    ta = page.locator("#mn-card-output textarea")
    val = ta.input_value()
    assert val.count("MP8000-") == 3, val

    m = re.search(r"MP8000-[A-Z0-9]+-[A-Z0-9]+", val)
    assert m, val
    key = m.group(0)

    page.fill("#mn-card-query", key)
    page.click("button:has-text('查询')")
    _wait(page, 1000)
    q = page.inner_text("#mn-card-query-out")
    assert key in q
    assert "未使用" in q

    # 保存计费配置
    page.select_option("#mn-cfg-enabled", "true")
    page.fill("#mn-cfg-price", "0.025")
    page.click("button:has-text('保存')")
    _wait(page, 1500)
    page.reload(wait_until="networkidle")
    assert page.input_value("#mn-cfg-price") == "0.025"


def test_customer_login_and_redeem(browser, admin_page, server):
    # 拿一个卡密
    page = admin_page
    page.goto(f"{server}/billing/cards", wait_until="networkidle")
    page.fill("#mn-card-amount", "8.00")
    page.fill("#mn-card-count", "1")
    page.click("button:has-text('生成')")
    _wait(page)
    val = page.locator("#mn-card-output textarea").input_value()
    key = re.search(r"MP8000-[A-Z0-9]+-[A-Z0-9]+", val).group(0)

    uname = "uicust" + os.urandom(3).hex()
    ctx = browser.new_context(viewport={"width": 1440, "height": 1000})
    p2 = ctx.new_page()
    p2.goto(f"{server}/user/login", wait_until="networkidle")
    p2.fill("input[name=username]", uname)
    p2.fill("input[name=password]", "custpass")
    p2.click("button[type=submit]")

    # 没这用户 → 应留在登录页并报错
    p2.wait_for_timeout(800)
    assert "/user/login" in p2.url

    # 管理员建这个用户并给余额
    admin_page.goto(f"{server}/billing/users", wait_until="networkidle")
    admin_page.fill("#mn-new-username", uname)
    admin_page.fill("#mn-new-password", "custpass")
    admin_page.click("button:has-text('创建')")
    _wait(admin_page)

    # 客户登录
    p2.goto(f"{server}/user/login", wait_until="networkidle")
    p2.fill("input[name=username]", uname)
    p2.fill("input[name=password]", "custpass")
    p2.click("button[type=submit]")
    p2.wait_for_timeout(1200)
    assert "/recharges" in p2.url, p2.url

    # 兑换充值卡 → 余额 8.000
    p2.fill("#mn-card-key", key)
    p2.click("button:has-text('兑换')")
    p2.wait_for_timeout(1800)
    body = p2.inner_text("body")
    assert "8.000" in body, body[:300]
    # 分栏正确：充值卡 8.000，扫码充值 0.000
    flat = re.sub(r"\s+", "", body)
    assert "充值卡充值8.000" in flat, body[:400]
    assert "扫码充值0.000" in flat, body[:400]

    ctx.close()


def test_customer_cannot_see_admin_pages(browser, server):
    ctx = browser.new_context()
    p = ctx.new_page()
    r = p.goto(f"{server}/billing/users", wait_until="networkidle")
    assert "/login" in p.url or "/user/login" in p.url, p.url
    ctx.close()
