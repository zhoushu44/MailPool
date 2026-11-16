"""TempMail.lol 临时邮箱 Provider：创建 inbox → 轮询收件 → 提取验证码。

API:
    POST /v2/inbox/create  -> { address, token }  (201)
    GET  /v2/inbox?token=  -> { emails[], expired } (200)
"""
from __future__ import annotations

import asyncio
import re
import time
from typing import Any

import requests
import urllib3

from .base import ConfigField, EmailProvider, ProviderError, register

urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

BASE_URL = "https://api.tempmail.lol"

# xAI 风格验证码（LSQ-OPU / ABC123）+ 通用数字码
_CODE_PATTERNS = (
    re.compile(r"(?<![A-Z0-9])([A-Z0-9]{3}-[A-Z0-9]{3})(?![A-Z0-9])"),
    re.compile(r"(?<![A-Z0-9])([A-Z0-9]{6})(?![A-Z0-9])"),
    re.compile(r"(?i)(?:code|otp|验证码|verification|verify)\s*[:：]?\s*([A-Z0-9]{3}-[A-Z0-9]{3})"),
    re.compile(r"(?i)(?:code|otp|验证码|verification|verify)\s*[:：]?\s*([A-Z0-9]{6})"),
)


def _extract_code(text: str) -> str | None:
    if not text:
        return None
    for pat in _CODE_PATTERNS:
        m = pat.search(text)
        if m:
            raw = m.group(1) if m.groups() else m.group(0)
            if raw.replace("-", "").isdigit():
                continue
            return raw.upper()
    return None


@register
class TempmailEmailProvider(EmailProvider):
    id = "tempmail"
    name = "TempMail.lol 临时邮箱"
    ephemeral = True

    config_fields = [
        ConfigField(key="api_key", label="API Key", type="password", required=True,
                    placeholder="temp_xxx", help="TempMail.lol API Key"),
        ConfigField(key="base_url", label="接口地址", default=BASE_URL),
        ConfigField(key="proxy", label="代理", placeholder="socks5://user:pass@host:port",
                    help="可选；多行取第一行"),
        ConfigField(key="poll_interval", label="轮询间隔(秒)", type="number", default="3"),
    ]

    def __init__(self) -> None:
        super().__init__()
        self._active: dict[str, dict[str, Any]] = {}

    def configure(self, config: dict[str, Any] | None = None) -> None:
        super().configure(config)
        self._active = {}

    def _fallback_extract_from_text(self, text: str) -> str:
        """保留 Tempmail 自带提取（xAI 风格如 LSQ-OPU），正则未命中时兜底。"""
        return _extract_code(text) or ""

    def _api_key(self) -> str:
        return str(self._config.get("api_key") or "").strip()

    def _base_url(self) -> str:
        return str(self._config.get("base_url") or BASE_URL).strip().rstrip("/")

    def _headers(self) -> dict[str, str]:
        key = self._api_key()
        if not key:
            raise ProviderError("TempMail.lol 未配置 api_key")
        return {"Authorization": f"Bearer {key}"}

    def _proxies(self) -> dict[str, str] | None:
        lines = [l.strip() for l in str(self._config.get("proxy") or "").split("\n") if l.strip()]
        if not lines:
            return None
        return {"http": lines[0], "https": lines[0]}

    def _poll_interval(self) -> float:
        try:
            return max(1.0, float(self._config.get("poll_interval") or 3))
        except (TypeError, ValueError):
            return 3.0

    def generate_address(self) -> str:
        key = self._api_key()
        if not key:
            raise ProviderError("TempMail.lol 未配置 api_key")
        base = self._base_url()
        proxies = self._proxies()
        last_err: Exception | None = None
        for attempt in range(3):
            try:
                resp = requests.post(
                    f"{base}/v2/inbox/create",
                    headers=self._headers(),
                    json={"prefix": "user"},
                    timeout=15,
                    proxies=proxies,
                )
                if resp.status_code != 201:
                    last_err = ProviderError(f"HTTP {resp.status_code}: {resp.text[:200]}")
                    if attempt < 2:
                        time.sleep(3)
                        continue
                    raise last_err
                data = resp.json()
                address = data.get("address", "")
                token = data.get("token", "")
                if not address:
                    raise ProviderError(f"TempMail.lol 创建邮箱返回空地址：{data!r}")
                self._active[address.lower()] = {
                    "address": address, "token": token, "started_at": time.time(),
                }
                return address
            except requests.exceptions.ProxyError as exc:
                last_err = exc
                if attempt < 2:
                    time.sleep(3)
                    continue
                raise ProviderError(f"TempMail.lol 代理连接失败（3次重试后）: {exc}")
            except requests.exceptions.RequestException as exc:
                last_err = exc
                if attempt < 2:
                    time.sleep(3)
                    continue
                raise ProviderError(f"TempMail.lol 请求失败（3次重试后）: {exc}")
        raise ProviderError(f"TempMail.lol 创建邮箱失败: {last_err}")

    def _get_emails_sync(self, email: str) -> list[dict]:
        info = self._active.get(email.lower())
        if not info:
            raise ProviderError(f"TempMail.lol 邮箱 {email} 未创建或已过期")
        try:
            resp = requests.get(
                f"{self._base_url()}/v2/inbox",
                headers=self._headers(),
                params={"token": info["token"]},
                timeout=15,
                proxies=self._proxies(),
            )
            if resp.status_code != 200:
                return []
            data = resp.json()
            return data.get("emails", [])
        except Exception:
            return []

    def _wait_code_sync(self, email: str, timeout: float) -> str | None:
        deadline = time.time() + timeout
        interval = self._poll_interval()
        seen_ids: set[str] = set()
        while time.time() < deadline:
            for mail in self._get_emails_sync(email):
                eid = f"{mail.get('from','')}:{mail.get('subject','')}:{mail.get('date','')}"
                if eid in seen_ids:
                    continue
                seen_ids.add(eid)
                text = " ".join([
                    mail.get("subject", "") or "",
                    mail.get("body", "") or "",
                    mail.get("from", "") or "",
                ])
                code = self.extract_from_text(text)
                if code:
                    self._active.pop(email.lower(), None)
                    return code
            time.sleep(interval)
        self._active.pop(email.lower(), None)
        return None

    async def wait_code(
        self, email: str, *, timeout: float = 120, project: dict[str, Any] | None = None
    ) -> str | None:
        return await asyncio.to_thread(self._wait_code_sync, email, timeout)

    def self_test(self) -> dict[str, Any]:
        try:
            address = self.generate_address()
            return {"ok": True, "message": f"连接成功，测试邮箱: {address}"}
        except Exception as exc:
            return {"ok": False, "message": str(exc)}
