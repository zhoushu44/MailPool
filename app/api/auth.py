"""API 鉴权：Bearer API Key（配置了 key 才强制校验）+ 管理后台口令。

安全模型（2026-11-15 加固）：
- 配置了 ``MAILPOOL_ADMIN_PASSWORD`` → 必须登录（会话 cookie）或带 X-Admin-Password。
- 未配置口令（本机自用模式）→ 仅放行来自回环地址（127.0.0.1 / ::1）的请求。
  这样既保留"本机免密"的便利，也不会把 1000+ 真实邮箱凭证暴露给局域网。

计费改造（2026-11-18）：
- API Key 现在**绑定用户**（api_keys 表的 user_id）。
- 老部署的 key 存在 settings.api_keys（一个 JSON 数组），没有归属用户。
  这类 key 视为「匿名 key」：仍可调用，但计费按 0 处理（保持免费行为）。
"""
from __future__ import annotations

import ipaddress
import secrets

from fastapi import Depends, Header, HTTPException, Request
from sqlalchemy import select
from sqlalchemy.orm import Session

from ..database import get_db
from ..models import ApiKey, User
from ..services import settings_service
from ..web.session import get_session

__all__ = [
    "require_api_key",
    "require_admin",
    "is_loopback",
    "client_host",
    "current_user",
    "require_user",
    "require_api_key_or_user",
    "require_api_key_or_session",
    "resolve_key_user",
]


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
    """校验 Bearer Key。

    两种 key 都认：
    - settings.api_keys 里的老 key（匿名身份，不计费）
    - api_keys 表里绑了 user_id 的 key（计费）
    """
    if not authorization:
        return False
    scheme, _, token = authorization.partition(" ")
    if scheme.lower() != "bearer" or not token.strip():
        return False
    token = token.strip()

    legacy = settings_service.get_api_keys(db)
    if legacy and token in legacy:
        return True

    # 绑定用户的 key（计费版）
    row = db.scalar(select(ApiKey).where(ApiKey.key == token))
    if row is not None and row.enabled:
        return True

    # 完全没有配置任何 key → 本机自用，放行
    if not legacy and db.scalar(select(ApiKey.id).limit(1)) is None:
        return True
    return False


async def require_api_key(
    authorization: str | None = Header(default=None),
    db: Session = Depends(get_db),
) -> None:
    if not _verify_api_key(db, authorization):
        raise HTTPException(status_code=401, detail="无效的 API Key")


# ── 用户身份 ────────────────────────────────────────────────────


def resolve_key_user(db: Session, authorization: str | None) -> User | None:
    """把 Bearer Key 解析成用户。

    - api_keys 表里绑定了 user_id 的 key → 返回该用户
    - 老 key（settings.api_keys 里的裸字符串）→ 返回 None（匿名）
    - 没配任何 key → 返回 None（匿名，本机自用）
    """
    if not authorization:
        return None
    scheme, _, token = authorization.partition(" ")
    if scheme.lower() != "bearer":
        return None
    token = token.strip()
    if not token:
        return None

    row = db.scalar(select(ApiKey).where(ApiKey.key == token))
    if row is not None:
        if not row.enabled:
            return None
        user = db.get(User, row.user_id)
        if user is not None and user.status == 1:
            return user
        return None
    return None


async def current_user(
    request: Request,
    authorization: str | None = Header(default=None),
    db: Session = Depends(get_db),
) -> User | None:
    """取当前用户（可能为 None = 匿名）。

    注意：这是 **软身份**，不是硬鉴权 —— 路由自己决定要不要强制要求用户。
    匿名用户仍能调接口，但计费按 0 处理（老部署行为不变）。
    """
    user = resolve_key_user(db, authorization)
    if user is None:
        # 也支持后台会话登录（网页端操作用）
        session = get_session(request)
        uid = session.get("user_id")
        if uid:
            user = db.get(User, int(uid))
            if user is not None and user.status != 1:
                return None
    if user is not None:
        try:
            request.state.user = user
        except Exception:
            pass
    return user


async def require_user(
    request: Request,
    authorization: str | None = Header(default=None),
    db: Session = Depends(get_db),
) -> User:
    """强制要求登录用户（充值等操作必须绑定真实用户）。"""
    user = await current_user(request, authorization, db)
    if user is None:
        raise HTTPException(status_code=401, detail="需要有效的用户身份（API Key 绑定用户或网页登录）")
    return user


async def require_api_key_or_session(
    request: Request,
    authorization: str | None = Header(default=None),
    db: Session = Depends(get_db),
) -> User | None:
    """两种身份任选其一即可通过：
      - 合法 API Key（绑了用户就返回该用户）
      - 网页登录会话（客户在后台页面上操作）

    没有 API Key 又没登录 → 401。
    """
    if _verify_api_key(db, authorization):
        return await current_user(request, authorization, db)
    # 网页会话
    session = get_session(request)
    uid = session.get("user_id")
    if uid:
        user = db.get(User, int(uid))
        if user is not None and user.status == 1:
            return user
    raise HTTPException(status_code=401, detail="需要有效的用户身份（API Key 绑定用户或网页登录）")


async def require_api_key_or_user(
    request: Request,
    authorization: str | None = Header(default=None),
    db: Session = Depends(get_db),
) -> User | None:
    """只要合法 API Key 即可；绑定了用户就返回该用户，否则返回 None（匿名）。

    充值页要区分两种情况：
      - 绑定了用户 → 充值到该用户账户
      - 匿名 key（老 key）→ 只能看，不能充值
    """
    if not _verify_api_key(db, authorization):
        raise HTTPException(status_code=401, detail="无效的 API Key")
    return await current_user(request, authorization, db)


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
