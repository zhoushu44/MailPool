"""验证码提取与可选 AI 识别。"""
from __future__ import annotations

import json
import os
import urllib.request
from typing import Any

from ..providers.base import extract_code, extract_code_from_mail


def extract_rule(mail: dict[str, Any]) -> str:
    code = extract_code_from_mail(mail)
    if code:
        return code
    return extract_code("\n".join(str(mail.get(k) or "") for k in ("subject", "body_preview", "body", "text")))


def extract_ai(mail: dict[str, Any], use_ai: bool = True) -> dict[str, Any]:
    rule_code = extract_rule(mail)
    base_url = os.getenv("MAILPOOL_AI_BASE_URL", "").strip()
    api_key = os.getenv("MAILPOOL_AI_API_KEY", "").strip()
    model = os.getenv("MAILPOOL_AI_MODEL", "").strip()
    empty = {"verification_code": rule_code, "confidence": 1.0 if rule_code else 0.0, "reason": "rule" if rule_code else ""}
    if not use_ai or not (base_url and api_key and model):
        return empty
    prompt = "Extract the verification code from this email. Return JSON with verification_code, confidence, reason.\n" + json.dumps(mail, ensure_ascii=False)[:12000]
    payload = json.dumps({"model": model, "messages": [{"role": "user", "content": prompt}], "temperature": 0}).encode()
    endpoint = base_url.rstrip("/")
    if not endpoint.endswith("/v1"):
        endpoint += "/v1"
    req = urllib.request.Request(endpoint + "/chat/completions", data=payload, method="POST", headers={"Authorization": "Bearer " + api_key, "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            data = json.loads(resp.read())
        content = data["choices"][0]["message"]["content"]
        result = json.loads(content.strip().removeprefix("```json").removesuffix("```").strip())
        return {"verification_code": str(result.get("verification_code") or rule_code), "confidence": float(result.get("confidence") or 0), "reason": str(result.get("reason") or "")}
    except Exception:
        return empty
