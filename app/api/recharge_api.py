"""充值 API（对齐 MailNest /recharges + new-api 充值流程）。

- GET  /api/v1/recharge            余额总览（四栏）
- GET  /api/v1/recharge/channels   支付方式
- POST /api/v1/recharge/create     创建充值订单（返回网关跳转地址）
- GET  /api/v1/recharge/query      查询订单状态
- POST /api/v1/recharge/notify     支付回调（易支付）
- POST /api/v1/recharge/redeem-card 充值卡兑换
- GET  /api/v1/recharge/orders     充值订单列表

鉴权：Bearer API Key（与对外邮箱 API 同一套），API Key 绑定到具体用户，
扣费与充值都落在同一个 user_id 上。
"""
from __future__ import annotations

import logging
from typing import Any

from fastapi import APIRouter, Depends, Query, Request
from pydantic import BaseModel
from sqlalchemy.orm import Session

from ..database import get_db
from ..services import billing_service, payment_service
from .auth import current_user, require_api_key_or_session, require_user

logger = logging.getLogger(__name__)

# 充值接口要认证，但**两种身份都认**：
#   - 绑定用户的 API Key（对外客户端用）
#   - 网页登录会话（客户在「余额与充值」页自己操作）
# 没身份就 401，谁都能下单是不行的。
router = APIRouter(prefix="/api/v1", dependencies=[Depends(require_api_key_or_session)])
# 支付回调由网关发起，没有用户身份 → 单独挂一个不带鉴权的路由
public_router = APIRouter(prefix="/api/v1")


class RechargeCreateBody(BaseModel):
    amount: float | str
    pay_type: str = "alipay"
    category: str = ""


class RedeemCardBody(BaseModel):
    card_key: str


def ok(data: Any = None) -> dict[str, Any]:
    return {"code": "00000", "msg": "", "type": "", "data": data}


def err(code: str, msg: str) -> dict[str, Any]:
    return {"code": code, "msg": msg, "type": "error", "data": None}


@router.get("/recharge")
def recharge_overview(db: Session = Depends(get_db), user=Depends(require_user)) -> dict[str, Any]:
    """余额总览：总余额 / 可用 / 冻结 / 扫码充值 / 充值卡 / 赠送。"""
    return ok(billing_service.balance_snapshot(db, user.id))


@router.get("/recharge/channels")
def recharge_channels(db: Session = Depends(get_db)) -> dict[str, Any]:
    """可用支付方式列表。"""
    return ok({"channels": payment_service.list_channels(db)})


@router.post("/recharge/create")
def recharge_create(
    body: RechargeCreateBody,
    request: Request,
    db: Session = Depends(get_db),
    user=Depends(require_user),
) -> dict[str, Any]:
    """创建充值订单，返回网关跳转地址与提交参数。"""
    amount = billing_service.to_milli(body.amount)
    try:
        order = payment_service.create_recharge_order(
            db,
            user_id=user.id,
            amount=amount,
            pay_type=body.pay_type,
            client_ip=request.client.host if request.client else "",
        )
    except payment_service.PaymentError as exc:
        return err("D0007", str(exc))

    try:
        jump_url = payment_service.submit_to_gateway(order["url"], order["params"])
    except payment_service.PaymentError as exc:
        logger.warning("支付网关下单失败 trade_no=%s: %s", order["trade_no"], exc)
        jump_url = order["url"]

    return ok({
        "id": order["id"],
        "trade_no": order["trade_no"],
        "key": "urlscheme",
        "url": jump_url,
        "params": order["params"],
    })


@router.get("/recharge/query")
def recharge_query(id: str = Query(default=""), db: Session = Depends(get_db), user=Depends(require_user)) -> dict[str, Any]:
    """查询充值订单状态（前端轮询用）。"""
    from sqlalchemy import select

    from ..models import RechargeOrder

    stmt = select(RechargeOrder).where(RechargeOrder.trade_no == id)
    if id.isdigit():
        stmt = select(RechargeOrder).where(RechargeOrder.id == int(id))
    order = db.scalar(stmt)
    if order is None:
        return err("D0003", "订单不存在")
    if order.user_id != user.id:
        return err("D0004", "无权查看该订单")
    return ok({
        "id": order.id,
        "trade_no": order.trade_no,
        "amount": billing_service.fmt_milli(order.amount),
        "money": billing_service.fmt_milli(order.money),
        "pay_type": order.pay_type,
        "status": order.status,
        "created_at": order.created_at.isoformat() if order.created_at else "",
        "paid_at": order.completed_at.isoformat() if order.completed_at else "",
    })


@router.get("/recharge/orders")
def recharge_orders(
    page: int = Query(default=1, ge=1),
    page_size: int = Query(default=20, ge=1, le=100),
    db: Session = Depends(get_db),
    user=Depends(require_user),
) -> dict[str, Any]:
    rows, total = billing_service.list_recharge_orders(
        db, user_id=user.id, limit=page_size, offset=(page - 1) * page_size
    )
    return ok({
        "total": total,
        "page": page,
        "page_size": page_size,
        "items": [
            {
                "id": r.id,
                "trade_no": r.trade_no,
                "amount": billing_service.fmt_milli(r.amount),
                "money": billing_service.fmt_milli(r.money),
                "pay_type": r.pay_type,
                "status": r.status,
                "created_at": r.created_at.isoformat() if r.created_at else "",
                "paid_at": r.completed_at.isoformat() if r.completed_at else "",
            }
            for r in rows
        ],
    })


@public_router.post("/recharge/notify")
async def recharge_notify(request: Request, db: Session = Depends(get_db)) -> dict[str, Any]:
    """支付回调（易支付）。

    幂等：重复回调不会二次到账。验签失败直接 fail（不碰余额）。
    """
    params: dict[str, str] = {}
    try:
        if request.method == "POST":
            form = await request.form()
            params = {k: str(v) for k, v in form.items()}
        else:
            params = {k: str(v) for k, v in request.query_params.items()}
    except Exception:
        params = {k: str(v) for k, v in request.query_params.items()}

    client_ip = request.client.host if request.client else ""
    try:
        success, message = payment_service.process_notify(db, params, client_ip)
    except payment_service.PaymentError as exc:
        logger.warning("充值回调处理失败: %s", exc)
        return {"code": "D0007", "msg": str(exc)}
    return {"code": "00000", "msg": message, "type": "", "data": {"success": success}}


@router.post("/recharge/redeem-card")
def recharge_redeem_card(
    body: RedeemCardBody,
    db: Session = Depends(get_db),
    user=Depends(require_user),
) -> dict[str, Any]:
    """充值卡兑换到账。"""
    success, message = payment_service.redeem_card(db, user.id, body.card_key)
    if not success:
        return err("D0007", message)
    return ok({"message": message, "balance": billing_service.balance_snapshot(db, user.id)})
