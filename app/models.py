"""ORM 模型：项目 / 邮箱池 / 邮件记录 / 全局设置。"""
from __future__ import annotations

from datetime import datetime, timezone

from sqlalchemy import DateTime, Index, Integer, String, Text, UniqueConstraint
from sqlalchemy.orm import Mapped, mapped_column

from .database import Base


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
    provider: Mapped[str] = mapped_column(String(32), nullable=False)  # mailnest / cf_temp / tempmail / outlook ...
    # provider 特定参数，如 {"product_code": "chatgpt001", "timeout": 120}
    config_json: Mapped[str] = mapped_column(Text, default="{}")
    enabled: Mapped[int] = mapped_column(Integer, default=1)
    created_at: Mapped[datetime] = mapped_column(DateTime, default=_now)


class EmailAccount(Base):
    """邮箱池：已经持有的邮箱账号（凭证 + 状态）。

    - 临时源（tempmail/cf_temp）：动态创建，不入池；池内仅记录历史。
    - 固定源（outlook / mailnest 购买的独享号）：入池，可 claim。
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
    """邮箱使用记录（防复用）：一个邮箱地址一旦被任何项目用过，即作废。

    - email 唯一：只用一次的邮箱，任何项目（含原项目）都不可再取
    - 释放回池后绑定保留，杜绝跨项目或同项目复用
    """

    __tablename__ = "email_usages"

    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    email: Mapped[str] = mapped_column(String(254), unique=True, index=True, nullable=False)
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
