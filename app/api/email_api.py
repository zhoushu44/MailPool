"""对外邮箱 API（MailNest 兼容格式）：产品信息 / 取号 / 取码 / 释放 / 邮件查询 / 健康检查。

- 鉴权：`Authorization: Bearer <API_KEY>`（除 `/api/product/info` 外均需鉴权）
- 响应格式：`{"code": "00000", "msg": "", "type": "", "data": ...}`
- 业务码：00000 成功；D0001 余额不足；D0002 邮箱库存不足；D0003 项目不存在或已停用；
  D0004 当前无法对此邮箱执行该操作；D0005 取件失败 请稍后再试；D0006 账号被封禁 取件失败；
  99999 系统异常
"""
from __future__ import annotations

import json
import logging
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query, Request
from pydantic import BaseModel
from sqlalchemy import func, select
from sqlalchemy.orm import Session

from ..config import project_logo_src
from ..database import get_db
from ..models import EmailOrder, MailRecord
from ..providers import ProviderError
from ..services import billing_service, mail_service, pool_service
from ..timeutil import utc_iso
from .auth import current_user, require_api_key_or_session

logger = logging.getLogger(__name__)

# ── 业务状态码（对齐 MailNest）──────────────────────────────
CODE_OK = "00000"         # 请求成功
CODE_BALANCE = "D0001"    # 余额不足
CODE_NO_STOCK = "D0002"   # 邮箱库存不足
CODE_NO_PROJECT = "D0003" # 项目不存在或已停用
CODE_OP_FAILED = "D0004"  # 当前无法对此邮箱执行该操作
CODE_FETCH_FAILED = "D0005"  # 取件失败 请稍后再试
CODE_BANNED = "D0006"     # 账号被封禁 取件失败
CODE_PAYMENT = "D0007"    # 充值/支付流程错误
CODE_SYSTEM = "99999"     # 系统异常


def ok(data: Any = None) -> dict[str, Any]:
    return {"code": CODE_OK, "msg": "", "type": "", "data": data}


def err(code: str, msg: str) -> dict[str, Any]:
    return {"code": code, "msg": msg, "type": "error", "data": None}


def _owns_email(db: Session, request: Any, user: Any, email: str, project_code: str) -> bool:
    """当前请求者能否操作该邮箱。

    - 管理员（会话/口令头）：任意邮箱
    - 匿名 key（老 key/本机自用，user=None）：放行（保持旧集成行为）
    - 绑定用户的 key / 网页客户会话：必须是该用户名下的取号订单邮箱
      （防止横向收别人的验证码）
    - 计费关闭时不建订单（user_id=0），没有归属数据可查 → 放行
    """
    from .auth import check_admin_access

    if check_admin_access(request):
        return True
    if user is None or not billing_service.is_billing_enabled(db):
        return True
    email = (email or "").strip().lower()
    return (
        db.scalar(
            select(EmailOrder.id).where(
                EmailOrder.user_id == user.id,
                EmailOrder.email == email,
                EmailOrder.project_code == project_code,
            )
        )
        is not None
    )


def _map_provider_error(exc: ProviderError) -> dict[str, Any]:
    """ProviderError → 业务状态码。

    软硬错误必须按 **业务码** 区分，不能看文案：
    - D0006 账号被封禁（凭证失效，号废了）→ 客户端应丢弃这个号
    - D0005 取件失败（临时性：网络抖动 / 邮箱服务繁忙）→ 可重试
    两者的中文文案都含"取件失败"，只看文案无法区分。
    """
    kind = str(getattr(exc, "kind", "") or "").lower()
    if exc.fatal or kind in ("credential", "banned"):
        return err(CODE_BANNED, "账号被封禁 取件失败")
    if kind in ("transient", "network", "fetch"):
        return err(CODE_FETCH_FAILED, "取件失败 请稍后再试")
    return err(CODE_OP_FAILED, str(exc) or "当前无法对此邮箱执行该操作")


# /api/product/info 无需鉴权（对齐 MailNest），其余走 /api/v1 + Bearer Key
public_router = APIRouter(prefix="/api")
# 鉴权：API Key 或网页客户会话任一即可（对齐 MailNest：客户页与第三方共用端点）。
# require_api_key_or_session 对纯 key 调用的行为与原 require_api_key 完全等价。
router = APIRouter(prefix="/api/v1", dependencies=[Depends(require_api_key_or_session)])


class BuyBody(BaseModel):
    project_code: str
    count: int = 1


class ReceiveBody(BaseModel):
    project_code: str
    email: str
    timeout: float | None = None
    skip_codes: list[str] | None = None


class ReleaseBody(BaseModel):
    project_code: str
    email: str


@public_router.get("/product/info")
def product_info(db: Session = Depends(get_db)) -> dict[str, Any]:
    """查询可用的项目（临时邮箱）列表，无需 API Key。

    与 MailNest 对齐：`{"temporary": [...], "exclusive": {...}}`。
    计费开启时 price 给出真实单价（元），关闭时为空串（免费）。
    """
    temporary = []
    stock_cache: dict[tuple[str, str], int] = {}  # (provider, project_code) → 库存
    # 按项目一次性：每个项目排除自己用过的邮箱，库存各不相同
    billing_on = billing_service.is_billing_enabled(db)
    for p in mail_service.list_projects(db):
        if not p.enabled:
            continue
        try:
            _, merged = mail_service.build_provider(db, p)
            prov, _ = mail_service.build_provider(db, p)
        except Exception:
            merged, prov = {}, None
        pooled = bool(getattr(prov, "pooled", False)) if prov else False
        if pooled:
            ck = (p.provider, p.code)
            if ck not in stock_cache:
                stock_cache[ck] = pool_service.count_claimable(db, p.provider, p.code)
            stock = stock_cache[ck]
        else:
            stock = None
        cfg = json.loads(p.config_json or "{}") if isinstance(p.config_json, str) else {}
        icon = project_logo_src(cfg.get("logo", "") if isinstance(cfg, dict) else "") or None
        price_milli = billing_service.project_price(db, p) if billing_on else 0
        original_milli = billing_service.to_milli(cfg.get("original_price")) if billing_on and cfg.get("original_price") else 0
        temporary.append({
            "code": p.code,
            "name": p.name,
            "query": (cfg.get("query", "") if isinstance(cfg, dict) else ""),
            "icon": icon,
            "stock": stock,
            "price": billing_service.fmt_milli(price_milli) if billing_on else "",
            "original_price": billing_service.fmt_milli(original_milli) if billing_on else "",
            "sender_domains": (cfg.get("sender_domains", "") if isinstance(cfg, dict) else ""),
            "mail_keywords": (cfg.get("mail_keywords", "") if isinstance(cfg, dict) else ""),
            "duration_seconds": None,
        })
    return ok({
        "temporary": temporary,
        "exclusive": {"stock": None, "price": "", "original_price": ""},
    })


def _normalize_buy_item(item: dict[str, Any]) -> dict[str, Any]:
    """统一取号返回的时间字段格式。

    踩过的坑：同一部署先后给出 `acquired_at` 带 +08:00 和带 +00:00 两种情况，
    调用方换算窗口时会偏 8 小时。这里统一输出 **带时区的 ISO8601（UTC）**。
    """
    out = dict(item or {})
    for key in ("acquired_at", "created_at"):
        if key in out:
            out[key] = utc_iso(out.get(key))
    return out


@router.post("/email/temporary/buy")
def buy_email(
    body: BuyBody,
    db: Session = Depends(get_db),
    user=Depends(current_user),
) -> dict[str, Any]:
    """按 project_code 取邮箱（临时源新建 / 固定池 claim）。

    count 支持 1-100，批量取号时返回数组。

    计费：计费开启且 key 绑定了用户时，取号前先冻结（hold）。
    余额不足 → 直接返回 D0001，**不会**把号发出去。
    """
    if body.count < 1 or body.count > 100:
        return err(CODE_OP_FAILED, "count 取值范围 1-100")
    project = mail_service.get_project(db, body.project_code.strip())
    if not project:
        return err(CODE_NO_PROJECT, "项目不存在或已停用")

    billing_on = billing_service.is_billing_enabled(db)
    price = billing_service.project_price(db, project) if billing_on else 0
    user_id = user.id if (user is not None and billing_on) else 0

    # 余额预检（批量时按单价 × count 估）
    if billing_on and user is not None and price > 0:
        need = price * body.count
        if billing_service.available_balance(user) < need:
            return err(
                CODE_BALANCE,
                f"余额不足，需要 {billing_service.fmt_milli(need)} 元，"
                f"可用 {billing_service.fmt_milli(billing_service.available_balance(user))} 元",
            )

    items = []
    failure: dict[str, Any] | None = None
    for _ in range(body.count):
        try:
            item = mail_service.acquire_email(db, project.code)
            items.append(_normalize_buy_item(item))
        except ProviderError as exc:
            if getattr(exc, "kind", "") == "no_stock" or any(
                k in str(exc) for k in ("无可用账号", "库存", "占用")
            ):
                failure = {"code": CODE_NO_STOCK, "message": "邮箱库存不足"}
            else:
                mapped = _map_provider_error(exc)
                failure = {"code": mapped["code"], "message": mapped["msg"]}
            break

    if items and billing_on and user is not None:
        # 取号成功后再冻结（每号一条订单）
        charged: list[dict[str, Any]] = []
        for item in items:
            try:
                billing_service.hold(
                    db,
                    user_id=user.id,
                    email=item["email"],
                    project_code=project.code,
                    provider=project.provider,
                    price=price,
                )
                charged.append(item)
            except billing_service.InsufficientBalance as exc:
                failure = {"code": CODE_BALANCE, "message": str(exc)}
                break
            except Exception as exc:
                logger.warning("冻结失败 email=%s: %s", item.get("email"), exc)
        items = charged
        # 必须提交：request 作用域的 session 关闭时会回滚未提交的改动，
        # 冻结记录和流水会一起丢掉（余额看着没动）。
        db.commit()

    if failure and not items:
        return err(failure["code"], failure["message"])
    if failure:
        return ok({
            "status": "partial",
            "requested": body.count,
            "succeeded": len(items),
            "failed": body.count - len(items),
            "items": items,
            "failure": failure,
        })
    return ok(items if body.count > 1 else items[0])


@router.post("/email/receive")
def receive_code(
    body: ReceiveBody,
    request: Request,
    db: Session = Depends(get_db),
    user=Depends(current_user),
) -> dict[str, Any]:
    """轮询等待验证码。

    返回约定（**重要**）：
    - 等到验证码：code=00000，data.status="success"，data.code=验证码
    - 没等到（空箱 / 对方还没发信）：code=00000，data.status="timeout"，data.code=""
      → 这是**正常轮询结果，不是错误**。调用方不应抛异常中断注册流程，
        而应稍后再次调用 receive（邮箱仍然占用着，可以继续等）。
    - 账号凭证失效：code=D0006（硬错误，该号作废，应丢弃换号）
    - 临时性故障（网络 / 上游抖动）：code=D0005（软错误，可重试同一封邮箱）

    data.elapsed 是本次实际耗时；它可能比 timeout 大几秒（最多约 +20s），
    因为要等当前这一轮 Graph/IMAP 轮询走完。客户端读超时请按 timeout + 30s 设置。

    计费：收到验证码的瞬间真扣费（charge）——冻结转实扣。
    """
    project = mail_service.get_project(db, body.project_code.strip())
    if not project:
        return err(CODE_NO_PROJECT, "项目不存在或已停用")
    if not _owns_email(db, request, user, body.email.strip(), project.code):
        return err(CODE_OP_FAILED, "当前无法对此邮箱执行该操作")
    try:
        result = mail_service.receive_code(
            db,
            project.code,
            body.email.strip(),
            timeout=body.timeout,
            skip_codes=body.skip_codes,
        )
    except ProviderError as exc:
        mapped = _map_provider_error(exc)
        return err(mapped["code"], mapped["msg"])

    # 收到验证码 → 真扣费
    if result.get("status") == "success" and user is not None and billing_service.is_billing_enabled(db):
        try:
            billing_service.charge(
                db, user.id, body.email.strip(),
                remark=f"收码扣费 {body.email.strip()}",
                project_code=project.code,
            )
            db.commit()
        except Exception as exc:
            logger.warning("扣费失败 email=%s: %s", body.email, exc)
    return ok(result)


@router.post("/email/release")
def release_email(
    body: ReleaseBody,
    request: Request,
    db: Session = Depends(get_db),
    user=Depends(current_user),
) -> dict[str, Any]:
    """释放邮箱（临时源退还 / 固定池回池）。计费：解冻，不扣钱。"""
    project = mail_service.get_project(db, body.project_code.strip())
    if not project:
        return err(CODE_NO_PROJECT, "项目不存在或已停用")
    if not _owns_email(db, request, user, body.email.strip(), project.code):
        return err(CODE_OP_FAILED, "当前无法对此邮箱执行该操作")
    try:
        result = mail_service.release_email(db, project.code, body.email.strip())
    except ProviderError as exc:
        return _map_provider_error(exc)
    if result.get("status") == "not_found":
        return err(CODE_OP_FAILED, "当前无法对此邮箱执行该操作")

    if user is not None and billing_service.is_billing_enabled(db):
        try:
            billing_service.release(
                db, user.id, body.email.strip(),
                remark=f"释放解冻 {body.email.strip()}",
                project_code=project.code,
            )
            db.commit()
        except Exception as exc:
            logger.warning("解冻失败 email=%s: %s", body.email, exc)

    pool_service.record_operation("release_count")
    return ok(result)


@router.get("/mails")
def query_mails(
    request: Request,
    email: str | None = Query(default=None),
    project_code: str | None = Query(default=None),
    page: int = Query(default=1, ge=1),
    page_size: int = Query(default=20, ge=1, le=500),
    db: Session = Depends(get_db),
    user=Depends(require_api_key_or_session),
) -> dict[str, Any]:
    """查询收件记录（按邮箱或项目过滤，分页返回）。

    数据边界：
      - 管理员（会话 / X-Admin-Password 头 / 未设口令的本机）→ 全量
      - 绑定用户的 API Key 或网页客户会话 → 只能看自己订单里的邮箱
      - 匿名 key（settings 里的老 key，未绑定用户）→ 全量
        （老 key 是运营方自用的服务端凭据，语义等同管理员；
         真正需要隔离的是"客户"身份，即绑了 user_id 的 key）
    """
    from .auth import check_admin_access

    scope_emails: set[str] | None = None  # None = 不限制
    if not check_admin_access(request) and user is not None:
        # 只有「明确的客户身份」才收窄，匿名 key 保持全量（兼容老集成）
        scope_emails = set(
            db.scalars(select(EmailOrder.email).where(EmailOrder.user_id == user.id))
        )
        if email and email.lower() not in scope_emails:
            return ok({"total": 0, "page": page, "page_size": page_size, "items": []})

    stmt = select(func.count()).select_from(MailRecord)
    if email:
        stmt = stmt.where(MailRecord.email == email.lower())
    if project_code:
        stmt = stmt.where(MailRecord.project_code == project_code)
    if scope_emails is not None:
        if not scope_emails:
            return ok({"total": 0, "page": page, "page_size": page_size, "items": []})
        stmt = stmt.where(MailRecord.email.in_(scope_emails))
    total = db.scalar(stmt) or 0

    # 客户数据边界：emails 白名单过滤。指定 email 时（已在白名单内验证过）
    # 缩窄为单邮箱，避免白名单吞掉 email 参数。
    if scope_emails is not None and email:
        emails_arg: list[str] | None = [email.lower()]
    elif scope_emails is not None:
        emails_arg = sorted(scope_emails)
    else:
        emails_arg = None
    records = mail_service.list_mail_records(
        db, email=email, project_code=project_code,
        limit=page_size, offset=(page - 1) * page_size,
        emails=emails_arg,
    )
    items = [
        {
            "id": r.id,
            "email": r.email,
            "project_code": r.project_code,
            "message_id": r.message_id,
            "provider": r.provider,
            "folder": r.folder,
            "sender": r.sender,
            "subject": r.subject,
            "body_preview": r.body_preview,
            "body": r.body,
            "code": r.code,
            "code_source": r.code_source,
            "received_at": utc_iso(r.received_at),
        }
        for r in records
    ]
    return ok({"total": total, "page": page, "page_size": page_size, "items": items})


@router.get("/email/orders/{order_id}/mails")
def order_mail_logs(
    order_id: int,
    request: Request,
    db: Session = Depends(get_db),
    user=Depends(require_api_key_or_session),
) -> dict[str, Any]:
    """单个取号订单的收件日志（对齐 MailNest 订单详情弹窗）。

    管理员可看任意订单；客户只能看自己的。
    """
    from .auth import check_admin_access

    order = db.get(EmailOrder, order_id)
    if order is None:
        return err(CODE_NO_PROJECT, "订单不存在")
    if not check_admin_access(request):
        if user is None or order.user_id != user.id:
            raise HTTPException(status_code=403, detail="无权查看该订单")
    records = mail_service.list_mail_records(
        db, email=order.email, project_code=order.project_code, limit=50,
    )
    items = [
        {
            "id": r.id,
            "sender": r.sender,
            "subject": r.subject,
            "code": r.code,
            "received_at": utc_iso(r.received_at),
        }
        for r in records
    ]
    return ok({
        "order": {
            "id": order.id,
            "email": order.email,
            "project_code": order.project_code,
            "billing_status": order.billing_status,
            "price": billing_service.fmt_milli(order.price),
            "charged_amount": billing_service.fmt_milli(order.charged_amount),
            "created_at": utc_iso(order.created_at),
        },
        "total": len(items),
        "items": items,
    })


@router.get("/health")
def health(db: Session = Depends(get_db)) -> dict[str, Any]:
    """平台健康状态。

    `configured` 对**号池型 provider**（outlook）不适用：它的账号在
    email_accounts 表里而不是 provider 配置里，原来一律返回 false 会让人
    误以为没配置（实际能正常取号）。现在号池型改为看池里有没有可用账号。

    注意：`status` 只表示服务进程在线，**不代表每个号都能取到码**。
    """
    from ..providers import list_providers
    from ..services import settings_service

    providers = []
    for meta in list_providers():
        provider_id = meta["id"]
        pooled = bool(meta.get("pooled"))
        entry: dict[str, Any] = {"id": provider_id, "name": meta["name"], "pooled": pooled}
        if pooled:
            entry["configured"] = pool_service.count_claimable(db, provider_id) > 0
            entry["source"] = "email_pool"
            entry["stock"] = pool_service.count_claimable(db, provider_id)
        else:
            cfg = settings_service.get_provider_config(db, provider_id)
            entry["configured"] = bool(cfg)
            entry["source"] = "provider_config"
        providers.append(entry)
    project_count = len(mail_service.list_projects(db))
    return ok({"status": "online", "providers": providers, "project_count": project_count})
