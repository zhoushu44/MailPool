"""管理 API（计费相关）：用户 / 充值卡 / 充值订单 / 计费配置。

抄 new-api 的管理面：
- 管理员可以给用户加钱（人工调整）
- 管理员可以给订单补单（AdminCompleteTopUp）
- 兑换码批量生成（Redemption）
"""
from __future__ import annotations

import logging
from typing import Any

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel
from sqlalchemy.orm import Session

from ..database import get_db
from ..models import ApiKey, BalanceLog, EmailOrder, RedeemCard, RechargeOrder
from ..services import billing_service, payment_service, settings_service
from .auth import require_admin

logger = logging.getLogger(__name__)

router = APIRouter(prefix="/api/admin/billing", dependencies=[Depends(require_admin)])


# ── 用户 ────────────────────────────────────────────────────────


@router.get("/users")
def list_users(
    page: int = 1,
    page_size: int = 50,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    page = max(1, page)
    page_size = min(max(1, page_size), 200)
    rows, total = billing_service.list_users(db, limit=page_size, offset=(page - 1) * page_size)
    return {"code": 0, "data": {
        "total": total,
        "items": [
            {
                "id": r.id,
                "username": r.username,
                "email": r.email,
                "role": r.role,
                "status": r.status,
                "balance": billing_service.fmt_milli(r.balance),
                "frozen_balance": billing_service.fmt_milli(r.frozen_balance),
                "available_balance": billing_service.fmt_milli(billing_service.available_balance(r)),
                "total_recharged": billing_service.fmt_milli(r.total_recharged),
                "total_charged": billing_service.fmt_milli(r.total_charged),
                "created_at": r.created_at.isoformat() if r.created_at else "",
            }
            for r in rows
        ],
    }}


class CreateUserBody(BaseModel):
    username: str
    password: str = ""
    email: str = ""
    role: int = 2


@router.post("/users")
def create_user(body: CreateUserBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    username = body.username.strip()
    if not username:
        raise HTTPException(status_code=400, detail="用户名不能为空")
    if billing_service.get_user_by_name(db, username) is not None:
        raise HTTPException(status_code=400, detail="用户名已存在")
    password_hash = ""
    if body.password:
        password_hash = hash_password(body.password)
    user = billing_service.create_user(
        db, username, password_hash=password_hash, role=body.role, email=body.email.strip()
    )
    return {"code": 0, "data": {"id": user.id, "username": user.username}}


class AdjustBalanceBody(BaseModel):
    amount: float
    remark: str = ""


@router.delete("/users/{user_id}")
def delete_user(
    user_id: int,
    force: bool = False,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    """删除用户（连带他的 Key、流水、订单、充值卡使用记录）。

    余额非 0 时默认拒绝 —— 免得手滑把还在充值的客户删了。
    传 force=true 强制删（余额会被清零，流水一并抹掉）。
    """
    user = billing_service.get_user(db, user_id)
    if user is None:
        raise HTTPException(status_code=404, detail="用户不存在")

    balance = int(user.balance or 0)
    frozen = int(user.frozen_balance or 0)
    if (balance or frozen) and not force:
        raise HTTPException(
            status_code=400,
            detail=f"用户还有余额 {billing_service.fmt_milli(balance)} 元"
                   f"（冻结 {billing_service.fmt_milli(frozen)} 元），确认清零请传 force=true",
        )

    db.query(ApiKey).filter(ApiKey.user_id == user_id).delete()
    db.query(BalanceLog).filter(BalanceLog.user_id == user_id).delete()
    db.query(RechargeOrder).filter(RechargeOrder.user_id == user_id).delete()
    db.query(EmailOrder).filter(EmailOrder.user_id == user_id).delete()
    db.query(RedeemCard).filter(RedeemCard.used_by == user_id).update(
        {RedeemCard.used_by: 0, RedeemCard.status: 0, RedeemCard.used_at: None}
    )
    db.delete(user)
    db.commit()
    return {"code": 0, "data": {"deleted": user_id}}


@router.post("/users/{user_id}/balance")
def adjust_balance(
    user_id: int,
    body: AdjustBalanceBody,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    """管理员给用户加钱（正数）或扣钱（负数）。"""
    amount = billing_service.to_milli(body.amount)
    if amount == 0:
        raise HTTPException(status_code=400, detail="金额不能为 0")
    user = billing_service.get_user(db, user_id)
    if user is None:
        raise HTTPException(status_code=404, detail="用户不存在")
    try:
        if amount > 0:
            # 管理员手动充值 = 客户线下付了钱，计入「累计充值」
            billing_service.credit(db, user_id, amount, "adjust",
                                   body.remark or "管理员充值",
                                   count_as_recharged=True)
        else:
            billing_service.debit(
                db, user_id, -amount, "adjust", body.remark or "管理员扣款",
                require_available=False,
            )
    except Exception as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    db.commit()
    return {"code": 0, "data": {
        "balance": billing_service.fmt_milli(user.balance),
        "available_balance": billing_service.fmt_milli(billing_service.available_balance(user)),
    }}


@router.get("/users/{user_id}/logs")
def user_logs(
    user_id: int,
    page: int = 1,
    page_size: int = 50,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    page = max(1, page)
    page_size = min(max(1, page_size), 200)
    rows, total = billing_service.list_balance_logs(
        db, user_id, limit=page_size, offset=(page - 1) * page_size
    )
    orders, orders_total = billing_service.list_email_orders(
        db, user_id, limit=page_size, offset=(page - 1) * page_size
    )
    return {"code": 0, "data": {
        "total": total,
        "logs": [
            {
                "id": r.id,
                "log_type": r.log_type,
                "amount": billing_service.fmt_milli(r.amount),
                "balance_before": billing_service.fmt_milli(r.balance_before),
                "balance_after": billing_service.fmt_milli(r.balance_after),
                "frozen_before": billing_service.fmt_milli(r.frozen_before),
                "frozen_after": billing_service.fmt_milli(r.frozen_after),
                "remark": r.remark,
                "created_at": r.created_at.isoformat() if r.created_at else "",
            }
            for r in rows
        ],
        "orders_total": orders_total,
        "orders": [
            {
                "id": o.id,
                "email": o.email,
                "project_code": o.project_code,
                "price": billing_service.fmt_milli(o.price),
                "billing_status": o.billing_status,
                "charged_amount": billing_service.fmt_milli(o.charged_amount),
                "created_at": o.created_at.isoformat() if o.created_at else "",
            }
            for o in orders
        ],
    }}


# ── 充值卡 ──────────────────────────────────────────────────────


class GenerateCardsBody(BaseModel):
    amount: float
    count: int = 1


@router.post("/cards/generate")
def generate_cards(body: GenerateCardsBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    """批量生成充值卡。"""
    try:
        batch_id, keys = payment_service.generate_cards(
            db, billing_service.to_milli(body.amount), body.count
        )
    except payment_service.PaymentError as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    return {"code": 0, "data": {"batch_id": batch_id, "count": len(keys), "keys": keys}}


@router.get("/cards/query")
def query_card(card_no: str = "", db: Session = Depends(get_db)) -> dict[str, Any]:
    """按卡密或批次 ID 查询。"""
    if not card_no:
        raise HTTPException(status_code=400, detail="请提供卡密")
    info = payment_service.query_card(db, card_no)
    if info is None:
        # 可能是批次 ID
        from sqlalchemy import select

        from ..models import RedeemCard

        cards = list(db.scalars(select(RedeemCard).where(RedeemCard.batch_id == card_no.strip())))
        if not cards:
            raise HTTPException(status_code=404, detail="卡密或批次不存在")
        return {"code": 0, "data": {"batch_id": card_no, "count": len(cards), "cards": [
            {
                "card_key": c.card_key,
                "amount": billing_service.fmt_milli(c.amount),
                "status": "未使用" if c.status == 1 else "已使用",
            } for c in cards
        ]}}
    return {"code": 0, "data": info}


# ── 充值订单 ────────────────────────────────────────────────────


@router.get("/orders")
def list_orders(
    page: int = 1,
    page_size: int = 50,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    page = max(1, page)
    page_size = min(max(1, page_size), 200)
    rows, total = billing_service.list_recharge_orders(
        db, user_id=None, limit=page_size, offset=(page - 1) * page_size
    )
    return {"code": 0, "data": {
        "total": total,
        "items": [
            {
                "id": r.id,
                "user_id": r.user_id,
                "trade_no": r.trade_no,
                "amount": billing_service.fmt_milli(r.amount),
                "money": billing_service.fmt_milli(r.money),
                "pay_type": r.pay_type,
                "pay_provider": r.pay_provider,
                "status": r.status,
                "remark": r.remark,
                "created_at": r.created_at.isoformat() if r.created_at else "",
                "completed_at": r.completed_at.isoformat() if r.completed_at else "",
            }
            for r in rows
        ],
    }}


class CompleteTopupBody(BaseModel):
    trade_no: str


@router.post("/orders/complete")
def complete_topup(body: CompleteTopupBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    """管理员补单：把订单标记为已支付并到账（幂等）。"""
    success, message = payment_service.admin_complete_topup(db, body.trade_no)
    if not success:
        raise HTTPException(status_code=400, detail=message)
    return {"code": 0, "msg": message}


@router.post("/orders/fail")
def fail_topup(body: CompleteTopupBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    success, message = payment_service.admin_fail_topup(db, body.trade_no)
    if not success:
        raise HTTPException(status_code=400, detail=message)
    return {"code": 0, "msg": message}


# ── API Key（绑定用户）─────────────────────────────────────────


@router.get("/api-keys")
def list_api_keys(
    user_id: int | None = None,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    """列出 API Key。带 user_id 的是计费 key，其余是老的匿名 key。"""
    rows = settings_service.list_user_api_keys(db, user_id)
    legacy = settings_service.get_api_keys(db)
    return {"code": 0, "data": {
        "keys": [
            {
                "id": r.id,
                "user_id": r.user_id,
                "key": r.key,
                "name": r.name,
                "enabled": r.enabled,
                "created_at": r.created_at.isoformat() if r.created_at else "",
                "last_used_at": r.last_used_at.isoformat() if r.last_used_at else "",
            }
            for r in rows
        ],
        "legacy_keys": legacy,
    }}


class IssueKeyBody(BaseModel):
    user_id: int
    name: str = ""


@router.post("/api-keys")
def issue_api_key(body: IssueKeyBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    """给用户签发 API Key。"""
    if billing_service.get_user(db, body.user_id) is None:
        raise HTTPException(status_code=404, detail="用户不存在")
    key = settings_service.create_user_api_key(db, body.user_id, body.name)
    return {"code": 0, "data": {"key": key, "user_id": body.user_id}}


@router.delete("/api-keys/{key_id}")
def revoke_api_key(key_id: int, db: Session = Depends(get_db)) -> dict[str, Any]:
    if not settings_service.delete_user_api_key(db, key_id):
        raise HTTPException(status_code=404, detail="Key 不存在")
    return {"code": 0, "msg": "已吊销"}


# ── 计费配置 ────────────────────────────────────────────────────


@router.get("/config")
def get_config(db: Session = Depends(get_db)) -> dict[str, Any]:
    return {"code": 0, "data": settings_service.get_billing_config(db)}


@router.put("/config")
def save_config(body: dict[str, Any], db: Session = Depends(get_db)) -> dict[str, Any]:
    cfg = settings_service.save_billing_config(db, body)
    return {"code": 0, "data": cfg}


# ── 密码哈希（PBKDF2，标准库实现，不引第三方依赖）─────────────


def hash_password(password: str) -> str:
    import hashlib
    import os

    salt = os.urandom(16)
    digest = hashlib.pbkdf2_hmac("sha256", password.encode("utf-8"), salt, 200_000)
    return f"pbkdf2_sha256$200000${salt.hex()}${digest.hex()}"


def verify_password(password: str, stored: str) -> bool:
    import hashlib
    import hmac

    try:
        algo, rounds, salt_hex, digest_hex = stored.split("$")
        if algo != "pbkdf2_sha256":
            return False
        digest = hashlib.pbkdf2_hmac(
            "sha256", password.encode("utf-8"), bytes.fromhex(salt_hex), int(rounds)
        )
        return hmac.compare_digest(digest.hex(), digest_hex)
    except Exception:
        return False
