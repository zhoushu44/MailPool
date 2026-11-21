"""邮箱 Provider 抽象层：加一种新邮箱 = 新建 1 个文件 + 注册表加 1 行。

设计对齐 RegiForge 的 mailsys 抽象（pooled/ephemeral 两个正交维度），
但完全独立实现，不依赖 RegiForge 代码。

    pooled     号是"持有的固定账号，claim/释放"（Outlook 私有号池）
    ephemeral  地址是不是每次现造

当前只有号池型：
    provider            pooled  ephemeral   说明
    ─────────────────── ─────── ─────────   ──────────────────────
    outlook              True    False      导入的固定号池，claim 用

（临时源 mailnest / cf_temp / tempmail 已于 2026-11-20 移除，只保留号池。）
"""
from __future__ import annotations

import re
from abc import ABC, abstractmethod
from typing import Any, Optional

from pydantic import BaseModel


class ProviderError(RuntimeError):
    """Provider 统一异常。fatal=True 表示号本身废了（凭证失效/不可用）。"""

    def __init__(self, message: str, *, fatal: bool = False, kind: str = ""):
        super().__init__(message)
        self.fatal = fatal
        self.kind = kind


# ────────────────────────────────────────────────────────────────
#  通用验证码提取（数字 / 字母数字混合，含关键词加权）
# ────────────────────────────────────────────────────────────────


def _normalize_text(value: str) -> str:
    text = re.sub(r"[\u200b-\u200f\u202a-\u202e]", "", str(value or ""))
    return re.sub(r"[０-９]", lambda m: chr(ord(m.group(0)) - 0xFEE0), text)


# 邮件正文里的常见英文词：被关键词正则误抓成验证码的元凶。
# （纯字母已在 add_alnum 里直接拒绝；这张表是第二道闸，挡 "Zai2024" 这类
#   带数字但不是验证码的噪声。）
_CODE_STOP_WORDS = frozenset({
    "hello", "hi", "dear", "thanks", "thank", "welcome", "please",
    "verify", "verification", "email", "account", "click", "button",
    "below", "here", "team", "support", "help", "code", "passcode",
    "valid", "expire", "expires", "minutes", "hours", "zai", "zai2",
})


def extract_code(text: str) -> str:
    """从邮件文本中提取验证码。数字优先（6 位加权），兼顾字母数字混合。

    返回 "" 表示没找到。
    """
    text = _normalize_text(text)
    candidates: list[tuple[str, int]] = []

    def add(raw: str, score: int = 0) -> None:
        code = re.sub(r"[^\d]", "", str(raw or ""))
        if len(code) < 4 or len(code) > 8 or re.match(r"^(19|20)\d{2}$", code):
            return
        candidates.append((code, score + (4 if len(code) == 6 else 0)))

    def add_alnum(raw: str, score: int = 0) -> None:
        code = re.sub(r"\s+", "", str(raw or ""))
        if not code or len(code) < 4 or len(code) > 10:
            return
        # 验证码必然含数字。纯字母（Hello / Please / Thanks / Verification…）
        # 绝不可能是验证码，但会被关键词正则抓到 —— 线上实测：
        #   "Email Verification\r\n\r\nHello zphp5wws1k5,..."
        #   → 'verification' + 空白 + 'Hello' → 被当成验证码返回
        # 所有「点链接验证」型邮件（z.ai / AWS / Meta / GitHub）都会中招。
        if not any(ch.isdigit() for ch in code):
            return
        if code.lower() in _CODE_STOP_WORDS:
            return
        candidates.append((code, score))

    # 1. 纯数字：关键词附近（如 "您的验证码是 123456"）
    for m in re.finditer(
        r"(验证码|校验码|动态码|安全码|code|passcode|otp|verification)[^\d]{0,40}((?:\d[\s-]*){4,8})",
        text,
        flags=re.IGNORECASE,
    ):
        add(m.group(2), 10)

    # 2. 字母数字混合：关键词附近（如 FPW-62T、X4K9P2）
    for m in re.finditer(
        r"(?:code|passcode|otp|verification|验证码|校验码)[^\w]{0,20}([A-Z0-9]{3,8}-?[A-Z0-9]{0,8})",
        text,
        flags=re.IGNORECASE,
    ):
        add_alnum(m.group(1), 12)

    # 3. 字母数字混合：独立行
    for m in re.finditer(r"(?:^|\n)\s*([A-Z0-9]{2,4}-[A-Z0-9]{2,4})\s*(?:\n|$)", text):
        add_alnum(m.group(1), 8)
    for m in re.finditer(r"(?:^|\n)\s*([A-Z]{2,4}\d{2,4})\s*(?:\n|$)", text):
        add_alnum(m.group(1), 6)

    # 4. 纯数字：任意位置
    for m in re.finditer(r"(?:^|[^\d])((?:\d[\s-]*){4,8})(?!\d)", text):
        add(m.group(1), 1)

    candidates.sort(key=lambda item: (item[1], len(item[0])), reverse=True)
    return candidates[0][0] if candidates else ""


def extract_code_from_mail(mail: dict[str, Any]) -> str:
    """从 mail dict 提取验证码：优先使用预提取字段，再回退全文。"""
    for key in ("code", "code_match", "otp", "verification_code"):
        raw = str(mail.get(key) or "").strip()
        if raw and 3 <= len(raw) <= 12:
            return raw
    combined = "\n".join(
        str(mail.get(k) or "") for k in ("subject", "body_preview", "body", "text", "content", "raw")
    )
    return extract_code(combined)


# ────────────────────────────────────────────────────────────────
#  自定义正则提取（项目级 code_regex，用户可在页面上直接测试）
# ────────────────────────────────────────────────────────────────


def parse_code_patterns(value: Any) -> list[re.Pattern[str]]:
    """解析项目配置的 code_regex（多行，每行一个正则）。

    - 支持命名组 (?P<code>...) 指定验证码捕获
    - 无命名组时取第一个捕获组；无捕获组时取整个匹配
    """
    patterns: list[re.Pattern[str]] = []
    for line in str(value or "").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            patterns.append(re.compile(line))
        except re.error:
            continue
    return patterns


def extract_code_by_regex(text: str, patterns: list[re.Pattern[str]]) -> str:
    """用自定义正则从文本提取验证码，返回 "" 表示未命中。"""
    for pat in patterns:
        for m in pat.finditer(text):
            code = ""
            if "code" in pat.groupindex:
                code = m.group("code") or ""
            elif m.groups():
                code = m.group(1) or ""
            else:
                code = m.group(0)
            code = str(code).strip()
            if code:
                return code
    return ""


# ────────────────────────────────────────────────────────────────
#  配置字段声明（供管理后台动态渲染表单）
# ────────────────────────────────────────────────────────────────


class ConfigField(BaseModel):
    key: str
    label: str
    type: str = "text"  # text / password / number / textarea
    required: bool = False
    placeholder: str = ""
    help: str = ""
    default: str = ""


# ────────────────────────────────────────────────────────────────
#  抽象基类
# ────────────────────────────────────────────────────────────────


class EmailProvider(ABC):
    id: str = ""
    name: str = ""
    pooled: bool = False
    ephemeral: bool = False
    config_fields: list[ConfigField] = []
    help_text: str = ""

    def __init__(self) -> None:
        self._config: dict[str, Any] = {}

    def configure(self, config: dict[str, Any] | None = None) -> None:
        self._config = config or {}

    def config(self, key: str, default: Any = "") -> Any:
        value = self._config.get(key)
        return default if value is None else value

    # ── 验证码提取（项目级 code_regex 优先，回退各 provider 自带逻辑）──
    def _code_patterns(self) -> list[re.Pattern[str]]:
        return parse_code_patterns(self.config("code_regex", ""))

    def extract_from_text(self, text: str) -> str:
        """从纯文本提取验证码：项目自定义正则优先，未命中走自兜底逻辑。"""
        code = extract_code_by_regex(text, self._code_patterns())
        return code or self._fallback_extract_from_text(text)

    def extract_from_mail(self, mail: dict[str, Any]) -> str:
        """从邮件 dict 提取验证码：先试项目正则，再回退通用 mail 提取。"""
        patterns = self._code_patterns()
        if patterns:
            combined = "\n".join(
                str(mail.get(k) or "") for k in ("subject", "body_preview", "body", "text", "content", "raw")
            )
            code = extract_code_by_regex(combined, patterns)
            if code:
                return code
        return extract_code_from_mail(mail)

    def _fallback_extract_from_text(self, text: str) -> str:
        """provider 可覆写：保留各自的特殊提取逻辑。"""
        return extract_code(text)

    @abstractmethod
    def generate_address(self) -> str:
        """返回一个可用的邮箱地址（临时源=新建；固定源=claim 一个）。"""

    @abstractmethod
    async def wait_code(
        self, email: str, *, timeout: float = 120, project: dict[str, Any] | None = None
    ) -> str | None:
        """轮询等待验证码，超时返回 None。"""

    async def wait_mail(
        self, email: str, *, timeout: float = 120, project: dict[str, Any] | None = None
    ) -> dict[str, Any] | None:
        """等待一封验证码邮件；不支持完整邮件的 Provider 返回最小结果。"""
        code = await self.wait_code(email, timeout=timeout, project=project)
        if not code:
            return None
        return {"code": code, "message_id": "", "sender": "", "subject": "", "body": ""}

    def release(self, email: str) -> None:
        """释放邮箱（临时源才需要；默认无操作）。"""

    def self_test(self) -> dict[str, Any]:
        return {"ok": True, "message": f"{self.name} 无需测试"}

    def meta(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "name": self.name,
            "pooled": self.pooled,
            "ephemeral": self.ephemeral,
            "config_fields": [f.model_dump() for f in self.config_fields],
            "help_text": self.help_text,
        }


# ────────────────────────────────────────────────────────────────
#  注册表 + 工厂
# ────────────────────────────────────────────────────────────────

_REGISTRY: dict[str, type[EmailProvider]] = {}


def register(cls: type[EmailProvider]) -> type[EmailProvider]:
    key = (cls.id or "").strip().lower()
    if not key:
        raise ValueError(f"{cls.__name__} 必须定义唯一 id")
    if key in _REGISTRY:
        raise ValueError(f"id='{key}' 已被 {_REGISTRY[key].__name__} 占用")
    _REGISTRY[key] = cls
    return cls


def get_provider_class(provider_id: str) -> type[EmailProvider]:
    key = (provider_id or "").strip().lower()
    if key not in _REGISTRY:
        known = ", ".join(sorted(_REGISTRY)) or "(空)"
        raise ProviderError(f"未知邮箱源: '{provider_id}'（已注册: {known}）", fatal=True)
    return _REGISTRY[key]


def create_provider(provider_id: str, config: dict[str, Any] | None = None) -> EmailProvider:
    provider = get_provider_class(provider_id)()
    provider.configure(config)
    return provider


def list_providers() -> list[dict[str, Any]]:
    return [cls().meta() for cls in _REGISTRY.values()]


# 延迟 import 触发注册
def _load_registry() -> None:
    from . import outlook  # noqa: F401


_load_registry()
