"""Cloudflare Worker 临时邮箱 Provider（dreamhunter2333/cloudflare_temp_email）。

- POST /admin/new_address 创建邮箱（enablePrefix=True）
- GET  /admin/mails?address=<email> 拉邮件
- 从 raw 字段抽 OTP
"""
from __future__ import annotations

import asyncio
import json
import logging
import random
import string
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timezone
from typing import Any, Optional

from .base import ConfigField, EmailProvider, ProviderError, register

logger = logging.getLogger(__name__)


def _gen_local_part(rng: random.Random, length: int = 10) -> str:
    return "".join(rng.choices(string.ascii_lowercase + string.digits, k=length))


@register
class CFTempEmailProvider(EmailProvider):
    id = "cf_temp"
    name = "CF Worker 临时邮箱"
    ephemeral = True

    config_fields = [
        ConfigField(key="cf_api_url", label="Worker 地址", required=True,
                    placeholder="https://mail.example.com",
                    help="Cloudflare Worker 的 HTTPS 地址，末尾不带斜杠"),
        ConfigField(key="cf_admin_token", label="Admin Token", type="password",
                    help="Worker 环境变量 ADMIN_PASSWORDS 的值"),
        ConfigField(key="cf_domain", label="收件域名", required=True,
                    placeholder="example.com", help="已配置 catch-all 的域名"),
        ConfigField(key="proxy", label="代理", placeholder="socks5://user:pass@host:port",
                    help="可选；多行取第一行"),
    ]

    def __init__(self) -> None:
        super().__init__()
        self._seen_mail_ids: set[str] = set()
        self._rng = random.Random()

    def configure(self, config: dict[str, Any] | None = None) -> None:
        super().configure(config)
        self._seen_mail_ids = set()

    def _api_url(self) -> str:
        return str(self._config.get("cf_api_url") or "").strip().rstrip("/")

    def _admin_token(self) -> str:
        return str(self._config.get("cf_admin_token") or "").strip()

    def _domain(self) -> str:
        return str(self._config.get("cf_domain") or "").strip()

    def _headers(self) -> dict:
        return {
            "accept": "application/json, text/plain, */*",
            "content-type": "application/json",
            "x-admin-auth": self._admin_token(),
        }

    def _request(self, method: str, path: str, **kwargs):
        """统一请求：urllib 实现，返回对象带 status_code/text/json。"""
        url = f"{self._api_url()}{path}"
        m = method.upper()
        timeout = kwargs.get("timeout", 15)
        headers = dict(kwargs.get("headers") or self._headers())
        json_body = kwargs.get("json")
        params = kwargs.get("params")

        if params:
            qs = urllib.parse.urlencode(params)
            url = f"{url}?{qs}"
        body = json.dumps(json_body, separators=(",", ":")).encode() if json_body is not None else None
        req = urllib.request.Request(url, data=body, headers=headers, method=m)

        class _Resp:
            def __init__(self, status, text):
                self.status_code = status
                self.text = text or ""
                self.ok = 200 <= status < 300

            def json(self) -> dict:
                return json.loads(self.text or "{}")

        try:
            with urllib.request.urlopen(req, timeout=timeout) as r:
                return _Resp(r.status, r.read().decode("utf-8", errors="replace"))
        except urllib.error.HTTPError as e:
            try:
                text = e.read().decode("utf-8", errors="replace")
            except Exception:
                text = ""
            return _Resp(e.code, text)

    def generate_address(self) -> str:
        api_url = self._api_url()
        domain = self._domain()
        if not api_url or not domain:
            raise ProviderError("CF Worker 临时邮箱未配置完整（缺 cf_api_url / cf_domain）")
        local = _gen_local_part(self._rng, length=10)
        payload = {"enablePrefix": True, "name": local, "domain": domain}
        resp = self._request("POST", "/admin/new_address", json=payload, timeout=15)
        if resp.status_code != 200:
            raise ProviderError(
                f"CFTempEmail create_mailbox 失败: status={resp.status_code} body={resp.text[:300]}"
            )
        data = resp.json()
        email = (data.get("email") or data.get("address") or "").strip()
        if not email:
            raise ProviderError(f"new_address 响应缺 email 字段: {data}")
        self._seen_mail_ids = set()
        logger.info(f"[cf_temp] 创建邮箱: {email}")
        return email

    def _get_mails(self, email: str) -> list[dict]:
        resp = self._request(
            "GET", "/admin/mails",
            params={"limit": 20, "offset": 0, "address": email},
            timeout=10,
        )
        if resp.status_code != 200:
            return []
        data = resp.json()
        if isinstance(data, dict):
            return data.get("results") or data.get("mails") or []
        if isinstance(data, list):
            return data
        return []

    @staticmethod
    def _mail_epoch(mail: dict) -> Optional[float]:
        raw = (mail.get("created_at") or "").strip()
        if not raw:
            return None
        raw = raw.replace("T", " ").replace("Z", "").split(".")[0]
        try:
            dt = datetime.strptime(raw, "%Y-%m-%d %H:%M:%S")
        except Exception:
            return None
        return dt.replace(tzinfo=timezone.utc).timestamp()

    def _wait_code_sync(self, email: str, timeout: float) -> str | None:
        timeout = max(float(timeout), 30.0)
        deadline = time.time() + timeout
        # 起始快照：旧邮件全部标记为已见
        try:
            for m in self._get_mails(email):
                mid = str(m.get("id", ""))
                if mid:
                    self._seen_mail_ids.add(mid)
        except Exception:
            pass
        while time.time() < deadline:
            try:
                for mail in sorted(self._get_mails(email), key=lambda x: x.get("id", 0), reverse=True):
                    mid = str(mail.get("id", ""))
                    if not mid or mid in self._seen_mail_ids:
                        continue
                    self._seen_mail_ids.add(mid)
                    code = self.extract_from_text(str(mail.get("raw") or ""))
                    if code:
                        logger.info(f"[cf_temp] 命中新邮件 id={mid}")
                        return code
            except Exception as exc:
                logger.warning(f"[cf_temp] poll 异常: {exc}")
            time.sleep(3)
        return None

    async def wait_code(
        self, email: str, *, timeout: float = 120, project: dict[str, Any] | None = None
    ) -> str | None:
        return await asyncio.to_thread(self._wait_code_sync, email, timeout)

    def self_test(self) -> dict[str, Any]:
        try:
            email = self.generate_address()
            return {"ok": True, "message": f"连接成功，测试邮箱: {email}"}
        except Exception as exc:
            return {"ok": False, "message": str(exc)}
