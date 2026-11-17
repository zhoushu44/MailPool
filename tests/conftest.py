# -*- coding: utf-8 -*-
"""pytest 全局配置。

**最重要的一件事：把所有测试的数据库隔离到临时目录。**

app.config 在 import 时就把 DATA_DIR / DB_PATH 定死了（从 MAILPOOL_DATA 读），
所以这个环境变量必须在任何 `import app.*` 之前设好 —— conftest.py 是唯一
能在所有测试模块之前跑的地方。

⚠️ 为什么统一在这里设，而不是每个测试文件自己设：
    两个测试文件各设各的会打架 —— 后设的那个覆盖先设的，
    先那个模块的保险丝（断言 DB_PATH 指向自己的临时目录）就直接炸，
    整套测试跑不起来。统一到 conftest，只有一份真相。
"""
import os
import sys
import tempfile

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

# ── 必须在 import app.* 之前 ──────────────────────────────────
# 测试库目录：整个会话共用一个，import app 时只会读一次
_TEST_DATA_DIR = tempfile.mkdtemp(prefix="mailpool_pytest_")
os.environ["MAILPOOL_DATA"] = _TEST_DATA_DIR
os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")
os.environ.setdefault("MAILPOOL_SESSION_SECRET", "test-secret-test-secret-test-secret-1234")

# 供测试模块用（不是 fixture）
TEST_DATA_DIR = _TEST_DATA_DIR


@pytest.fixture(scope="session")
def test_data_dir():
    """测试用的临时数据库目录。"""
    return _TEST_DATA_DIR


def pytest_sessionstart(session):
    """保险丝：确认 engine 没指向生产库。"""
    try:
        from app.config import DB_PATH
        from app.database import engine
    except Exception:
        # app 还没被任何用例 import —— 没问题
        return

    expected = os.path.join(_TEST_DATA_DIR, "mailpool.db").replace("\\", "/").lower()
    assert str(DB_PATH).replace("\\", "/").lower() == expected, (
        f"MAILPOOL_DATA 没生效，DB_PATH={DB_PATH}，拒绝跑测试（会污染生产库）"
    )
    assert str(engine.url).replace("\\", "/").lower().endswith(expected), (
        f"engine 指向了 {engine.url}，拒绝跑测试"
    )
