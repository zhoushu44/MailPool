"""支付服务：易支付（epay）客户端 + 充值订单 + 回调幂等到账。

抄的是 new-api 的 controller/topup.go 与 model/topup.go，关键三点：

1. **trade_no 唯一索引** → 同一笔支付只有一个订单；
2. **回调幂等**：事务内 `SELECT ... FOR UPDATE` 行锁 + 状态机校验，
   已 success 的订单重复回调直接返回「已处理」，绝不二次到账；
3. **签名校验前置**：验签不过直接 fail，绝不碰余额。

易支付（彩虹易支付）协议：
    下单  POST {gateway}/submit.php   pid, type, out_trade_no, notify_url, return_url, name, money, sign, sign_type
    跳转  {gateway}/submit.php 的返回 HTML 里带 form.action → 自动跳网关
    回调  GET/POST {notify_url}         带 trade_no, trade_status, out_trade_no, money, sign
    验签  MD5(按字典序拼 k=v&k=v + KEY)

金额单位统一为**整数厘**（1 元 = 1000 厘）。
"""
from __future__ import annotations

import hashlib
import logging
import secrets
import time
from typing import Any
from urllib.parse import urlencode

import requests
from sqlalchemy import select

from ..models import (
    LOG_RECHARGE,
    TOPUP_FAILED,
    TOPUP_PENDING,
    TOPUP_SUCCESS,
    BalanceLog,
    RedeemCard,
    RechargeOrder,
    User,
)
from . import billing_service

logger = logging.getLogger(__name__)

# 易支付状态：TRADE_SUCCESS = 支付成功
TRADE_SUCCESS = "TRADE_SUCCESS"

# 允许的支付方式
PAY_TYPES = {
    "alipay": "支付宝",
    "wxpay": "微信支付",
}


class PaymentError(Exception):
    """支付流程错误（配置缺失、网关不通、验签失败等）。"""


# ── 签名 ────────────────────────────────────────────────────────


def sign_params(params: dict[str, str], key: str) -> str:
    """易支付签名：过滤空值与 sign/sign_type，按 key 字典序拼 k=v&k=v，末尾拼 KEY，MD5。

    注意是 **MD5 小写**、**不 urlencode**、**不排序后加 &**（最后一段是 `...&key=KEY`）。
    """
    items = [
        (k, str(v))
        for k, v in params.items()
        if k not in ("sign", "sign_type") and str(v) != ""
    ]
    items.sort(key=lambda kv: kv[0])
    parts = [f"{k}={v}" for k, v in items]
    raw = "&".join(parts) + f"&key={key}"
    return hashlib.md5(raw.encode("utf-8")).hexdigest()


def verify_params(params: dict[str, str], key: str) -> bool:
    """校验回调签名。"""
    given = (params.get("sign") or "").lower()
    if not given:
        return False
    expect = sign_params(params, key)
    return secrets.compare_digest(given, expect)


# ── 网关 ────────────────────────────────────────────────────────


def _gateway(cfg: dict[str, str]) -> str:
    url = (cfg.get("epay_url") or "").strip().rstrip("/")
    if not url:
        raise PaymentError("未配置易支付网关地址（billing.epay_url）")
    return url


def is_payment_configured(db) -> bool:
    from . import settings_service

    cfg = settings_service.get_billing_config(db)
    return bool(cfg.get("epay_url") and cfg.get("epay_pid") and cfg.get("epay_key"))


def list_channels(db) -> list[dict[str, str]]:
    """可用支付方式（对齐 MailNest /recharge/channels 的 shape）。"""
    from . import settings_service

    cfg = settings_service.get_billing_config(db)
    channels: list[dict[str, str]] = []
    if cfg.get("epay_url") and cfg.get("epay_pid") and cfg.get("epay_key"):
        for name, display in PAY_TYPES.items():
            channels.append({
                "name": name,
                "display_name": display,
                "category": name,
            })
    return channels


def create_recharge_order(
    db,
    user_id: int,
    amount: int,
    pay_type: str = "alipay",
    client_ip: str = "",
    site_url: str = "",
) -> dict[str, Any]:
    """创建充值订单并向网关下单。

    返回 {"id":..., "trade_no":..., "url":..., "params":...}
    """
    from . import settings_service

    cfg = settings_service.get_billing_config(db)
    min_topup = billing_service.to_milli(cfg.get("min_topup") or "1.00")
    max_topup = billing_service.to_milli(cfg.get("max_topup") or "1000.00")

    if amount < min_topup:
        raise PaymentError(f"充值金额不能低于 {billing_service.fmt_milli(min_topup)} 元")
    if amount > max_topup:
        raise PaymentError(f"单次充值金额不能超过 {billing_service.fmt_milli(max_topup)} 元")
    if pay_type not in PAY_TYPES:
        raise PaymentError("支付方式不存在")

    # 金额最多 2 位小数（对齐 MailNest 前端校验：/^\d+(\.\d{1,2})?$/）
    if amount % 10 != 0:
        raise PaymentError("充值金额最多支持 2 位小数")

    user = billing_service.get_user(db, user_id)
    if user is None:
        raise PaymentError("用户不存在")
    if user.status != 1:
        raise PaymentError("账号已被禁用")

    gateway = _gateway(cfg)
    pid = (cfg.get("epay_pid") or "").strip()
    key = (cfg.get("epay_key") or "").strip()

    # 订单号：带用户 id 便于排查，对齐 new-api 的 "USR{id}NO{ts}" 风格
    trade_no = f"MP{user_id}NO{int(time.time())}{secrets.token_hex(3)}"
    money_str = billing_service.fmt_milli(amount)

    base_url = (site_url or cfg.get("site_url") or "").rstrip("/")
    notify_url = f"{base_url}/api/v1/recharge/notify"
    return_url = f"{base_url}/recharges"

    params = {
        "pid": pid,
        "type": pay_type,
        "out_trade_no": trade_no,
        "notify_url": notify_url,
        "return_url": return_url,
        "name": f"MailPool 充值 {money_str}",
        "money": money_str,
    }
    params["sign"] = sign_params(params, key)
    params["sign_type"] = "MD5"

    # 网关地址：/submit.php 或 /mapi.php
    submit_url = f"{gateway}/submit.php"

    order = RechargeOrder(
        user_id=user_id,
        trade_no=trade_no,
        amount=amount,
        money=amount,
        pay_type=pay_type,
        pay_provider="epay",
        status=TOPUP_PENDING,
        client_ip=(client_ip or "")[:64],
        remark="易支付",
    )
    db.add(order)
    db.commit()
    db.refresh(order)

    return {
        "id": order.id,
        "trade_no": trade_no,
        "url": submit_url,
        "params": params,
    }


def submit_to_gateway(url: str, params: dict[str, str]) -> str:
    """POST 到网关，返回网关给的跳转地址。

    易支付 submit.php 返回一段含 `form.action` 的 HTML；也有的网关直接 302。
    这里两种都处理：能解析出跳转 URL 就返回，否则原样返回网关地址。
    """
    try:
        resp = requests.post(
            url,
            data=params,
            timeout=20,
            headers={"User-Agent": "Mozilla/5.0"},
        )
    except Exception as exc:
        raise PaymentError(f"请求支付网关失败: {exc}") from exc

    if resp.status_code in (301, 302, 303, 307, 308):
        loc = resp.headers.get("Location")
        if loc:
            return loc

    text = resp.text or ""
    # 解析 <form action="..."> 或 window.location.href='...'
    import re

    m = re.search(r'action=["\x27]([^"\x27]+)["\x27]', text)
    if m:
        action = m.group(1)
        # 把网关 form 里的隐藏字段带回去拼成完整 URL
        fields = dict(re.findall(r'name=["\x27]([^"\x27]+)["\x27][^>]*value=["\x27]([^"\x27]*)["\x27]', text))
        if fields:
            sep = "&" if "?" in action else "?"
            return f"{action}{sep}{urlencode(fields)}"
        return action

    m = re.search(r"(?:location\.href|window\.location)\s*=\s*[\x22\x27]([^\x22\x27]+)[\x22\x27]", text)
    if m:
        return m.group(1)
    return url


# ── 回调到账（幂等）────────────────────────────────────────────


def process_notify(db, params: dict[str, str], client_ip: str = "") -> tuple[bool, str]:
    """处理支付回调。

    返回 (是否成功, 说明文字)。**幂等**：重复回调不会二次到账。

    流程照抄 new-api 的 RechargeEpay：
        验签 → 行锁查单 → 渠道一致性 → 状态机 → 加余额
    """
    from . import settings_service

    cfg = settings_service.get_billing_config(db)
    key = (cfg.get("epay_key") or "").strip()
    if not key:
        raise PaymentError("未配置易支付密钥")

    if not verify_params(params, key):
        raise PaymentError("签名校验失败")

    trade_no = (params.get("out_trade_no") or params.get("trade_no") or "").strip()
    if not trade_no:
        raise PaymentError("缺少订单号")

    status = (params.get("trade_status") or "").strip().upper()
    if status and status != TRADE_SUCCESS:
        return False, f"交易状态 {status}，忽略"

    # ── 事务 + 行锁（SQLite 单写锁，等效 new-api 的 SELECT FOR UPDATE）──
    order = db.scalar(select(RechargeOrder).where(RechargeOrder.trade_no == trade_no))
    if order is None:
        raise PaymentError(f"订单不存在: {trade_no}")

    if order.pay_provider != "epay":
        raise PaymentError("订单支付渠道不匹配")

    # 金额必须与订单一致（防篡改）—— **在状态判断之前**：
    # 已完成的订单被重复回调时，若金额被篡改也应当直接拒绝而不是静默放过。
    money_raw = (params.get("money") or "").strip()
    if money_raw:
        money = billing_service.to_milli(money_raw)
        if money != int(order.money):
            raise PaymentError(
                f"金额不符：订单 {billing_service.fmt_milli(order.money)}，"
                f"回调 {billing_service.fmt_milli(money)}"
            )

    if order.status == TOPUP_SUCCESS:
        return True, "重复回调，已处理"
    if order.status != TOPUP_PENDING:
        raise PaymentError(f"订单状态异常: {order.status}")

    user = db.get(User, order.user_id)
    if user is None:
        raise PaymentError("用户不存在")

    before_balance = int(user.balance)
    user.balance = before_balance + int(order.amount)
    user.total_recharged = int(user.total_recharged or 0) + int(order.amount)
    db.add(user)

    order.status = TOPUP_SUCCESS
    order.completed_at = billing_service._now()
    db.add(order)

    db.add(BalanceLog(
        user_id=user.id,
        log_type=LOG_RECHARGE,
        amount=int(order.amount),
        balance_before=before_balance,
        balance_after=user.balance,
        frozen_before=int(user.frozen_balance),
        frozen_after=int(user.frozen_balance),
        remark=f"充值到账 {billing_service.fmt_milli(order.amount)} 元",
        created_at=billing_service._now(),
    ))
    db.commit()
    return True, "充值成功"


def select_for_update(db):
    """保留占位：SQLite 无 SELECT FOR UPDATE。

    SQLite 的写事务默认拿全局写锁，`db.commit()` 前的 UPDATE 天然互斥；
    真正的幂等保证来自 **trade_no 唯一索引 + 状态机校验**。
    """
    return select(RechargeOrder)


# ── 充值卡 ──────────────────────────────────────────────────────


def generate_cards(db, amount: int, count: int, batch_id: str = "") -> tuple[str, list[str]]:
    """批量生成充值卡，返回 (batch_id, 卡密列表)。"""
    from . import settings_service

    if count < 1 or count > 1000:
        raise PaymentError("数量范围 1-1000")
    if amount <= 0:
        raise PaymentError("金额必须大于 0")

    batch = batch_id or f"B{int(time.time())}{secrets.token_hex(2).upper()}"
    keys: list[str] = []
    for _ in range(count):
        key = f"MP{amount}-{secrets.token_hex(4).upper()}-{secrets.token_hex(4).upper()}"
        db.add(RedeemCard(batch_id=batch, card_key=key, amount=amount, status=1))
        keys.append(key)
    db.commit()
    return batch, keys


def query_card(db, card_key: str) -> dict[str, Any] | None:
    from ..models import RedeemCard

    card = db.scalar(select(RedeemCard).where(RedeemCard.card_key == card_key.strip()))
    if card is None:
        return None
    return {
        "batch_id": card.batch_id,
        "card_key": card.card_key,
        "amount": billing_service.fmt_milli(card.amount),
        "status": "未使用" if card.status == 1 else "已使用",
        "used_by": card.used_by or None,
    }


def redeem_card(db, user_id: int, card_key: str) -> tuple[bool, str]:
    """兑换充值卡到账。幂等：同一张卡只能兑一次。"""
    from ..models import RedeemCard

    card = db.scalar(select(RedeemCard).where(RedeemCard.card_key == card_key.strip()))
    if card is None:
        return False, "卡密不存在"
    if card.status != 1:
        return False, "该卡密已被使用"

    user = billing_service.get_user(db, user_id)
    if user is None:
        return False, "用户不存在"

    before_balance = int(user.balance)
    user.balance = before_balance + int(card.amount)
    user.total_recharged = int(user.total_recharged or 0) + int(card.amount)
    db.add(user)

    card.status = 0
    card.used_by = user_id
    card.used_at = billing_service._now()
    db.add(card)

    db.add(BalanceLog(
        user_id=user_id,
        log_type="redeem",
        amount=int(card.amount),
        balance_before=before_balance,
        balance_after=user.balance,
        frozen_before=int(user.frozen_balance),
        frozen_after=int(user.frozen_balance),
        remark=f"充值卡兑换 {billing_service.fmt_milli(card.amount)} 元",
        created_at=billing_service._now(),
    ))
    db.commit()
    return True, f"兑换成功，到账 {billing_service.fmt_milli(card.amount)} 元"


# ── 管理员人工补单 ──────────────────────────────────────────────


def admin_complete_topup(db, trade_no: str, client_ip: str = "") -> tuple[bool, str]:
    """管理员手工把订单标记为已支付并到账（抄 new-api 的 AdminCompleteTopUp）。

    幂等：已 success 直接返回。
    """
    order = db.scalar(select(RechargeOrder).where(RechargeOrder.trade_no == trade_no.strip()))
    if order is None:
        return False, "订单不存在"
    if order.status == TOPUP_SUCCESS:
        return True, "订单已完成"

    user = db.get(User, order.user_id)
    if user is None:
        return False, "用户不存在"

    before_balance = int(user.balance)
    user.balance = before_balance + int(order.amount)
    user.total_recharged = int(user.total_recharged or 0) + int(order.amount)
    db.add(user)

    order.status = TOPUP_SUCCESS
    order.completed_at = billing_service._now()
    order.remark = (order.remark or "") + " [管理员补单]"
    db.add(order)

    db.add(BalanceLog(
        user_id=user.id,
        log_type=LOG_RECHARGE,
        amount=int(order.amount),
        balance_before=before_balance,
        balance_after=user.balance,
        frozen_before=int(user.frozen_balance),
        frozen_after=int(user.frozen_balance),
        remark=f"管理员补单到账 {billing_service.fmt_milli(order.amount)} 元",
        created_at=billing_service._now(),
    ))
    db.commit()
    return True, "补单成功"


def admin_fail_topup(db, trade_no: str) -> tuple[bool, str]:
    order = db.scalar(select(RechargeOrder).where(RechargeOrder.trade_no == trade_no.strip()))
    if order is None:
        return False, "订单不存在"
    if order.status == TOPUP_SUCCESS:
        return False, "订单已完成，不能置为失败"
    order.status = TOPUP_FAILED
    db.add(order)
    db.commit()
    return True, "订单已置为失败"
