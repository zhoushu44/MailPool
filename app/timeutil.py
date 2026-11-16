"""时间处理工具：统一时区语义，避免"偏 8 小时"类错误。

本项目的坑：
- Graph API 返回 `2026-11-08T10:23:00Z`（UTC 带 Z）
- IMAP 返回 `Sat, 8 Nov 2026 10:23:00 +0800`（RFC2822 带偏移）
- 数据库 `DateTime` 列存的是 **naive UTC**（SQLite 不保存时区）
- 部分历史接口返回过 `+08:00`，部分返回 `+00:00`

如果直接把 aware 和 naive 相减，会抛 TypeError；
如果按字面值减，会整体偏 8 小时，把"刚到的信"判成"旧信"。
本模块提供唯一入口：`normalize_utc()` 与 `to_naive_utc()`。
"""
from __future__ import annotations

import email.utils as _eu
from datetime import datetime, timezone
from typing import Any

# 服务器时钟与上游可能存在的偏差容忍
CLOCK_SLACK_SECONDS = 60


def normalize_utc(value: Any) -> datetime | None:
    """把任意时间表示解析为 **带时区的 UTC datetime**。

    支持：datetime（naive 视为 UTC / aware 自动换算）、ISO8601（含 Z）、
    RFC2822、unix 时间戳（秒）。无法解析返回 None。
    """
    if value is None or value == "":
        return None
    dt: datetime | None = None
    if isinstance(value, datetime):
        dt = value
    elif isinstance(value, (int, float)) and not isinstance(value, bool):
        try:
            dt = datetime.fromtimestamp(float(value), tz=timezone.utc)
        except (OverflowError, OSError, ValueError):
            return None
    else:
        text = str(value).strip()
        if not text:
            return None
        try:
            dt = datetime.fromisoformat(text.replace("Z", "+00:00").replace("z", "+00:00"))
        except ValueError:
            dt = None
        if dt is None:
            try:
                dt = _eu.parsedate_to_datetime(text)
            except (TypeError, ValueError):
                return None
    if dt is None:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def to_naive_utc(value: Any) -> datetime | None:
    """转成数据库可存的 naive UTC。"""
    dt = normalize_utc(value)
    return dt.replace(tzinfo=None) if dt else None


def utc_now() -> datetime:
    """当前 UTC（aware）。"""
    return datetime.now(timezone.utc)


def utc_iso(value: Any) -> str:
    """统一输出 ISO8601 字符串（**始终带时区**，UTC）。

    接口里所有时间字段都走这个函数，避免同一部署先后给出
    `+08:00` 和 `+00:00`、以及"有的带时区有的不带"，导致调用方相减偏 8 小时。
    无法解析的时间返回空字符串。
    """
    dt = normalize_utc(value)
    return dt.isoformat() if dt else ""


def is_after(value: Any, reference: Any, slack_seconds: float = CLOCK_SLACK_SECONDS) -> bool:
    """value 是否晚于 reference（允许 slack 秒时钟误差）。

    任一无法解析 → False（保守：宁可当旧数据）。
    """
    a = normalize_utc(value)
    b = normalize_utc(reference)
    if a is None or b is None:
        return False
    return a.timestamp() >= b.timestamp() - float(slack_seconds)
