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
    "health_check_enabled": "false",
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
