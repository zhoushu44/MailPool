"""MailNest 临时邮箱 Provider：按 product_code 购买，轮询 receive 取验证码。

对齐 RegiForge mailsys/mailnest，独立实现。API 文档：https://mailnest.top
"""
from __future__ import annotations

import asyncio
import json
import time
from typing import Any

import requests
import urllib3

from .base import ConfigField, EmailProvider, ProviderError, register

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

DEFAULT_BASE_URL = "https://mailnest.top"
SUCCESS_CODE = "00000"


def _parse_bool(value: Any, default: bool = True) -> bool:
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() not in {"0", "false", "no", "off"}


@register
class MailnestEmailProvider(EmailProvider):
    id = "mailnest"
    name = "MailNest 临时邮箱"
    ephemeral = True

    config_fields = [
        ConfigField(key="api_key", label="API Key", type="password", required=True,
                    placeholder="ak_xxx", help="MailNest 控制台的 API Key"),
        ConfigField(key="base_url", label="接口地址", default=DEFAULT_BASE_URL,
                    help="默认 https://mailnest.top"),
        ConfigField(key="product_code", label="产品编码", required=True,
                    placeholder="chatgpt001", help="项目对应的 product_code"),
        ConfigField(key="proxy", label="代理", placeholder="socks5://user:pass@host:port",
                    help="可选；多行取第一行"),
        ConfigField(key="poll_interval", label="轮询间隔(秒)", type="number", default="3"),
        ConfigField(key="auto_release", label="超时自动释放", default="1"),
    ]

    def __init__(self) -> None:
        super().__init__()
        self._active: dict[str, dict[str, Any]] = {}

    def configure(self, config: dict[str, Any] | None = None) -> None:
        super().configure(config)
        self._active = {}

    # ── 配置 ──────────────────────────────────────────────
    def _base_url(self) -> str:
        return str(self._config.get("base_url") or DEFAULT_BASE_URL).strip().rstrip("/")

    def _api_key(self) -> str:
        return str(self._config.get("api_key") or "").strip()

    def _product_code(self) -> str:
        return str(self._config.get("product_code") or "").strip()

    def _poll_interval(self) -> float:
        try:
            return max(1.0, float(self._config.get("poll_interval") or 3))
        except (TypeError, ValueError):
            return 3.0

    def _auto_release(self) -> bool:
        return _parse_bool(self._config.get("auto_release"), True)

    # ── HTTP ──────────────────────────────────────────────
    def _headers(self) -> dict[str, str]:
        key = self._api_key()
        if not key:
            raise ProviderError("MailNest 未配置 api_key")
        return {
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        }

    def _proxies(self) -> dict[str, str] | None:
        lines = [l.strip() for l in str(self._config.get("proxy") or "").split("\n") if l.strip()]
        if not lines:
            return None
        return {"http": lines[0], "https": lines[0]}

    def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: dict[str, Any] | None = None,
        auth: bool = True,
        timeout: float = 60,
        retries: int = 5,
    ) -> Any:
        url = f"{self._base_url()}{path}"
        headers = self._headers() if auth else {"Accept": "application/json"}
        proxies = self._proxies()
        last_error: Exception | None = None
        for attempt in range(1, max(1, retries) + 1):
            try:
                with requests.Session() as session:
                    if proxies is None:
                        session.trust_env = False
                    resp = session.request(
                        method, url, headers=headers, json=json_body,
                        timeout=timeout, proxies=proxies, verify=False,
                    )
            except (requests.exceptions.SSLError, requests.exceptions.ConnectionError) as exc:
                last_error = exc
                if attempt >= retries:
                    break
                time.sleep(min(2.0 * attempt, 10.0))
                continue
            if resp.status_code == 401:
                raise ProviderError("MailNest API Key 无效或已失效（HTTP 401）", fatal=True)
            try:
                payload = resp.json()
            except Exception as exc:
                raise ProviderError(
                    f"MailNest 响应非 JSON（HTTP {resp.status_code}）: {resp.text[:200]}"
                ) from exc
            if not resp.ok:
                detail = payload.get("detail") or payload.get("msg") or payload
                raise ProviderError(f"MailNest HTTP {resp.status_code}: {detail}")
            if isinstance(payload, dict) and "code" in payload:
                code = str(payload.get("code") or "")
                if code != SUCCESS_CODE:
                    raise ProviderError(f"MailNest 业务错误 {code}: {payload.get('msg') or code}")
                return payload.get("data")
            return payload
        raise ProviderError(f"MailNest 网络请求失败（已重试 {retries} 次）: {last_error}")

    # ── 核心 ──────────────────────────────────────────────
    def _buy_temporary(self, product_code: str) -> dict[str, Any]:
        data = self._request(
            "POST", "/api/v1/email/temporary/buy",
            json_body={"project_code": product_code, "count": 1},
        )
        if isinstance(data, list) and data:
            item = data[0]
            if isinstance(item, dict) and item.get("email"):
                return item
        raise ProviderError(f"MailNest 购买临时邮箱返回异常: {data!r}")

    def generate_address(self) -> str:
        code = self._product_code()
        if not code:
            raise ProviderError("MailNest 未配置 product_code")
        item = self._buy_temporary(code)
        email = str(item.get("email") or "").strip()
        if not email:
            raise ProviderError(f"MailNest 购买成功但未返回邮箱: {item!r}")
        self._active[email.lower()] = {
            "email": email,
            "order_id": item.get("id"),
            "product_code": item.get("project_code") or code,
            "started_at": time.time(),
            "raw": item,
        }
        return email

    def _receive_once(self, email: str) -> list[dict[str, Any]]:
        try:
            data = self._request("POST", "/api/v1/email/receive", json_body={"email": email})
        except ProviderError as exc:
            if "D0005" in str(exc) or "取件失败" in str(exc):
                return []
            raise
        if isinstance(data, list):
            return [item for item in data if isinstance(item, dict)]
        if isinstance(data, dict):
            return [data]
        return []

    def release(self, email: str) -> None:
        address = str(email or "").strip()
        if not address:
            return
        try:
            self._request("POST", "/api/v1/email/release", json_body={"email": address})
        except ProviderError as exc:
            if "D0004" in str(exc):
                return
            raise
        finally:
            self._active.pop(address.lower(), None)

    def _wait_code_sync(
        self, email: str, timeout: float,
    ) -> str | None:
        address = str(email or "").strip()
        if not address:
            raise ProviderError("等待验证码时邮箱地址为空")
        deadline = time.time() + timeout
        interval = self._poll_interval()
        last_error = ""
        while time.time() < deadline:
            try:
                mails = self._receive_once(address)
                for mail in mails:
                    code = self.extract_from_mail(mail)
                    if code:
                        self._active.pop(address.lower(), None)
                        return code
            except Exception as exc:
                last_error = str(exc)
            time.sleep(interval)
        if self._auto_release():
            try:
                self.release(address)
            except Exception:
                pass
        if last_error:
            raise ProviderError(f"等待验证码超时，最后错误: {last_error}")
        return None

    async def wait_code(
        self, email: str, *, timeout: float = 120, project: dict[str, Any] | None = None
    ) -> str | None:
        return await asyncio.to_thread(self._wait_code_sync, email, timeout)

    def self_test(self) -> dict[str, Any]:
        try:
            data = self._request("GET", "/api/v1/balance")
            if isinstance(data, dict):
                return {"ok": True, "message": f"连接成功，余额信息: {json.dumps(data, ensure_ascii=False)[:200]}"}
            return {"ok": True, "message": "连接成功"}
        except Exception as exc:
            return {"ok": False, "message": str(exc)}
