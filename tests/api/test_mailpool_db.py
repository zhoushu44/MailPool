# -*- coding: utf-8 -*-
"""MailPool 数据库结构验证：迁移、刷新历史、标签/代理字段落地。

改造说明（2026-11-18）：
    原来是「脚本」—— import 时就直连**生产库** data/mailpool.db 并 exit()，
    pytest 收集到它会 INTERNALERROR，整套测试跑不起来。
    现在改成正经的 pytest 用例：
      * 只读，且优先读临时库（MAILPOOL_DATA），没设置就退回生产库只读；
      * 用 tmp_path 复制一份生产库来查，绝不写生产库；
      * 不再 exit()，失败用 assert 报告。

    单独跑：python -m pytest tests/api/test_mailpool_db.py -q
"""
from __future__ import annotations

import os
import shutil
import sqlite3
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
# 生产库位置（相对仓库根，不受 MAILPOOL_DATA 影响 —— 别的测试模块会在 import 期
# 改这个环境变量，用 app.config.DB_PATH 会读到别人的临时库）。
PROD_DB = REPO_ROOT / "data" / "mailpool.db"


@pytest.fixture(scope="module")
def db_path(tmp_path_factory) -> Path:
    """拿一份可读的数据库副本。

    优先用显式指定的 MAILPOOL_TEST_DB；否则复制生产库到临时目录。
    全程只读，绝不碰生产库本体。
    """
    explicit = os.getenv("MAILPOOL_TEST_DB", "").strip()
    if explicit:
        src = Path(explicit)
        if src.exists():
            return src
    if PROD_DB.exists():
        dst = tmp_path_factory.mktemp("mpdb") / "mailpool.db"
        shutil.copy2(PROD_DB, dst)
        return dst
    pytest.skip("找不到数据库文件（既没有 MAILPOOL_TEST_DB 库，也没有生产库）")


@pytest.fixture(scope="module")
def conn(db_path):
    c = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    yield c
    c.close()


def test_email_accounts_columns(conn):
    cols = [r[1] for r in conn.execute("PRAGMA table_info(email_accounts)")]
    for field in ("tags", "proxy", "remark"):
        assert field in cols, f"email_accounts 缺列 {field}，实际列：{cols}"


def test_token_refresh_logs_table(conn):
    tabs = [r[0] for r in conn.execute(
        "SELECT name FROM sqlite_master WHERE type='table'")]
    assert "token_refresh_logs" in tabs, f"缺 token_refresh_logs 表，现有：{tabs}"


def test_token_refresh_logs_columns(conn):
    lcols = [r[1] for r in conn.execute("PRAGMA table_info(token_refresh_logs)")]
    for f in ("id", "trigger", "provider", "status", "total", "success",
              "failed", "skipped", "duration", "detail", "created_at"):
        assert f in lcols, f"token_refresh_logs 缺字段 {f}，实际：{lcols}"


def test_token_refresh_logs_content(conn):
    logs = list(conn.execute(
        "SELECT id, trigger, status, total, success, failed, skipped, duration "
        "FROM token_refresh_logs ORDER BY id DESC LIMIT 5"))
    assert logs, "没有任何刷新历史记录"
    recent = logs[0]
    assert recent[2] == "finished", f"最新记录 status={recent[2]}，期望 finished"
    for i, name in ((3, "total"), (4, "success"), (5, "failed"), (6, "skipped")):
        assert isinstance(recent[i], int) and recent[i] >= 0, (
            f"{name} 不是非负整数：{recent[i]!r}"
        )


def test_account_tags_proxy_readable(conn):
    rows = list(conn.execute(
        "SELECT email, tags, proxy FROM email_accounts "
        "WHERE tags != '' OR proxy != '' LIMIT 5"))
    for email, tags, proxy in rows:
        assert isinstance(tags, str), f"{email} 的 tags 不是字符串"
        assert isinstance(proxy, str), f"{email} 的 proxy 不是字符串"


def test_account_count_positive(conn):
    n = conn.execute("SELECT COUNT(*) FROM email_accounts").fetchone()[0]
    assert n > 0, "email_accounts 一条都没有"
    n_h = conn.execute(
        "SELECT COUNT(*) FROM email_accounts WHERE health_status='healthy'"
    ).fetchone()[0]
    print(f"\n  INFO  总账号 {n} / 健康 {n_h}")
