# -*- coding: utf-8 -*-
"""链接验证型邮件的处理：无 code 也要返回邮件正文。

背景（线上事故）：
  z.ai 的验证邮件是「点击链接验证」，正文只有 URL 没有数字码。
  旧逻辑 `if not code: continue` 把这封邮件直接跳过，客户端等 180 秒
  拿到 timeout + 空 mails，完全拿不到验证链接。
"""
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))))

os.environ.setdefault("MAILPOOL_ADMIN_PASSWORD", "adminpass")

from app.providers.base import _CODE_STOP_WORDS, extract_code  # noqa: E402


class TestNoFalseCode:
    """提取器不能把英文单词当验证码。"""

    @pytest.mark.parametrize("text", [
        # 真实 z.ai 邮件 preview 开头
        "Z.ai\r\n\r\nEmail Verification\r\n\r\nHello zphp5wws1k5,\r\n\r\nThank you",
        "Email Verification\r\n\r\nHello",
        "verification code: Hello",
        "verification: Thanks",
        "Please verify your email address by clicking the link below:",
        "Your verification code is Welcome",
    ])
    def test_no_word_as_code(self, text):
        assert extract_code(text) == ""

    @pytest.mark.parametrize("text,want", [
        ("Your verification code is 483920.", "483920"),
        ("Your temporary ChatGPT verification code is 338176", "338176"),
        ("验证码：583921，5分钟内有效", "583921"),
        ("Your code: FPW-62T", "FPW-62T"),
        ("Enter code X4K9P2 to continue", "X4K9P2"),
        ("Your AWS Builder ID verification code is 066684", "066684"),
        ("code: AB12CD", "AB12CD"),
    ])
    def test_real_codes_still_work(self, text, want):
        assert extract_code(text) == want

    def test_stop_words_reject_uppercase_too(self):
        """黑名单必须大小写不敏感。"""
        assert extract_code("verification: HELLO") == ""
        assert "hello" in _CODE_STOP_WORDS


class TestReceiveReturnsMails:
    """receive 必须把原始邮件（含正文）返回给调用方。"""

    def test_success_includes_mails_with_body(self):
        from app.services import mail_service

        captured = {}

        class FakeProv:
            pooled = True

            def set_credentials(self, email, cred):
                pass

            def set_baseline(self, ids, at):
                pass

            async def wait_mail(self, email, *, timeout=120, project=None):
                return {
                    "code": "123456",
                    "message_id": "mid-1",
                    "provider": "outlook",
                    "folder": "inbox",
                    "sender": "noreply@example.com",
                    "subject": "Your code",
                    "body_preview": "code 123456",
                    "body": "<p>Your code is 123456</p>",
                    "received_at": "2026-11-21T08:00:00+00:00",
                    "code_source": "rule",
                }

        import app.services.mail_service as ms

        orig_build = ms.build_provider
        orig_get = ms.pool_service.get_account_credentials
        orig_base = ms.pool_service.get_mail_baseline
        orig_save = ms.save_mail_record
        try:
            ms.build_provider = lambda db, project: (FakeProv(), {"timeout": 30})
            ms.pool_service.get_account_credentials = lambda db, email: {"refresh_token": "rt", "client_id": "cid"}
            ms.pool_service.get_mail_baseline = lambda db, email: {"message_ids": [], "baseline_at": None}
            ms.save_mail_record = lambda *a, **k: captured.update(k)

            from app.database import SessionLocal, init_db
            from app.models import Project

            init_db()
            db = SessionLocal()
            try:
                if not db.query(Project).filter_by(code="probe_linksite").first():
                    db.add(Project(code="probe_linksite", name="probe", provider="outlook"))
                    db.commit()
                out = ms.receive_code(db, "probe_linksite", "x@example.com", timeout=10)
            finally:
                db.close()
        finally:
            ms.build_provider = orig_build
            ms.pool_service.get_account_credentials = orig_get
            ms.pool_service.get_mail_baseline = orig_base
            ms.save_mail_record = orig_save

        assert out["status"] == "success"
        assert out["code"] == "123456"
        assert isinstance(out["mails"], list) and len(out["mails"]) == 1
        m = out["mails"][0]
        for key in ("id", "message_id", "subject", "body", "body_preview", "from", "sender", "received_at"):
            assert key in m, f"mails[0] 缺字段 {key}"
        assert "123456" in m["body"]

    def test_link_only_mail_reports_mail_arrived(self):
        """没有验证码但邮件到了 → status=mail_arrived，正文照给。"""
        from app.services import mail_service as ms

        BODY = '<html><a href="https://chat.z.ai/auth/verify_email?token=abc123">Verify Email</a></html>'

        class FakeProv:
            pooled = True

            def set_credentials(self, email, cred):
                pass

            def set_baseline(self, ids, at):
                pass

            async def wait_mail(self, email, *, timeout=120, project=None):
                return {
                    "code": "",  # ← 提取不出验证码
                    "message_id": "mid-2",
                    "provider": "outlook",
                    "folder": "inbox",
                    "sender": "noreply@email.z.ai",
                    "subject": "Verify Your Email",
                    "body_preview": "Hello, please verify...",
                    "body": BODY,
                    "received_at": "2026-11-21T08:01:56+00:00",
                    "code_source": "none",
                }

        orig_build = ms.build_provider
        orig_get = ms.pool_service.get_account_credentials
        orig_base = ms.pool_service.get_mail_baseline
        orig_save = ms.save_mail_record
        try:
            ms.build_provider = lambda db, project: (FakeProv(), {"timeout": 30})
            ms.pool_service.get_account_credentials = lambda db, email: {"refresh_token": "rt", "client_id": "cid"}
            ms.pool_service.get_mail_baseline = lambda db, email: {"message_ids": [], "baseline_at": None}
            ms.save_mail_record = lambda *a, **k: None

            from app.database import SessionLocal, init_db
            from app.models import Project

            init_db()
            db = SessionLocal()
            try:
                if not db.query(Project).filter_by(code="probe_linksite2").first():
                    db.add(Project(code="probe_linksite2", name="probe2", provider="outlook"))
                    db.commit()
                out = ms.receive_code(db, "probe_linksite2", "y@example.com", timeout=10)
            finally:
                db.close()
        finally:
            ms.build_provider = orig_build
            ms.pool_service.get_account_credentials = orig_get
            ms.pool_service.get_mail_baseline = orig_base
            ms.save_mail_record = orig_save

        assert out["status"] == "mail_arrived"
        assert out["code"] == ""
        assert len(out["mails"]) == 1
        assert "chat.z.ai/auth/verify_email?token=abc123" in out["mails"][0]["body"]

    def test_timeout_returns_empty_mails(self):
        from app.services import mail_service as ms

        class FakeProv:
            pooled = True

            def set_credentials(self, email, cred):
                pass

            def set_baseline(self, ids, at):
                pass

            async def wait_mail(self, email, *, timeout=120, project=None):
                return None

        orig_build = ms.build_provider
        orig_get = ms.pool_service.get_account_credentials
        orig_base = ms.pool_service.get_mail_baseline
        try:
            ms.build_provider = lambda db, project: (FakeProv(), {"timeout": 30})
            ms.pool_service.get_account_credentials = lambda db, email: {"refresh_token": "rt", "client_id": "cid"}
            ms.pool_service.get_mail_baseline = lambda db, email: {"message_ids": [], "baseline_at": None}

            from app.database import SessionLocal, init_db
            from app.models import Project

            init_db()
            db = SessionLocal()
            try:
                if not db.query(Project).filter_by(code="probe_linksite3").first():
                    db.add(Project(code="probe_linksite3", name="probe3", provider="outlook"))
                    db.commit()
                out = ms.receive_code(db, "probe_linksite3", "z@example.com", timeout=10)
            finally:
                db.close()
        finally:
            ms.build_provider = orig_build
            ms.pool_service.get_account_credentials = orig_get
            ms.pool_service.get_mail_baseline = orig_base

        assert out["status"] == "timeout"
        assert out["code"] == ""
        assert out["mails"] == []
