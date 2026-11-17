"""计费服务：余额三段式扣费（hold → release → charge）。

设计说明
--------
**充值**抄 new-api：trade_no 唯一索引 + 事务 + 状态机幂等 + 整数厘金额。
**扣费**抄 MailNest（迈巢）：取号不扣钱，只冻结；收到验证码才真扣。

    buy   → hold    : frozen += price, balance 不变, order.billing_status = pending
    超时  → release : frozen -= price, balance 不变, order.billing_status = free
    收码  → charge  : frozen -= price, balance -= price, order.billing_status = charged

为什么这么设计：
1. 用户体感是「用了才付费」，但钱在冻结位锁着跑不掉；
2. 迈巢实测就是这个口径（/api/v1/balance-logs 的 log_type 就是
   recharge / hold / charge / release 四种），对齐后两边数据能互相印证；
3. 余额不足时在 hold 阶段就拦（返回 D0001），不会先把号发出去再赖账。

金额单位统一为**整数厘**（1 元 = 1000 厘），见 models.MONEY_SCALE。
"""
from __future__ import annotations

import logging
from datetime import datetime, timedelta, timezone
from typing import Any

from sqlalchemy import func, select
from sqlalchemy.orm import Session

from ..models import (
    LOG_ADJUST,
    LOG_AWARD,
    LOG_CHARGE,
    LOG_HOLD,
    LOG_RECHARGE,
    LOG_REDEEM,
    LOG_RELEASE,
    MONEY_SCALE,
    BalanceLog,
    EmailOrder,
    Project,
    RechargeOrder,
    User,
)

logger = logging.getLogger(__name__)


class InsufficientBalance(Exception):
    """余额不足（冻结额度不够）。"""

    def __init__(self, need: int, available: int) -> None:
        super().__init__(f"余额不足，需要 {fmt_milli(need)} 元，可用 {fmt_milli(available)} 元")
        self.need = need
        self.available = available


# ── 金额工具 ────────────────────────────────────────────────────


def to_milli(value: float | int | str) -> int:
    """元 → 厘（整数）。四舍五入到最近的厘。

    用 Decimal 而非 float：float("0.018")*1000 = 17.999999999999996，
    直接 int() 会截成 17，账就差 1 厘。
    """
    from decimal import ROUND_HALF_UP, Decimal

    try:
        d = Decimal(str(value))
    except Exception:
        return 0
    return int((d * MONEY_SCALE).quantize(Decimal("1"), rounding=ROUND_HALF_UP))


def fmt_milli(value: int | None) -> str:
    """厘 → 元字符串（3 位小数）。"""
    if value is None:
        value = 0
    sign = "-" if value < 0 else ""
    v = abs(int(value))
    return f"{sign}{v // MONEY_SCALE}.{v % MONEY_SCALE:03d}"


def _now() -> datetime:
    return datetime.now(timezone.utc)


# ── 用户 ────────────────────────────────────────────────────────


def get_user(db: Session, user_id: int) -> User | None:
    if not user_id:
        return None
    return db.get(User, user_id)


def get_user_by_name(db: Session, username: str) -> User | None:
    return db.scalar(select(User).where(User.username == username))


def list_users(db: Session, limit: int = 100, offset: int = 0) -> tuple[list[User], int]:
    total = db.scalar(select(func.count()).select_from(User)) or 0
    rows = list(db.scalars(select(User).order_by(User.id.asc()).limit(limit).offset(offset)))
    return rows, total


def create_user(
    db: Session,
    username: str,
    password_hash: str = "",
    role: int = 2,
    email: str = "",
) -> User:
    user = User(username=username, password_hash=password_hash, role=role, email=email)
    db.add(user)
    db.commit()
    db.refresh(user)
    return user


def available_balance(user: User | None) -> int:
    """可用余额 = 总余额 - 冻结。"""
    if user is None:
        return 0
    return int(user.balance) - int(user.frozen_balance)


def balance_snapshot(db: Session, user_id: int) -> dict[str, Any]:
    """余额六栏（对齐 MailNest /api/v1/balance）。

    所有分栏都从 balance_logs 流水算出来 —— 流水是唯一事实来源，
    拿充值订单表去减充值卡流水会出现负数（人工调整过的账户必炸）。
    """
    user = get_user(db, user_id)
    if user is None:
        return {
            "balance": "0.000",
            "frozen_balance": "0.000",
            "available_balance": "0.000",
            "scan_recharge": "0.000",
            "card_recharge": "0.000",
            "award_amount": "0.000",
        }

    rows = db.execute(
        select(BalanceLog.log_type, func.coalesce(func.sum(BalanceLog.amount), 0))
        .where(BalanceLog.user_id == user_id)
        .group_by(BalanceLog.log_type)
    ).all()
    buckets = {log_type: int(total) for log_type, total in rows}

    scan = buckets.get(LOG_RECHARGE, 0)
    card = buckets.get(LOG_REDEEM, 0)
    award = buckets.get(LOG_AWARD, 0)

    return {
        "balance": fmt_milli(user.balance),
        "frozen_balance": fmt_milli(user.frozen_balance),
        "available_balance": fmt_milli(available_balance(user)),
        "scan_recharge": fmt_milli(scan),
        "card_recharge": fmt_milli(card),
        "award_amount": fmt_milli(award),
    }


# ── 内部记账 ────────────────────────────────────────────────────


def _write_log(
    db: Session,
    user_id: int,
    log_type: str,
    amount: int,
    balance_before: int,
    balance_after: int,
    frozen_before: int,
    frozen_after: int,
    remark: str = "",
) -> BalanceLog:
    log = BalanceLog(
        user_id=user_id,
        log_type=log_type,
        amount=amount,
        balance_before=balance_before,
        balance_after=balance_after,
        frozen_before=frozen_before,
        frozen_after=frozen_after,
        remark=remark[:255],
        created_at=_now(),
    )
    db.add(log)
    return log


def credit(
    db: Session,
    user_id: int,
    amount: int,
    log_type: str = LOG_ADJUST,
    remark: str = "",
    flush: bool = False,
    count_as_recharged: bool | None = None,
) -> User:
    """给用户加钱（amount 为正数厘）。

    余额上限保护：new-api 用 `WHERE quota <= max` 保证不会溢出，
    这里同样在 UPDATE 条件里带上上限，越界时抛异常而不是静默截断。

    count_as_recharged：是否计入「累计充值」。
    传 None 时自动判断 —— 充值类（recharge/redeem/award）算，管理员手动调整不算。
    """
    user = get_user(db, user_id)
    if user is None:
        raise ValueError(f"用户不存在: {user_id}")
    if amount < 0:
        raise ValueError("credit 的金额必须为正")
    if count_as_recharged is None:
        count_as_recharged = log_type in (LOG_RECHARGE, LOG_REDEEM, LOG_AWARD)
    before_balance = int(user.balance)
    before_frozen = int(user.frozen_balance)
    user.balance = before_balance + amount
    if count_as_recharged:
        user.total_recharged = int(user.total_recharged or 0) + amount
    db.add(user)
    _write_log(
        db, user_id, log_type, amount,
        before_balance, user.balance, before_frozen, before_frozen,
        remark,
    )
    if flush:
        db.flush()
    return user


def debit(
    db: Session,
    user_id: int,
    amount: int,
    log_type: str = LOG_CHARGE,
    remark: str = "",
    require_available: bool = True,
) -> User:
    """从用户余额扣钱（amount 为正数厘，实际减少 amount）。"""
    user = get_user(db, user_id)
    if user is None:
        raise ValueError(f"用户不存在: {user_id}")
    if amount < 0:
        raise ValueError("debit 的金额必须为正")
    before_balance = int(user.balance)
    before_frozen = int(user.frozen_balance)
    if require_available and available_balance(user) < amount:
        raise InsufficientBalance(amount, available_balance(user))
    user.balance = before_balance - amount
    db.add(user)
    _write_log(
        db, user_id, log_type, -amount,
        before_balance, user.balance, before_frozen, before_frozen,
        remark,
    )
    return user


# ── 单价 ────────────────────────────────────────────────────────


def project_price(db: Session, project: Project) -> int:
    """项目单价（厘）。

    优先取项目 config_json 里的 `price`（元），其次取全局设置
    `billing.default_price`（元）。都没有则 0 —— 即免费（保持旧行为）。
    """
    import json

    from . import settings_service

    cfg: dict[str, Any] = {}
    try:
        raw = json.loads(project.config_json or "{}")
        if isinstance(raw, dict):
            cfg = raw
    except Exception:
        cfg = {}

    raw_price = cfg.get("price")
    if raw_price not in (None, ""):
        try:
            return max(0, to_milli(raw_price))
        except Exception:
            pass

    default_raw = settings_service.get_billing_config(db).get("default_price", "")
    if default_raw not in (None, ""):
        try:
            return max(0, to_milli(default_raw))
        except Exception:
            pass
    return 0


def is_billing_enabled(db: Session) -> bool:
    """是否开启计费。全局开关 `billing.enabled`。

    默认关闭 —— 不影响现有部署的免费行为，打开后 D0001 才真正生效。
    """
    from . import settings_service

    cfg = settings_service.get_billing_config(db)
    return str(cfg.get("enabled", "false")).lower() in ("1", "true", "yes", "on")


# ── 三段式扣费 ──────────────────────────────────────────────────


def hold(
    db: Session,
    user_id: int,
    email: str,
    project_code: str,
    provider: str = "",
    price: int = 0,
    remark: str = "",
) -> EmailOrder | None:
    """取号冻结：frozen += price，balance 不变。

    price 为 0（免费项目）时只建订单记录，不动余额。
    """
    order = db.scalar(select(EmailOrder).where(EmailOrder.email == email.lower()))
    if order is not None:
        # 已存在订单（同一邮箱只取一次）：不重复冻结
        return order

    user = get_user(db, user_id)
    if user is None:
        raise ValueError(f"用户不存在: {user_id}")

    before_frozen = int(user.frozen_balance)
    if price > 0:
        if available_balance(user) < price:
            raise InsufficientBalance(price, available_balance(user))
        user.frozen_balance = before_frozen + price
        db.add(user)
        _write_log(
            db, user_id, LOG_HOLD, 0,
            int(user.balance), int(user.balance), before_frozen, user.frozen_balance,
            remark or f"取号冻结 {email}",
        )

    order = EmailOrder(
        user_id=user_id,
        email=email.lower(),
        project_code=project_code,
        provider=provider,
        price=price,
        billing_status="pending",
        hold_amount=price,
        charged_amount=0,
        created_at=_now(),
    )
    db.add(order)
    db.flush()
    return order


def charge(
    db: Session,
    user_id: int,
    email: str,
    remark: str = "",
) -> EmailOrder | None:
    """收到验证码 → 真扣：frozen -= price, balance -= price。"""
    order = db.scalar(select(EmailOrder).where(EmailOrder.email == email.lower()))
    if order is None:
        return None
    if order.billing_status == "charged":
        return order  # 幂等：已扣过不再扣
    if order.billing_status != "pending":
        return order  # 已释放/已免单，不再扣

    price = int(order.hold_amount or 0)
    user = get_user(db, user_id)
    if user is None:
        raise ValueError(f"用户不存在: {user_id}")

    if price > 0:
        before_balance = int(user.balance)
        before_frozen = int(user.frozen_balance)
        # 冻结额先退，再从总余额扣 —— 与 MailNest 的账目顺序一致
        user.frozen_balance = max(0, before_frozen - price)
        user.balance = before_balance - price
        user.total_charged = int(user.total_charged or 0) + price
        db.add(user)
        _write_log(
            db, user_id, LOG_CHARGE, -price,
            before_balance, user.balance, before_frozen, user.frozen_balance,
            remark or f"收码扣费 {email}",
        )

    order.billing_status = "charged"
    order.charged_amount = price
    order.charged_at = _now()
    db.add(order)
    db.flush()
    return order


def release(
    db: Session,
    user_id: int,
    email: str,
    remark: str = "",
) -> EmailOrder | None:
    """释放/超时 → 解冻：frozen -= price，balance 不变，订单标记 free。"""
    order = db.scalar(select(EmailOrder).where(EmailOrder.email == email.lower()))
    if order is None:
        return None
    if order.billing_status in ("charged", "free"):
        return order  # 幂等

    price = int(order.hold_amount or 0)
    user = get_user(db, user_id)
    if user is not None and price > 0:
        before_balance = int(user.balance)
        before_frozen = int(user.frozen_balance)
        user.frozen_balance = max(0, before_frozen - price)
        db.add(user)
        _write_log(
            db, user_id, LOG_RELEASE, 0,
            before_balance, user.balance, before_frozen, user.frozen_balance,
            remark or f"释放解冻 {email}",
        )

    order.billing_status = "free"
    order.released_at = _now()
    db.add(order)
    db.flush()
    return order


def release_expired(db: Session, timeout_seconds: int = 1200, limit: int = 500) -> int:
    """定时任务：把超时的 pending 订单释放（不扣费）。

    迈巢的临时邮箱有有效期（20 分钟），超时自动释放。
    """
    cutoff = _now() - timedelta(seconds=max(0, timeout_seconds))
    orders = list(
        db.scalars(
            select(EmailOrder)
            .where(EmailOrder.billing_status == "pending", EmailOrder.created_at < cutoff)
            .limit(limit)
        )
    )
    count = 0
    for order in orders:
        release(db, order.user_id, order.email, remark="超时自动释放")
        count += 1
    if count:
        db.commit()
    return count


def list_balance_logs(
    db: Session,
    user_id: int | None,
    limit: int = 20,
    offset: int = 0,
    log_type: str | None = None,
) -> tuple[list[BalanceLog], int]:
    """流水列表。

    user_id 传 None = 全部用户（管理员看全局）。
    """
    stmt = select(BalanceLog)
    if user_id is not None:
        stmt = stmt.where(BalanceLog.user_id == user_id)
    if log_type:
        stmt = stmt.where(BalanceLog.log_type == log_type)
    total = db.scalar(select(func.count()).select_from(stmt.subquery())) or 0
    rows = list(db.scalars(stmt.order_by(BalanceLog.id.desc()).limit(limit).offset(offset)))
    return rows, total


def list_email_orders(
    db: Session,
    user_id: int | None,
    limit: int = 20,
    offset: int = 0,
    project_code: str | None = None,
) -> tuple[list[EmailOrder], int]:
    """订单列表。user_id 传 None = 全部用户（管理员看全局）。"""
    stmt = select(EmailOrder)
    if user_id is not None:
        stmt = stmt.where(EmailOrder.user_id == user_id)
    if project_code:
        stmt = stmt.where(EmailOrder.project_code == project_code)
    total = db.scalar(select(func.count()).select_from(stmt.subquery())) or 0
    rows = list(db.scalars(stmt.order_by(EmailOrder.id.desc()).limit(limit).offset(offset)))
    return rows, total


def list_recharge_orders(
    db: Session,
    user_id: int | None = None,
    limit: int = 20,
    offset: int = 0,
) -> tuple[list[RechargeOrder], int]:
    stmt = select(RechargeOrder)
    if user_id is not None:
        stmt = stmt.where(RechargeOrder.user_id == user_id)
    total = db.scalar(select(func.count()).select_from(stmt.subquery())) or 0
    rows = list(db.scalars(stmt.order_by(RechargeOrder.id.desc()).limit(limit).offset(offset)))
    return rows, total
