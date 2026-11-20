"""ORM 模型：项目 / 邮箱池 / 邮件记录 / 全局设置 / 计费（用户·余额·订单·充值卡）。"""
from __future__ import annotations

from datetime import datetime, timezone

from sqlalchemy import BigInteger, DateTime, Index, Integer, String, Text, UniqueConstraint
from sqlalchemy.orm import Mapped, mapped_column

from .database import Base

# ── 金额单位 ────────────────────────────────────────────────────
# 全部金额以 **整数厘**（1 元 = 1000 厘）存储。
# 为什么不用 float / Decimal：
#   - float 累加会产生 0.018000000000000002 这类误差，余额对不上账；
#   - SQLite 的 Numeric 会退化成 float，等于没解决；
#   - 整数厘是最稳的：0.018 元 → 18 厘，1.00 元 → 1000 厘，精度足够（3 位小数），
#     且加法零误差。迈巢的 0.018/0.038 单价正好是 3 位小数。
MONEY_SCALE = 1000


def _now() -> datetime:
    return datetime.now(timezone.utc)


class Project(Base):
    """一个"项目"= 一个 project_code + 指定 provider 及其参数。

    类似 MailNest 的 product_code 概念：调用方用 project_code 取号/取码，
    系统据此路由到对应 provider。
    """

    __tablename__ = "projects"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    code: Mapped[str] = mapped_column(String(64), unique=True, index=True, nullable=False)
    name: Mapped[str] = mapped_column(String(128), default="")
    provider: Mapped[str] = mapped_column(String(32), nullable=False)  # 当前只有 outlook 号池
    # provider 特定参数，如 {"product_code": "chatgpt001", "timeout": 120}
    config_json: Mapped[str] = mapped_column(Text, default="{}")
    enabled: Mapped[int] = mapped_column(Integer, default=1)
    created_at: Mapped[datetime] = mapped_column(DateTime, default=_now)


class EmailAccount(Base):
    """邮箱池：已经持有的邮箱账号（凭证 + 状态），固定源入池可 claim。
    （临时源已移除，池里只剩 outlook 号。）
    """

    __tablename__ = "email_accounts"
    __table_args__ = (UniqueConstraint("email", name="uq_email"),)

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    email: Mapped[str] = mapped_column(String(254), index=True, nullable=False)
    provider: Mapped[str] = mapped_column(String(32), nullable=False)
    # 凭证 JSON：如 outlook 四段式 {password, client_id, refresh_token}
    account_json: Mapped[str] = mapped_column(Text, default="{}")
    # 旧 status 保留用于读取已有数据库；新逻辑使用两个独立状态字段。
    status: Mapped[str] = mapped_column(String(16), default="available")
    pool_status: Mapped[str] = mapped_column(String(16), default="available")
    health_status: Mapped[str] = mapped_column(String(16), default="unknown")
    health_reason: Mapped[str] = mapped_column(String(32), default="")
    claimed_at: Mapped[datetime | None] = mapped_column(DateTime, nullable=True)
    remark: Mapped[str] = mapped_column(String(255), default="")
    # 标签（逗号分隔，如 "养号期,接码,备份"）
    tags: Mapped[str] = mapped_column(String(255), default="")
    # 账号级代理，如 "http://127.0.0.1:7890" / "socks5://user:pass@host:port"
    proxy: Mapped[str] = mapped_column(String(255), default="")
    last_check_at: Mapped[datetime | None] = mapped_column(DateTime, nullable=True)
    check_result: Mapped[str] = mapped_column(String(255), default="")
    # 连续失败计数：健康扫描/心跳保活失败累加，成功清零；达到阈值自动降级（unhealthy→dead）
    fail_count: Mapped[int] = mapped_column(Integer, default=0)
    created_at: Mapped[datetime] = mapped_column(DateTime, default=_now)


class MailRecord(Base):
    """收件记录：缓存收到的邮件 + 解析出的验证码。"""

    __tablename__ = "mail_records"
    __table_args__ = (
        Index("ix_mail_records_email_message_id", "email", "message_id"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    email: Mapped[str] = mapped_column(String(254), index=True, nullable=False)
    project_code: Mapped[str] = mapped_column(String(64), index=True, default="")
    message_id: Mapped[str] = mapped_column(String(512), default="")
    provider: Mapped[str] = mapped_column(String(32), default="")
    folder: Mapped[str] = mapped_column(String(64), default="")
    sender: Mapped[str] = mapped_column(String(254), default="")
    subject: Mapped[str] = mapped_column(String(512), default="")
    body_preview: Mapped[str] = mapped_column(Text, default="")
    body: Mapped[str] = mapped_column(Text, default="")
    code: Mapped[str] = mapped_column(String(32), default="")
    code_source: Mapped[str] = mapped_column(String(16), default="")
    received_at: Mapped[datetime] = mapped_column(DateTime, index=True, default=_now)


class EmailUsage(Base):
    """邮箱使用记录（按项目防复用）。

    - (email, project_code) 唯一：同一个邮箱**同一个项目**只能用一次；
      其他项目仍可取用该邮箱（A 项目用过，B 项目还能用）
    - 释放回池后绑定保留，本项目永远不可再取
    """

    __tablename__ = "email_usages"
    __table_args__ = (
        UniqueConstraint("email", "project_code", name="uq_email_usage_project"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    email: Mapped[str] = mapped_column(String(254), index=True, nullable=False)
    project_code: Mapped[str] = mapped_column(String(64), index=True, nullable=False)
    first_used_at: Mapped[datetime] = mapped_column(DateTime, default=_now)


class MailBaseline(Base):
    """取号瞬间的收件箱基线（防"旧验证码"误判）。

    邮箱池里的账号都是回收号，收件箱往往留着几天前的旧信。
    claim 时把当时所有 message_id 记下来，receive 只认不在基线里的新邮件；
    同时记录 baseline_at，超过 BASELINE_MAX_AGE 的旧邮件一律不认（双判据）。

    email 唯一：每次 claim 覆盖写即可。
    """

    __tablename__ = "mail_baselines"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    email: Mapped[str] = mapped_column(String(254), unique=True, index=True, nullable=False)
    project_code: Mapped[str] = mapped_column(String(64), index=True, default="")
    # 基线抓取时刻（UTC，aware 转 naive 存）
    baseline_at: Mapped[datetime] = mapped_column(DateTime, default=_now)
    # 基线里的 message_id，逗号分隔
    message_ids: Mapped[str] = mapped_column(Text, default="")
    # 基线里最新一封的时间（UTC），用于时间窗口对比
    latest_received_at: Mapped[datetime | None] = mapped_column(DateTime, nullable=True)
    # 抓基线时是否成功（失败则 receive 走"只认时间窗口"的降级策略）
    captured: Mapped[int] = mapped_column(Integer, default=1)


class Setting(Base):
    """全局设置（键值）：API Keys、provider 全局配置等。"""

    __tablename__ = "settings"

    key: Mapped[str] = mapped_column(String(64), primary_key=True)
    value: Mapped[str] = mapped_column(Text, default="")
    updated_at: Mapped[datetime] = mapped_column(DateTime, default=_now, onupdate=_now)


class TokenRefreshLog(Base):
    """Token 全量/定时刷新历史记录。

    每次全量刷新任务（手动或定时触发）完成/失败后写入一行，
    前端可查看历史执行情况（成功/失败/跳过数量、耗时、触发方式）。
    """

    __tablename__ = "token_refresh_logs"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    trigger: Mapped[str] = mapped_column(String(16), default="manual")  # manual / schedule
    provider: Mapped[str] = mapped_column(String(32), default="outlook")
    status: Mapped[str] = mapped_column(String(16), default="finished")  # finished / failed
    total: Mapped[int] = mapped_column(Integer, default=0)
    success: Mapped[int] = mapped_column(Integer, default=0)
    failed: Mapped[int] = mapped_column(Integer, default=0)
    skipped: Mapped[int] = mapped_column(Integer, default=0)
    duration: Mapped[float] = mapped_column(default=0.0)  # 秒
    detail: Mapped[str] = mapped_column(Text, default="")  # 每账号结果 JSON 或错误摘要
    created_at: Mapped[datetime] = mapped_column(DateTime, index=True, default=_now)


# ────────────────────────────────────────────────────────────────
#  计费体系（用户 / 余额 / 订单 / 充值卡）
#  设计取自 new-api（充值）与 MailNest（扣费）两套：
#    充值 → new-api：trade_no 唯一 + 事务 + 状态机幂等
#    扣费 → MailNest：hold → release → charge 三段式（收到验证码才真扣）
# ────────────────────────────────────────────────────────────────

# 用户角色
ROLE_ADMIN = 1
ROLE_USER = 2

# 邮箱订单计费状态（对齐 MailNest billing_status）
BILLING_PENDING = "pending"   # 已取号，冻结中，未扣费
BILLING_CHARGED = "charged"   # 已收到验证码，真实扣费
BILLING_FREE = "free"         # 超时释放或免单，未扣费

# 余额流水类型（对齐 MailNest log_type）
LOG_RECHARGE = "recharge"   # 充值到账
LOG_HOLD = "hold"           # 取号冻结
LOG_RELEASE = "release"     # 释放解冻
LOG_CHARGE = "charge"       # 收码实扣
LOG_REDEEM = "redeem"       # 充值卡兑换
LOG_AWARD = "award"         # 充值赠送
LOG_ADJUST = "adjust"       # 管理员调整

# 充值订单状态（对齐 new-api TopUpStatus）
TOPUP_PENDING = "pending"
TOPUP_SUCCESS = "success"
TOPUP_FAILED = "failed"


class User(Base):
    """平台用户（客户）。

    余额语义对齐 MailNest：
        balance        总余额（可用 + 冻结）
        frozen_balance 冻结中（已取号未收码）
        available      = balance - frozen_balance
    金额单位：整数厘（1 元 = 1000 厘），见 MONEY_SCALE。
    """

    __tablename__ = "users"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    username: Mapped[str] = mapped_column(String(64), unique=True, index=True, nullable=False)
    password_hash: Mapped[str] = mapped_column(String(255), default="")
    role: Mapped[int] = mapped_column(Integer, default=ROLE_USER)
    status: Mapped[int] = mapped_column(Integer, default=1)   # 1 启用 / 0 禁用
    email: Mapped[str] = mapped_column(String(128), default="", index=True)
    balance: Mapped[int] = mapped_column(BigInteger, default=0)        # 厘
    frozen_balance: Mapped[int] = mapped_column(BigInteger, default=0)  # 厘
    total_recharged: Mapped[int] = mapped_column(BigInteger, default=0)  # 累计充值（厘）
    total_charged: Mapped[int] = mapped_column(BigInteger, default=0)    # 累计消耗（厘）
    created_at: Mapped[datetime] = mapped_column(DateTime, default=_now)


class ApiKey(Base):
    """API Key（绑定用户）。

    老版本把 key 存在 settings.api_keys（一个 JSON 数组），所有 key 共享同一个
    匿名身份 —— 扣费扣不到人头上。新版每个 key 归属一个 user_id。
    """

    __tablename__ = "api_keys"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    user_id: Mapped[int] = mapped_column(Integer, index=True, default=0, nullable=False)
    key: Mapped[str] = mapped_column(String(128), unique=True, index=True, nullable=False)
    name: Mapped[str] = mapped_column(String(64), default="")
    enabled: Mapped[int] = mapped_column(Integer, default=1)
    created_at: Mapped[datetime] = mapped_column(DateTime, default=_now)
    last_used_at: Mapped[datetime | None] = mapped_column(DateTime, nullable=True)


class RechargeOrder(Base):
    """充值订单（对齐 new-api 的 TopUp 表）。

    - trade_no 唯一索引 → 回调幂等的第一道防线
    - money 是**实际支付金额**（厘），amount 是**到账额度**（厘）
    - pay_provider：epay（易支付）/ redeem（充值卡）/ admin（人工）
    """

    __tablename__ = "recharge_orders"
    __table_args__ = (
        UniqueConstraint("trade_no", name="uq_recharge_trade_no"),
        Index("ix_recharge_orders_user_status", "user_id", "status"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    user_id: Mapped[int] = mapped_column(Integer, index=True, default=0, nullable=False)
    trade_no: Mapped[str] = mapped_column(String(64), nullable=False)
    amount: Mapped[int] = mapped_column(BigInteger, default=0)        # 到账额度（厘）
    money: Mapped[int] = mapped_column(BigInteger, default=0)        # 实付金额（厘）
    pay_type: Mapped[str] = mapped_column(String(32), default="")     # alipay / wxpay
    pay_provider: Mapped[str] = mapped_column(String(32), default="epay")
    status: Mapped[str] = mapped_column(String(16), default=TOPUP_PENDING)
    client_ip: Mapped[str] = mapped_column(String(64), default="")
    remark: Mapped[str] = mapped_column(String(255), default="")
    created_at: Mapped[datetime] = mapped_column(DateTime, index=True, default=_now)
    completed_at: Mapped[datetime | None] = mapped_column(DateTime, nullable=True)


class BalanceLog(Base):
    """余额流水（对齐 MailNest 的 balance-logs）。

    每一条都带变动前/后的余额与冻结额，页面才能对得上账。
    """

    __tablename__ = "balance_logs"
    __table_args__ = (
        Index("ix_balance_logs_user_created", "user_id", "created_at"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    user_id: Mapped[int] = mapped_column(Integer, index=True, default=0, nullable=False)
    log_type: Mapped[str] = mapped_column(String(16), default=LOG_ADJUST)
    amount: Mapped[int] = mapped_column(BigInteger, default=0)          # 本次变动（厘，负数=减少）
    balance_before: Mapped[int] = mapped_column(BigInteger, default=0)
    balance_after: Mapped[int] = mapped_column(BigInteger, default=0)
    frozen_before: Mapped[int] = mapped_column(BigInteger, default=0)
    frozen_after: Mapped[int] = mapped_column(BigInteger, default=0)
    remark: Mapped[str] = mapped_column(String(255), default="")
    created_at: Mapped[datetime] = mapped_column(DateTime, index=True, default=_now)


class RedeemCard(Base):
    """充值卡（卡密）。

    走 new-api 的兑换码思路：不依赖任何支付通道也能卖钱，
    管理员批量生成卡密，用户自己兑换到账。
    """

    __tablename__ = "redeem_cards"
    __table_args__ = (
        UniqueConstraint("card_key", name="uq_redeem_card_key"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    batch_id: Mapped[str] = mapped_column(String(64), default="", index=True)
    card_key: Mapped[str] = mapped_column(String(64), nullable=False)
    amount: Mapped[int] = mapped_column(BigInteger, default=0)   # 面额（厘）
    status: Mapped[int] = mapped_column(Integer, default=1)       # 1 未使用 / 0 已使用
    used_by: Mapped[int] = mapped_column(Integer, default=0)      # user_id
    used_at: Mapped[datetime | None] = mapped_column(DateTime, nullable=True)
    created_at: Mapped[datetime] = mapped_column(DateTime, default=_now)


class EmailOrder(Base):
    """邮箱取号订单（计费侧）。

    一次取号 = 一条记录，跟踪 hold → charge / free 的状态流转。
    """

    __tablename__ = "email_orders"
    __table_args__ = (
        # 同一邮箱可被多个项目各买一次，但同一项目同一邮箱只有一单
        UniqueConstraint("email", "project_code", name="uq_email_order_project"),
        Index("ix_email_orders_user_billing", "user_id", "billing_status"),
    )

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    user_id: Mapped[int] = mapped_column(Integer, index=True, default=0, nullable=False)
    email: Mapped[str] = mapped_column(String(254), nullable=False)
    project_code: Mapped[str] = mapped_column(String(64), default="", index=True)
    provider: Mapped[str] = mapped_column(String(32), default="")
    price: Mapped[int] = mapped_column(BigInteger, default=0)       # 单价（厘）
    billing_status: Mapped[str] = mapped_column(String(16), default=BILLING_PENDING)
    hold_amount: Mapped[int] = mapped_column(BigInteger, default=0)  # 冻结额（厘）
    charged_amount: Mapped[int] = mapped_column(BigInteger, default=0)  # 实扣额（厘）
    created_at: Mapped[datetime] = mapped_column(DateTime, index=True, default=_now)
    charged_at: Mapped[datetime | None] = mapped_column(DateTime, nullable=True)
    released_at: Mapped[datetime | None] = mapped_column(DateTime, nullable=True)
