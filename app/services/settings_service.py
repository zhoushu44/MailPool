"""全局设置服务：provider 全局配置、API Keys 的读写。

存储方式：Setting 表，键为
    provider_config.<provider_id>   -> provider 全局配置 JSON
    api_keys                        -> API Key 列表 JSON
"""
from __future__ import annotations

import json
import secrets
from datetime import datetime, timezone
from typing import Any

from sqlalchemy.orm import Session

from ..models import Setting


def _get_value(db: Session, key: str, default: Any = "") -> Any:
    row = db.get(Setting, key)
    return row.value if row else default


def _set_value(db: Session, key: str, value: str) -> None:
    row = db.get(Setting, key)
    if row:
        row.value = value
    else:
        db.add(Setting(key=key, value=value))
    db.commit()


def get_provider_config(db: Session, provider_id: str) -> dict[str, Any]:
    raw = _get_value(db, f"provider_config.{provider_id}", "{}")
    try:
        data = json.loads(raw)
        return data if isinstance(data, dict) else {}
    except (json.JSONDecodeError, TypeError):
        return {}


def save_provider_config(db: Session, provider_id: str, config: dict[str, Any]) -> None:
    _set_value(db, f"provider_config.{provider_id}", json.dumps(config, ensure_ascii=False))


def get_api_keys(db: Session) -> list[str]:
    raw = _get_value(db, "api_keys", "[]")
    try:
        data = json.loads(raw)
        return [str(k) for k in data] if isinstance(data, list) else []
    except (json.JSONDecodeError, TypeError):
        return []


def generate_api_key() -> str:
    return f"ak_{secrets.token_hex(16)}"


def add_api_key(db: Session, key: str) -> list[str]:
    keys = get_api_keys(db)
    if key not in keys:
        keys.append(key)
        _set_value(db, "api_keys", json.dumps(keys))
    return keys


def remove_api_key(db: Session, key: str) -> list[str]:
    keys = [k for k in get_api_keys(db) if k != key]
    _set_value(db, "api_keys", json.dumps(keys))
    return keys


# ── 绑定用户的 API Key（计费版）───────────────────────────────


def create_user_api_key(db: Session, user_id: int, name: str = "") -> str:
    """给用户签发一个绑定身份的 API Key。

    绑了 user_id 的 key 才能计费；老的裸 key（settings.api_keys）视为匿名。
    """
    from ..models import ApiKey

    key = f"ak_{secrets.token_hex(16)}"
    db.add(ApiKey(user_id=user_id, key=key, name=name[:64], enabled=1))
    db.commit()
    return key


def list_user_api_keys(db: Session, user_id: int | None = None) -> list[Any]:
    from sqlalchemy import select

    from ..models import ApiKey

    stmt = select(ApiKey)
    if user_id is not None:
        stmt = stmt.where(ApiKey.user_id == user_id)
    return list(db.scalars(stmt.order_by(ApiKey.id.asc())))


def delete_user_api_key(db: Session, key_id: int) -> bool:
    from ..models import ApiKey

    row = db.get(ApiKey, key_id)
    if row is None:
        return False
    db.delete(row)
    db.commit()
    return True


# ────────────────────────────────────────────────────────────────
#  计费 / 支付配置
# ────────────────────────────────────────────────────────────────

# 支付通道配置（抄 new-api 的 Option 表思路：一切可调项存 settings 键值表）
BILLING_DEFAULTS: dict[str, str] = {
    # 计费总开关：关闭时保持旧的"免费发放"行为
    "enabled": "false",
    # 默认单价（元），项目 config_json 里的 price 优先
    "default_price": "0.018",
    # 最小充值金额（元）
    "min_topup": "1.00",
    # 单次最大充值金额（元）
    "max_topup": "1000.00",
    # 临时邮箱冻结超时（秒），超时自动释放不扣费
    "hold_timeout": "1200",
    # 充值赠送比例（百分比，0 = 不赠送）
    "award_percent": "0",
    # 易支付网关地址（空 = 未配置，充值页面只显示充值卡）
    "epay_url": "",
    "epay_pid": "",
    "epay_key": "",
    # 本站对外地址，用于拼支付回调地址
    "site_url": "",
    # 充值卡默认面额（元），批量生成时用
    "card_default_amount": "10.00",
}


def get_billing_config(db: Session) -> dict[str, str]:
    return {k: _get_value(db, f"billing.{k}", v) for k, v in BILLING_DEFAULTS.items()}


def save_billing_config(db: Session, config: dict[str, Any]) -> dict[str, str]:
    for key in BILLING_DEFAULTS:
        if key in config:
            _set_value(db, f"billing.{key}", str(config[key]))
    return get_billing_config(db)


# ────────────────────────────────────────────────────────────────
#  心跳保活配置
# ────────────────────────────────────────────────────────────────

HEARTBEAT_DEFAULTS = {
    "heartbeat_enabled": "false",
    "heartbeat_mode": "silent",
    "heartbeat_hour": "9",
    "heartbeat_minute": "0",
}


def get_heartbeat_config(db: Session) -> dict[str, Any]:
    return {k: _get_value(db, k, v) for k, v in HEARTBEAT_DEFAULTS.items()}


def save_heartbeat_config(db: Session, config: dict[str, Any]) -> dict[str, Any]:
    for k in HEARTBEAT_DEFAULTS:
        if k in config:
            _set_value(db, k, str(config[k]))
    return get_heartbeat_config(db)


def get_heartbeat_last_run(db: Session) -> dict[str, Any]:
    raw = _get_value(db, "heartbeat_last_summary", "{}")
    try:
        data = json.loads(raw)
        return data if isinstance(data, dict) else {}
    except (json.JSONDecodeError, TypeError):
        return {}


def set_heartbeat_last_run(db: Session, summary: dict[str, Any]) -> None:
    _set_value(db, "heartbeat_last_run", datetime.now(timezone.utc).isoformat())
    _set_value(db, "heartbeat_last_summary", json.dumps(summary, ensure_ascii=False))


# ────────────────────────────────────────────────────────────────
#  健康扫描配置
# ────────────────────────────────────────────────────────────────

HEALTH_CHECK_DEFAULTS = {
    # 默认开启：微软会静默把号打成 service abuse mode（token 全废），
    # 关掉健康扫描等于让死号一直躺在池子里发给客户。
    "health_check_enabled": "true",
    "health_check_hour": "8",
    "health_check_minute": "0",
}


def get_health_check_config(db: Session) -> dict[str, Any]:
    return {k: _get_value(db, k, v) for k, v in HEALTH_CHECK_DEFAULTS.items()}


def save_health_check_config(db: Session, config: dict[str, Any]) -> dict[str, Any]:
    for k in HEALTH_CHECK_DEFAULTS:
        if k in config:
            _set_value(db, k, str(config[k]))
    return get_health_check_config(db)


def get_health_check_last_run(db: Session) -> dict[str, Any]:
    raw = _get_value(db, "health_check_last_summary", "{}")
    try:
        data = json.loads(raw)
        return data if isinstance(data, dict) else {}
    except (json.JSONDecodeError, TypeError):
        return {}


def set_health_check_last_run(db: Session, summary: dict[str, Any]) -> None:
    _set_value(db, "health_check_last_run", datetime.now(timezone.utc).isoformat())
    _set_value(db, "health_check_last_summary", json.dumps(summary, ensure_ascii=False))


# ────────────────────────────────────────────────────────────────
#  定时全量刷新 Token 配置
# ────────────────────────────────────────────────────────────────

REFRESH_DEFAULTS = {
    "refresh_enabled": "false",
    "refresh_hour": "3",
    "refresh_minute": "0",
}


def get_refresh_config(db: Session) -> dict[str, Any]:
    return {k: _get_value(db, k, v) for k, v in REFRESH_DEFAULTS.items()}


def save_refresh_config(db: Session, config: dict[str, Any]) -> dict[str, Any]:
    for k in REFRESH_DEFAULTS:
        if k in config:
            _set_value(db, k, str(config[k]))
    return get_refresh_config(db)


def get_refresh_last_run(db: Session) -> dict[str, Any]:
    raw = _get_value(db, "refresh_last_summary", "{}")
    try:
        data = json.loads(raw)
        return data if isinstance(data, dict) else {}
    except (json.JSONDecodeError, TypeError):
        return {}


def set_refresh_last_run(db: Session, summary: dict[str, Any]) -> None:
    _set_value(db, "refresh_last_run", datetime.now(timezone.utc).isoformat())
    _set_value(db, "refresh_last_summary", json.dumps(summary, ensure_ascii=False))
