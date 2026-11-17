# -*- coding: utf-8 -*-
"""MailPool API 端点回归测试。

改造说明（2026-11-18）：
    原来是「脚本」—— import 时就 exit()，pytest 收集会 INTERNALERROR；
    而且 BASE 写死 127.0.0.1:18080，PATCH 真实账号标签、还触发 995 个号的全量刷新
    —— 直接打到生产库上，非常危险。

    现在改成正经的 pytest 用例：
      * 自己起 uvicorn 子进程 + **独立临时数据库**，不碰生产库；
      * 种子数据走应用自己的导入 API 塞进去（别手写 INSERT，
        email_accounts 有一堆 NOT NULL 列，漏一个就被静默吞掉）；
      * 用例只读或只改自己的种子数据，不触发真实收信、不刷生产号池。

    跑法：python -m pytest tests/api/test_mailpool_full.py -q
"""
from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

ADMIN = "adminpass"
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def call(base: str, method: str, path: str, body=None):
    """发请求，返回 (status, json)。"""
    url = base + path
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        url, data=data, method=method,
        headers={
            "Content-Type": "application/json",
            "X-Admin-Password": ADMIN,
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            txt = resp.read().decode()
            try:
                return resp.status, json.loads(txt)
            except Exception:
                return resp.status, {"raw": txt[:300]}
    except urllib.error.HTTPError as e:
        try:
            txt = e.read().decode()
        except Exception:
            txt = ""
        try:
            return e.code, json.loads(txt)
        except Exception:
            return e.code, {"raw": txt[:300]}


def _seed(server: str, data_dir: str) -> None:
    """塞种子数据。

    账号走应用自己的导入 API（字段默认值由应用填），刷新历史直接写库
    （token_refresh_logs 没有 NOT NULL 陷阱）。
    """
    lines = "\n".join(
        f"pytest{i}@outlook.com----pw{i}----client{i}----token{i}" for i in (1, 2)
    )
    s, d = call(server, "POST", "/api/admin/emails/import",
                {"provider": "outlook", "text": lines, "remark": "pytest-seed"})
    assert s == 200, (s, d)
    data = d.get("data") or {}
    imported = data.get("imported")
    assert imported and imported >= 2, f"种子导入失败：{d}"

    import sqlite3
    from datetime import datetime

    db_path = os.path.join(data_dir, "mailpool.db")
    conn = sqlite3.connect(db_path)
    try:
        conn.execute(
            "INSERT INTO token_refresh_logs "
            "(trigger, provider, status, total, success, failed, skipped, "
            " duration, detail, created_at) VALUES (?,?,?,?,?,?,?,?,?,?)",
            ("manual", "outlook", "finished", 2, 2, 0, 0, "1.2s", "seed",
             datetime.now().isoformat(sep=" ", timespec="seconds")),
        )
        conn.commit()
    finally:
        conn.close()


@pytest.fixture(scope="module")
def server():
    """独立的后台服务（临时库 + 随机端口）。yield (base_url, data_dir)。"""
    data_dir = tempfile.mkdtemp(prefix="mailpool_full_")
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
    ready = False
    for _ in range(80):
        if proc.poll() is not None:
            out = proc.stdout.read() if proc.stdout else ""
            pytest.fail(f"服务启动失败：\n{out}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.4):
                ready = True
                break
        except OSError:
            time.sleep(0.25)
    if not ready:
        proc.terminate()
        pytest.fail("服务启动超时")

    try:
        _seed(base, data_dir)
        yield base, data_dir
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


# ── 1. 账号列表（含新字段 tags/proxy）────────────────────────


def test_emails_list_has_new_fields(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/emails?provider=outlook&limit=5")
    assert s == 200, (s, d)
    accs = (d.get("data") or {}).get("accounts")
    assert isinstance(accs, list) and accs, d
    for field in ("tags", "proxy", "remark"):
        assert field in accs[0], f"账号缺字段 {field}"


# ── 2. 标签管理 ───────────────────────────────────────────────


def test_tags_list(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/emails/tags?provider=outlook")
    assert s == 200, (s, d)
    assert isinstance(d.get("data"), list), d


def test_patch_account_tags(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/emails?provider=outlook&limit=1")
    accs = (d.get("data") or {}).get("accounts") or []
    assert accs, "没有账号可测"
    real_id = accs[0]["id"]

    s, d = call(base, "PATCH", f"/api/admin/emails/{real_id}",
                {"tags": "全面测试标签", "proxy": "http://127.0.0.1:9999",
                 "remark": "autotest"})
    assert s == 200, (s, d)
    got = (d.get("data") or {})
    assert got.get("tags") == "全面测试标签", got
    assert got.get("proxy") == "http://127.0.0.1:9999", got

    # 无效账号 → 404
    s, _ = call(base, "PATCH", "/api/admin/emails/99999", {"tags": "x"})
    assert s == 404, s


# ── 3. 导出账号 ───────────────────────────────────────────────


def test_export_accounts(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/emails/export?provider=outlook&limit=3")
    assert s == 200, (s, d)
    data = d.get("data") or {}
    lines = data.get("lines")
    assert isinstance(lines, list), d
    assert data.get("count") == len(lines), d
    for line in lines:
        parts = line.split("----")
        assert len(parts) == 4, f"导出格式不是 4 段：{line}"
        assert parts[0].endswith("@outlook.com"), line


# ── 4. 验证码日志 ─────────────────────────────────────────────


def test_codes_log(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/codes?limit=10")
    assert s == 200, (s, d)
    data = d.get("data") or {}
    assert isinstance(data.get("records"), list), d
    assert "total" in data, d


# ── 5. 刷新历史 ───────────────────────────────────────────────


def test_refresh_logs(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/emails/refresh-logs?limit=10")
    assert s == 200, (s, d)
    logs = (d.get("data") or {}).get("logs")
    assert isinstance(logs, list), d
    assert logs, "没有刷新历史（种子数据应有一条）"
    for field in ("trigger", "status", "total", "success", "failed", "skipped", "duration"):
        assert field in logs[0], logs[0]


# ── 6. 定时刷新配置 ───────────────────────────────────────────


def test_refresh_config(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/pool/refresh-config")
    assert s == 200, (s, d)
    cfg = (d.get("data") or {}).get("config") or {}
    assert "refresh_enabled" in cfg, cfg

    s, d = call(base, "PUT", "/api/admin/pool/refresh-config",
                {"refresh_enabled": "false", "refresh_hour": "4", "refresh_minute": "30"})
    assert s == 200, (s, d)


# ── 7. 概览看板 ───────────────────────────────────────────────


def test_overview_summary(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/overview/summary")
    assert s == 200, (s, d)
    data = d.get("data") or {}
    for key in ("accounts", "pool", "today", "projects", "monitoring"):
        assert key in data, list(data.keys())


def test_overview_refreshes(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/overview/refreshes")
    assert s == 200, (s, d)
    data = d.get("data") or {}
    for key in ("last", "today_count", "history_count", "config", "last_run"):
        assert key in data, list(data.keys())


# ── 8. 旧端点回归 ─────────────────────────────────────────────


def test_projects_list(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/projects")
    assert s == 200, (s, d)


def test_buy_nonexistent_project_not_500(server):
    base, _ = server
    s, d = call(base, "POST", "/api/email/temporary/buy",
                {"project_code": "__not_exist__", "count": 1})
    assert s != 500, (s, d)


def test_buy_bad_params_not_500(server):
    base, _ = server
    s, d = call(base, "POST", "/api/email/temporary/buy",
                {"project_code": "__not_exist__"})
    assert s != 500, (s, d)


def test_refresh_all_status_not_found(server):
    base, _ = server
    s, d = call(base, "GET", "/api/admin/emails/refresh-all/status?task_id=nonexist")
    assert s == 200, (s, d)
    assert (d.get("data") or {}).get("status") == "not_found", d
