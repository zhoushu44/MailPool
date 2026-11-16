"""API 鉴权：Bearer API Key（配置了 key 才强制校验）+ 管理后台口令。

安全模型（2026-11-15 加固）：
- 配置了 ``MAILPOOL_ADMIN_PASSWORD`` → 必须登录（会话 cookie）或带 X-Admin-Password。
- 未配置口令（本机自用模式）→ 仅放行来自回环地址（127.0.0.1 / ::1）的请求。
  这样既保留"本机免密"的便利，也不会把 1000+ 真实邮箱凭证暴露给局域网。
"""
from __future__ import annotations

import ipaddress
import secrets

from fastapi import Depends, Header, HTTPException, Request
from sqlalchemy.orm import Session

from ..database import get_db
from ..services import settings_service
from ..web.session import get_session

__all__ = ["require_api_key", "require_admin", "is_loopback", "client_host"]


def client_host(request: Request) -> str:
    """取真实客户端地址（只信任直连 peer，不信任可伪造的 X-Forwarded-For）。"""
    return (request.client.host if request.client else "") or ""


def is_loopback(host: str) -> bool:
    if not host:
        return False
    if host in ("localhost", "testclient"):
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def _verify_api_key(db: Session, authorization: str | None) -> bool:
    keys = settings_service.get_api_keys(db)
    if not keys:
        # 未配置任何 key => 本机自用，放行
        return True
    if not authorization:
        return False
    scheme, _, token = authorization.partition(" ")
    if scheme.lower() != "bearer" or not token.strip():
        return False
    return secrets.compare_digest(token.strip(), "") is False and token.strip() in keys


async def require_api_key(
    authorization: str | None = Header(default=None),
    db: Session = Depends(get_db),
) -> None:
    if not _verify_api_key(db, authorization):
        raise HTTPException(status_code=401, detail="无效的 API Key")


def check_admin_access(request: Request) -> bool:
    """管理后台是否放行（不抛异常，便于页面路由复用做重定向）。"""
    if get_session(request).get("admin_ok"):
        return True

    from ..config import settings

    password = request.headers.get("X-Admin-Password", "")
    if settings.admin_password and password:
        if secrets.compare_digest(password, settings.admin_password):
            return True

    if not settings.admin_password:
        # 本机自用模式：仅回环地址免密
        return is_loopback(client_host(request))

    return False


async def require_admin(request: Request) -> None:
    """管理后台口令校验（会话已登录 / header 带口令 / 未设口令时的本机访问）。"""
    if check_admin_access(request):
        return
    from ..config import settings

    if settings.admin_password:
        raise HTTPException(status_code=401, detail="需要管理员口令（X-Admin-Password 或后台登录）")
    raise HTTPException(
        status_code=403,
        detail=f"后台未设置口令时仅允许本机访问（当前来源 {client_host(request) or '未知'}）；"
               f"如需远程访问请设置 MAILPOOL_ADMIN_PASSWORD",
    )
