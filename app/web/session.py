"""轻量会话：用 itsdangerous 签名 cookie 代替 FastAPI SessionMiddleware。

（当前 Python 环境的 fastapi 安装被裁剪，无 middleware.sessions；
 用它的底层依赖 itsdangerous 自己实现等价的签名 cookie 会话。）

签名密钥来源优先级：
1. 环境变量 ``MAILPOOL_SESSION_SECRET``
2. ``MAILPOOL_DATA/.session_secret``（首次启动随机生成并持久化，权限 0600）
3. ``MAILPOOL_ADMIN_PASSWORD``

绝不再使用代码里写死的字面量密钥（旧版本用 "mailpool-dev-secret"，
任何人都能离线伪造 admin_ok cookie，等于后台无鉴权）。
"""
from __future__ import annotations

import logging
import os
import secrets
from datetime import timedelta
from pathlib import Path

from itsdangerous import BadSignature, SignatureExpired, URLSafeTimedSerializer
from starlette.requests import Request
from starlette.responses import Response

from ..config import DATA_DIR, settings

logger = logging.getLogger(__name__)

COOKIE_NAME = "mailpool_session"
_MAX_AGE = timedelta(days=7)


def _load_secret() -> str:
    env_secret = os.getenv("MAILPOOL_SESSION_SECRET", "").strip()
    if env_secret:
        return env_secret

    secret_file = Path(DATA_DIR) / ".session_secret"
    try:
        if secret_file.exists():
            content = secret_file.read_text(encoding="utf-8").strip()
            if len(content) >= 32:
                return content
        secret = secrets.token_urlsafe(48)
        secret_file.parent.mkdir(parents=True, exist_ok=True)
        secret_file.write_text(secret, encoding="utf-8")
        try:  # 尽量收紧权限（Windows 上 chmod 影响有限，但无副作用）
            os.chmod(secret_file, 0o600)
        except OSError:
            pass
        logger.info("已生成新的会话签名密钥: %s", secret_file)
        return secret
    except OSError as exc:
        logger.warning("无法持久化会话密钥（%s），降级为进程内随机密钥（重启后需重新登录）", exc)
        return secrets.token_urlsafe(48)


_SECRET = _load_secret()

_serializer = URLSafeTimedSerializer(_SECRET, salt="mailpool-session")


def set_session(response: Response, data: dict, max_age: timedelta = _MAX_AGE) -> None:
    token = _serializer.dumps(data)
    response.set_cookie(
        COOKIE_NAME,
        token,
        max_age=int(max_age.total_seconds()),
        httponly=True,
        samesite="lax",
    )


def clear_session(response: Response) -> None:
    response.delete_cookie(COOKIE_NAME)


def get_session(request: Request) -> dict:
    token = request.cookies.get(COOKIE_NAME)
    if not token:
        return {}
    try:
        data = _serializer.loads(token, max_age=int(_MAX_AGE.total_seconds()))
        return data if isinstance(data, dict) else {}
    except (BadSignature, SignatureExpired):
        return {}
