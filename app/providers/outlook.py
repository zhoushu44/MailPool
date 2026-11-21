"""Outlook 私有邮箱池 Provider（Graph API + IMAP 双通道）。

账号凭证存于邮箱池（email_accounts 表），格式 4 段式：
    email----password----client_id----refresh_token

取件优先级：Graph API（inbox/junkemail/deleteditems）→ IMAP XOAUTH2 → IMAP 密码。
"""
from __future__ import annotations

import asyncio
import email as _email
import email.utils as _eu
import imaplib
import json
import logging
import re
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from typing import Any, Callable, Optional

from .base import ConfigField, EmailProvider, ProviderError, register

logger = logging.getLogger(__name__)

TOKEN_ENDPOINTS = [
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/token",
    "https://login.live.com/oauth20_token.srf",
    "https://login.microsoftonline.com/common/oauth2/v2.0/token",
]
GRAPH_SCOPE = "https://graph.microsoft.com/.default"
IMAP_SCOPE = "https://outlook.office.com/IMAP.AccessAsUser.All offline_access"
GRAPH_BASE = "https://graph.microsoft.com/v1.0"
GRAPH_FOLDERS = ["inbox", "junkemail", "deleteditems"]
IMAP_SERVERS = ["outlook.live.com", "outlook.office365.com"]

# ── 旧信防护参数 ────────────────────────────────────────────
# 号池里的邮箱都是回收号，收件箱里躺着几天前的旧信。
# 只认「取号之后」到达的邮件：时间窗口 + message_id 基线双判据，
# 任一不满足就跳过，宁可超时也绝不把旧验证码交出去。
MAIL_FRESH_SLACK_SECONDS = 60      # 服务器时钟误差容忍
MAIL_FRESH_MAX_AGE_MINUTES = 30    # 没有基线时，最多认 30 分钟内到达的信

_FATAL_IMAP_PATTERNS = (
    "user is authenticated but not connected",
    "authentication failed",
    "authenticate failed",
    "imap xoauth2",
    "invalid_grant",
    "invalid_client",
    "basic authentication is disabled",
)


def _build_opener(proxy: str | None = None):
    """构建 urllib opener，支持账号级代理。

    proxy 格式：http://host:port / https://host:port / socks5://user:pass@host:port
    （socks5 需安装 pysocks；缺失时回退直连并告警。）
    """
    if not proxy:
        return urllib.request.build_opener()
    proxy = str(proxy).strip()
    if proxy.startswith(("http://", "https://")):
        return urllib.request.build_opener(urllib.request.ProxyHandler({
            "http": proxy, "https": proxy,
        }))
    if proxy.startswith(("socks5://", "socks5h://", "socks4://")):
        try:
            import socks  # pysocks
            return urllib.request.build_opener(
                socks.SOCKSProxyHandler({"http": proxy, "https": proxy})
            )
        except ImportError:
            logger.warning("[proxy] socks5 代理需要 pysocks（pip install pysocks），本次直连")
            return urllib.request.build_opener()
    logger.warning("[proxy] 无法识别的代理格式: %s，本次直连", proxy)
    return urllib.request.build_opener()


def _urlopen(req: urllib.request.Request, timeout: float = 15, proxy: str | None = None):
    """带代理支持的 urlopen。"""
    try:
        return _build_opener(proxy).open(req, timeout=timeout)
    except urllib.error.URLError as e:
        # 单个代理异常时，再直连重试一次，避免代理失效导致全部失败
        if proxy and isinstance(e.reason, OSError):
            logger.warning("[proxy] 代理请求失败，尝试直连: %s", proxy)
            return urllib.request.urlopen(req, timeout=timeout)
        raise


def _is_fatal_imap_error(exc: Exception) -> bool:
    msg = str(exc or "").lower()
    return any(p in msg for p in _FATAL_IMAP_PATTERNS)


# ── 跨次取件去重（进程内缓存，TTL 24h）─────────────────────
# 仅缓存"已成功返回过验证码"的邮件，避免同一封邮件在多次 wait_code 调用中
# 被反复提取返回（例如用户连续等两个码时，旧码邮件会被再次拉到）。
# 进程重启后缓存丢失，最坏情况是返回一次旧码（不会漏码）。

_returned_messages: dict[str, dict[str, float]] = {}  # {email_lower: {msg_id: ts}}
_RETURNED_TTL = 86400  # 24 小时


def _is_returned(email: str, msg_id: str) -> bool:
    """某邮件是否在 24h 内已返回过验证码。"""
    if not msg_id:
        return False
    bucket = _returned_messages.get((email or "").lower())
    if not bucket:
        return False
    ts = bucket.get(msg_id)
    if ts is None:
        return False
    if time.time() - ts > _RETURNED_TTL:
        del bucket[msg_id]
        return False
    return True


def _mark_returned(email: str, msg_id: str) -> None:
    """标记某邮件已返回过验证码（顺便清理过期项）。"""
    if not msg_id:
        return
    key = (email or "").lower()
    bucket = _returned_messages.setdefault(key, {})
    bucket[msg_id] = time.time()
    now = time.time()
    expired = [k for k, v in bucket.items() if now - v > _RETURNED_TTL]
    for k in expired:
        del bucket[k]


# ── 时间解析（多格式 + 时区归一）────────────────────────────
def parse_mail_time(value: Any) -> datetime | None:
    """把各种来源的邮件时间统一解析成 **带时区** 的 UTC datetime。

    踩过的坑：Graph 返回 '2026-11-08T10:23:00Z'，IMAP 返回 RFC2822
    'Sat, 8 Nov 2026 10:23:00 +0800'，数据库里存的是不带时区的 naive UTC。
    直接相减会整体偏 8 小时，导致刚到的信被判成旧信（永远取不到码）。

    支持：datetime、ISO8601（Z / +08:00 / 无时区按 UTC）、RFC2822、unix 时间戳。
    """
    if value is None or value == "":
        return None
    if isinstance(value, datetime):
        dt = value
    elif isinstance(value, (int, float)):
        try:
            dt = datetime.fromtimestamp(float(value), tz=timezone.utc)
        except (OverflowError, OSError, ValueError):
            return None
    else:
        text = str(value).strip()
        if not text:
            return None
        dt = None
        # ISO8601：'2026-11-08T10:23:00Z' / '2026-11-08T10:23:00+08:00' / '2026-11-08 10:23:00'
        try:
            dt = datetime.fromisoformat(text.replace("Z", "+00:00").replace("z", "+00:00"))
        except ValueError:
            dt = None
        # RFC2822：'Sat, 8 Nov 2026 10:23:00 +0800'
        if dt is None:
            try:
                dt = _eu.parsedate_to_datetime(text)
            except (TypeError, ValueError):
                dt = None
        if dt is None:
            return None
    # 无时区 → 视为 UTC（数据库统一按 UTC 存）
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def is_fresh_mail(
    received_at: Any,
    baseline_at: datetime | None,
    *,
    slack_seconds: float = MAIL_FRESH_SLACK_SECONDS,
    max_age_minutes: float = MAIL_FRESH_MAX_AGE_MINUTES,
) -> bool:
    """判断一封邮件是否是「取号之后」到达的（用于挡掉回收号里的旧信）。

    判据：received_at >= baseline_at - slack
    - 没有基线时：只认 max_age_minutes 以内的邮件（更保守）
    - 时间无法解析时：返回 False（宁可超时也不给旧码）
    """
    dt = parse_mail_time(received_at)
    if dt is None:
        return False
    now = datetime.now(timezone.utc)
    if baseline_at is not None:
        base = baseline_at if baseline_at.tzinfo else baseline_at.replace(tzinfo=timezone.utc)
        return dt >= base - timedelta(seconds=slack_seconds)
    return dt >= now - timedelta(minutes=max_age_minutes)


# ── 健康扫描 & 心跳保活 ──────────────────────────────────


def verify_account(cred: dict[str, Any]) -> tuple[str, str]:
    """验证 Outlook 账号凭证是否有效。

    返回 (status, detail)：
      status = "healthy"   账号可用
      status = "unhealthy" 账号异常（token 失效/被风控/缺凭证）

    检测流程：
      1. 检查 client_id + refresh_token 是否存在
      2. 尝试 Graph scope 刷新 token → GET /me/messages 验证
      3. Graph 失败时降级 IMAP scope 尝试
    """
    rt = cred.get("refresh_token") or ""
    cid = cred.get("client_id") or ""
    proxy = cred.get("proxy") or ""

    if not rt or not cid:
        logger.info(f"[verify] 跳过：缺少凭证（rt={'有' if rt else '空'}, cid={'有' if cid else '空'}）")
        return "unhealthy", "缺少 client_id 或 refresh_token"

    # 尝试 Graph scope
    try:
        data = _refresh_token_for(cred, GRAPH_SCOPE)
        access_token = data["access_token"]

        # Graph API 验证：拉取 1 封邮件
        req = urllib.request.Request(
            f"{GRAPH_BASE}/me/messages?$top=1&$select=id",
            headers={"Authorization": f"Bearer {access_token}"},
        )
        _urlopen(req, timeout=15, proxy=proxy)
        logger.info(f"[verify] ✓ Graph 验证通过 cid={cid[:8]}...{cid[-4:]}")
        return "healthy", "Graph API 验证通过"
    except urllib.error.HTTPError as e:
        if e.code == 400:
            body = e.read().decode("utf-8", errors="replace")
            body_short = body[:200]
            if "invalid_grant" in body:
                # 降级试 IMAP scope
                logger.info(f"[verify] Graph scope invalid_grant，降级试 IMAP scope")
                try:
                    data = _refresh_token_for(cred, IMAP_SCOPE)
                    logger.info(f"[verify] ✓ IMAP scope 验证通过（Graph scope 缺失）")
                    return "healthy", "IMAP scope 验证通过（Graph scope 缺失）"
                except Exception as e2:
                    logger.info(f"[verify] ✗ IMAP scope 也失败：{str(e2)[:100]}")
                    # 返回完整错误信息，包含 service abuse mode
                    return "unhealthy", f"invalid_grant: {body[:300]}"
            logger.info(f"[verify] ✗ HTTP 400: {body_short[:80]}")
            return "unhealthy", f"HTTP {e.code}: {body_short[:100]}"
        if e.code in (401, 403):
            logger.info(f"[verify] ✗ HTTP {e.code}（账号可能被风控锁定）")
            return "unhealthy", f"HTTP {e.code}: 账号可能被风控锁定"
        logger.info(f"[verify] ✗ HTTP {e.code}")
        return "unhealthy", f"HTTP {e.code}"
    except ProviderError:
        # _request_access_token 遍历所有端点后抛出
        # 降级试 IMAP scope
        logger.info(f"[verify] Graph scope 所有端点失败，降级试 IMAP scope")
        try:
            data = _refresh_token_for(cred, IMAP_SCOPE)
            logger.info(f"[verify] ✓ IMAP scope 验证通过")
            return "healthy", "IMAP scope 验证通过"
        except Exception as e2:
            logger.info(f"[verify] ✗ IMAP scope 也失败：{str(e2)[:100]}")
            # 保留完整错误信息用于检测 service abuse
            return "unhealthy", str(e2)
    except Exception as e:
        logger.info(f"[verify] ✗ 异常: {e}")
        return "unhealthy", f"异常: {e}"


def silent_keepalive(cred: dict[str, Any], interactive: bool = False) -> tuple[bool, str]:
    """心跳保活：Graph API 静默拉取收件箱。

    interactive=False（默认）：只读 GET /me/messages
    interactive=True：额外标记 1 封未读邮件为已读

    返回 (ok, message)。成功时自动更新 cred 中的 refresh_token（微软可能轮换）。
    """
    rt = cred.get("refresh_token") or ""
    cid = cred.get("client_id") or ""
    proxy = cred.get("proxy") or ""

    if not rt or not cid:
        logger.info("[keepalive] 跳过：缺少凭证")
        return False, "缺少 client_id 或 refresh_token"

    try:
        data = _refresh_token_for(cred, GRAPH_SCOPE)
        access_token = data["access_token"]

        # 轮换回写由 _refresh_token_for 统一处理（内存 + hook 落库）
        if data.get("refresh_token") and data["refresh_token"] != rt:
            logger.info(f"[keepalive] refresh_token 已轮换（微软返回新 token）")
        else:
            logger.info(f"[keepalive] token 刷新成功，refresh_token 未变")

        # 拉取收件箱前 5 封
        params = urllib.parse.urlencode({
            "$top": "5",
            "$orderby": "receivedDateTime DESC",
            "$select": "id,subject,isRead",
        })
        req = urllib.request.Request(
            f"{GRAPH_BASE}/me/mailFolders/inbox/messages?{params}",
            headers={"Authorization": f"Bearer {access_token}"},
        )
        resp = _urlopen(req, timeout=15, proxy=proxy)
        messages = json.loads(resp.read()).get("value", [])
        logger.info(f"[keepalive] 拉取收件箱成功，共 {len(messages)} 封邮件")

        # 拟真交互：标记 1 封未读邮件为已读
        if interactive and messages:
            unread = [m for m in messages if not m.get("isRead")]
            if unread:
                msg_id = unread[0]["id"]
                logger.info(f"[keepalive] interactive 模式：标记未读邮件为已读（subject={(unread[0].get('subject') or '')[:30]!r}）")
                patch = urllib.request.Request(
                    f"{GRAPH_BASE}/me/messages/{msg_id}",
                    data=json.dumps({"isRead": True}).encode(),
                    method="PATCH",
                    headers={
                        "Authorization": f"Bearer {access_token}",
                        "Content-Type": "application/json",
                    },
                )
                _urlopen(patch, timeout=10, proxy=proxy)
                return True, f"保活成功，标记已读 1 封（共 {len(messages)} 封）"
            else:
                logger.info(f"[keepalive] interactive 模式：无未读邮件可标记")

        return True, f"保活成功，收件箱 {len(messages)} 封邮件"

    except urllib.error.HTTPError as e:
        body = e.read().decode("utf-8", errors="replace")[:200]
        logger.info(f"[keepalive] ✗ HTTP {e.code}: {body[:80]}")
        return False, f"HTTP {e.code}: {body[:100]}"
    except Exception as e:
        logger.info(f"[keepalive] ✗ 异常: {e}")
        return False, f"异常: {e}"[:200]


def _request_access_token(refresh_token: str, client_id: str, scope: str, proxy: str | None = None) -> dict:
    logger.info(f"[token] 刷新 token cid={client_id[:8]}...{client_id[-4:]} scope={scope} endpoints={len(TOKEN_ENDPOINTS)} proxy={'yes' if proxy else 'no'}")
    last_error = ""
    for idx, endpoint in enumerate(TOKEN_ENDPOINTS, 1):
        try:
            body = urllib.parse.urlencode({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": client_id,
                "scope": scope,
            }).encode()
            req = urllib.request.Request(endpoint, data=body)
            resp = _urlopen(req, timeout=15, proxy=proxy)
            data = json.loads(resp.read())
            if data.get("access_token"):
                logger.info(f"[token] ✓ 端点 {idx}/{len(TOKEN_ENDPOINTS)} {endpoint} 刷新成功")
                return data
            last_error = f"no access_token from {endpoint}"
            logger.info(f"[token] ✗ 端点 {idx}/{len(TOKEN_ENDPOINTS)} {endpoint} 返回无 access_token")
        except urllib.error.HTTPError as e:
            text = e.read().decode("utf-8", errors="replace")
            last_error = f"HTTP {e.code} {endpoint}: {text}"
            logger.info(f"[token] ✗ 端点 {idx}/{len(TOKEN_ENDPOINTS)} {endpoint} HTTP {e.code}: {text[:80]}")
            if e.code in (400, 401, 403):
                continue
            raise
        except Exception as e:
            last_error = f"{endpoint}: {e}"
            logger.info(f"[token] ✗ 端点 {idx}/{len(TOKEN_ENDPOINTS)} {endpoint} 异常: {e}")
            continue
    logger.info(f"[token] 所有端点均失败，scope={scope} last_error={last_error[:100]}")
    raise ProviderError(f"token 获取失败 (scope={scope}): {last_error}", fatal=True)


# ── refresh_token 轮换的并发治理 ─────────────────────────────
# 微软 consumer MSA 的 refresh_token 是轮换的：用 IMAP scope 刷新会
# 返回「新 refresh_token」，旧 token 随即作废。历史上收码线程把新
# rt 只留在内存里，库里的 rt 立刻变尸体，于是下一次任何组件（批量
# 刷新 / 健康扫描 / 下一次收码）全部 invalid_grant，再降级密码 IMAP
# 撞「Basic authentication is disabled」——收码超时的元凶。
# 现在统一走 _refresh_token_for：同邮箱互斥 + 轮换结果回写数据库。

_rt_save_hook: Optional[Callable[[str, str], None]] = None
_refresh_locks: dict[str, threading.Lock] = {}
_refresh_locks_guard = threading.Lock()


def set_rt_save_hook(fn: Optional[Callable[[str, str], None]]) -> None:
    """注入 refresh_token 轮换回写回调 fn(email, new_refresh_token)。"""
    global _rt_save_hook
    _rt_save_hook = fn


def _refresh_lock(email: str, cred: dict[str, Any]) -> threading.Lock:
    key = email or (cred.get("refresh_token") or "")[:24] or "?"
    with _refresh_locks_guard:
        return _refresh_locks.setdefault(key, threading.Lock())


def _refresh_token_for(cred: dict[str, Any], scope: str, email: str = "") -> dict:
    """互斥刷新 access_token；轮换出的新 refresh_token 同步内存并回写库。"""
    email = email or str(cred.get("email") or "")
    with _refresh_lock(email, cred):
        rt = cred.get("refresh_token") or ""  # 锁内重读：同号可能刚被轮换
        data = _request_access_token(rt, cred.get("client_id") or "", scope,
                                     proxy=cred.get("proxy") or "")
        new_rt = data.get("refresh_token")
        if new_rt and new_rt != rt:
            cred["refresh_token"] = new_rt  # provider 实例内存同步
            hook = _rt_save_hook
            if hook is not None:
                try:
                    hook(email, new_rt)
                    logger.info(f"[token] {email or '(no-email)'} 轮换 rt 已回写库")
                except Exception as e:
                    logger.warning(f"[token] 轮换 rt 回写失败 {email}: {e}")
        return data


def _is_hex_color_context(haystack: str, idx: int) -> bool:
    if idx > 0 and haystack[idx - 1] == "#":
        return True
    before = haystack[max(0, idx - 30):idx]
    return bool(re.search(
        r"(?:color|background|bgcolor|fill|stroke)\s*[:=]\s*[\"']?#?\s*$",
        before, re.IGNORECASE,
    ))


def _extract_otp_from_html(body: str) -> Optional[str]:
    for pat in (
        r"(?:code(?:\s*is)?|verification|one[-\s]*time|verify|kode|verifikasi|代码|验证码|驗證碼)[^\d<>]{0,80}(\d{6})\b",
        r"(\d{6})\b",
    ):
        for m in re.finditer(pat, body, re.IGNORECASE | re.DOTALL):
            if not _is_hex_color_context(body, m.start(1)):
                return m.group(1)
    return None


def _graph_list_messages(access_token: str, folder: str, timeout: float = 15) -> list:
    """列一个文件夹的最近邮件。

    性能要点（线上 604 秒事故）：
      1. **不拉 body**：`$select=body` 会让 Graph 为每封邮件渲染完整 HTML
         （单封可达数百 KB），15 封就是几 MB，公网慢链路要几百秒。
         这里只取判断所需的轻量字段，命中候选后再单独取正文。
      2. **不加 $orderby**：Graph 对 `receivedDateTime DESC` + 大 $select
         组合会走低效排序计划。默认顺序（最新在前）足够。
    """
    params = urllib.parse.urlencode({
        "$top": "15",
        "$select": "id,subject,bodyPreview,receivedDateTime,from,toRecipients",
    })
    url = f"{GRAPH_BASE}/me/mailFolders/{folder}/messages?{params}"
    req = urllib.request.Request(url, headers={
        "Authorization": f"Bearer {access_token}",
        "Accept": "application/json",
    })
    resp = urllib.request.urlopen(req, timeout=timeout)
    data = json.loads(resp.read())
    value = data.get("value") or []
    return value if isinstance(value, list) else []


def _call_with_hard_timeout(fn, seconds: float, *args, **kwargs):
    """给阻塞调用套一个**硬性墙钟超时**。

    urllib 的 timeout 只管 socket 单次读写：服务器握手后就慢慢滴数据时，
    一次请求可以挂几分钟（线上实测单次 Graph 查信 604 秒）。收码窗口只有
    180 秒，必须能主动放弃这种请求。

    用守护线程执行 + join(seconds)：超时就把结果丢掉返回 None，
    后台线程自行结束（daemon，不阻塞进程退出）。
    """
    import threading as _threading

    box: dict = {}

    def runner():
        try:
            box["value"] = fn(*args, **kwargs)
        except BaseException as exc:  # noqa: BLE001 传回调用方处理
            box["error"] = exc

    t = _threading.Thread(target=runner, daemon=True)
    t.start()
    t.join(seconds)
    if t.is_alive():
        raise TimeoutError(f"调用超过 {seconds:.0f}s 未返回（已放弃）")
    if "error" in box:
        raise box["error"]
    return box.get("value")


def _graph_get_message_rest(access_token: str, message_id: str, timeout: float = 10) -> dict:
    """按 id 取单封邮件正文（已持有 access_token 时用）。"""
    safe_id = urllib.parse.quote(message_id, safe="")
    params = urllib.parse.urlencode({"$select": "id,subject,body,bodyPreview,receivedDateTime,from,toRecipients"})
    req = urllib.request.Request(
        f"{GRAPH_BASE}/me/messages/{safe_id}?{params}",
        headers={"Authorization": f"Bearer {access_token}", "Accept": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read())


def graph_list_messages(cred: dict[str, Any], folder: str = "inbox", top: int = 20) -> list[dict[str, Any]]:
    folder = folder if folder in GRAPH_FOLDERS else "inbox"
    proxy = cred.get("proxy") or ""
    data = _refresh_token_for(cred, GRAPH_SCOPE)
    params = urllib.parse.urlencode({"$top": str(max(1, min(top, 100))), "$orderby": "receivedDateTime DESC", "$select": "id,subject,bodyPreview,receivedDateTime,from,toRecipients,isRead"})
    req = urllib.request.Request(f"{GRAPH_BASE}/me/mailFolders/{folder}/messages?{params}", headers={"Authorization": f"Bearer {data['access_token']}", "Accept": "application/json"})
    with _urlopen(req, timeout=15, proxy=proxy) as resp:
        return json.loads(resp.read()).get("value", [])


def graph_get_message(cred: dict[str, Any], message_id: str) -> dict[str, Any]:
    proxy = cred.get("proxy") or ""
    data = _refresh_token_for(cred, GRAPH_SCOPE)
    safe_id = urllib.parse.quote(message_id, safe="")
    params = urllib.parse.urlencode({"$select": "id,subject,body,bodyPreview,receivedDateTime,from,toRecipients,isRead"})
    req = urllib.request.Request(f"{GRAPH_BASE}/me/messages/{safe_id}?{params}", headers={"Authorization": f"Bearer {data['access_token']}", "Accept": "application/json"})
    with _urlopen(req, timeout=15, proxy=proxy) as resp:
        return json.loads(resp.read())


def _extract_code_from_graph_message(msg: dict) -> str:
    for field in ("bodyPreview", "subject"):
        code = _extract_otp_from_html(str(msg.get(field) or ""))
        if code:
            return code
    body = (msg.get("body") or {}).get("content", "")
    return _extract_otp_from_html(body) or ""


def _extract_code_from_raw(raw: bytes) -> str:
    try:
        msg = _email.message_from_bytes(raw)
    except Exception:
        return ""
    text_body = ""
    for part in msg.walk():
        if part.get_content_type() in ("text/plain", "text/html"):
            try:
                payload = part.get_payload(decode=True) or b""
                text_body += payload.decode(
                    part.get_content_charset() or "utf-8", errors="replace"
                ) + "\n"
            except Exception:
                continue
    return _extract_otp_from_html(text_body) or ""


def _raw_to_text(raw: bytes) -> str:
    """IMAP 原始邮件 → 纯文本（正则提取用）。"""
    try:
        msg = _email.message_from_bytes(raw)
    except Exception:
        return raw.decode("utf-8", errors="replace")
    parts: list[str] = []
    for part in msg.walk():
        if part.get_content_type() in ("text/plain", "text/html"):
            try:
                payload = part.get_payload(decode=True) or b""
                parts.append(payload.decode(
                    part.get_content_charset() or "utf-8", errors="replace"
                ))
            except Exception:
                continue
    return "\n".join(parts)


@register
class OutlookEmailProvider(EmailProvider):
    id = "outlook"
    name = "Outlook 私有邮箱池"
    pooled = True
    ephemeral = False

    config_fields = [
        ConfigField(key="timeout", label="默认收码超时(秒)", type="number", default="120"),
        ConfigField(
            key="default_client_id",
            label="默认 client_id（3 段导入时自动填充）",
            type="text",
            default="",
            placeholder="9e5f94bc-e8a4-4e73-b8be-63364c29d753",
            help="导入 3 段格式 email----password----refresh_token 时自动套用此 client_id；留空则 3 段号入库时 client_id 为空（无法走 Graph/IMAP-XOAUTH2 通道）",
        ),
    ]
    help_text = ("账号格式（每行一个，---- 分隔）:\n"
                 "4 段：email----password----client_id----refresh_token\n"
                 "3 段：email----password----refresh_token（套用默认 client_id）\n"
                 "注：微软已禁用 IMAP 密码登录，2 段格式无法收件，必须带 OAuth 凭据")

    def __init__(self) -> None:
        super().__init__()
        # email -> 凭证 dict（由调用方在 claim 时注入）
        self._credentials: dict[str, dict[str, Any]] = {}
        # 取号瞬间的收件箱基线（message_id 集合 + 时刻），用于挡掉旧信
        self._baseline_ids: set[str] = set()
        self._baseline_at: datetime | None = None

    def set_credentials(self, email: str, cred: dict[str, Any]) -> None:
        self._credentials[email.lower()] = cred

    def set_baseline(self, message_ids: list[str] | set[str] | None, baseline_at: datetime | None) -> None:
        """注入取号瞬间的收件箱基线。

        message_ids：claim 那一刻收件箱里已有的全部 message_id（这些一律视为旧信）
        baseline_at：claim 时刻（UTC aware）；早于它的邮件一律视为旧信
        """
        self._baseline_ids = {str(x) for x in (message_ids or []) if x}
        self._baseline_at = baseline_at

    # ── 验证码提取（项目正则优先）──────────────────────────
    def _fallback_extract_from_text(self, text: str) -> str:
        """Outlook 自带 HTML 提取（排除 hex 颜色上下文），正则未命中时兜底。"""
        return _extract_otp_from_html(text) or super()._fallback_extract_from_text(text)

    def _extract_graph(self, msg: dict[str, Any]) -> str:
        body = (msg.get("body") or {}).get("content", "")
        text = "\n".join([
            str(msg.get("subject") or ""),
            str(msg.get("bodyPreview") or ""),
            body,
        ])
        return self.extract_from_text(text)

    def _extract_raw(self, raw: bytes) -> str:
        return self.extract_from_text(_raw_to_text(raw))

    def _cred(self, email: str) -> dict[str, Any]:
        key = str(email or "").strip().lower()
        cred = self._credentials.get(key)
        if not cred:
            raise ProviderError(f"未找到邮箱凭据: {email}", fatal=True)
        return cred

    # 固定号池由上层（PoolService）管理 claim，generate_address 在此
    # 只承担"从池子拿一个"的语义，实际由 PoolService 调用 set_credentials。
    def generate_address(self) -> str:
        raise ProviderError("Outlook 为固定号池，请通过邮箱池 API 取号", fatal=True)

    # ── Graph API ──────────────────────────────────────────
    def _graph_wait_code(self, cred: dict[str, Any], email: str, timeout: float, skip_codes: set[str] | None = None) -> dict[str, Any] | None:
        data = _refresh_token_for(cred, GRAPH_SCOPE, email=email)
        access_token = data["access_token"]
        seen: set[str] = set()
        deadline = time.time() + timeout
        baseline: set[str] = set(self._baseline_ids)
        baseline_at: datetime | None = self._baseline_at
        stale_skipped = 0
        logger.info(
            f"[outlook-graph] 轮询开始 email={email} folders={GRAPH_FOLDERS} "
            f"基线={len(baseline)}封 baseline_at={baseline_at}"
        )
        while time.time() < deadline:
            round_slow = False
            for folder in GRAPH_FOLDERS:
                if time.time() >= deadline:
                    break
                try:
                    # 单文件夹硬上限 8 秒：3 个文件夹一轮最多 24 秒，
                    # 保证在 180 秒收码窗口内还能轮好几遍。
                    messages = _call_with_hard_timeout(
                        _graph_list_messages, 8, access_token, folder)
                except urllib.error.HTTPError as e:
                    if e.code == 401:
                        logger.info(f"[outlook-graph] 401 token 过期，重新刷新（folder={folder}）")
                        data = _refresh_token_for(cred, GRAPH_SCOPE, email=email)
                        access_token = data["access_token"]
                        continue
                    if e.code in (400, 403):
                        logger.info(f"[outlook-graph] HTTP {e.code} folder={folder}，跳过")
                        continue
                    raise
                except TimeoutError:
                    round_slow = True
                    logger.warning(f"[outlook-graph] folder={folder} 查询超 8s 已放弃，本轮降速")
                    continue
                except Exception:
                    continue
                for msg in messages:
                    mid = msg.get("id", "")
                    if not mid or mid in seen:
                        continue
                    seen.add(mid)
                    # ① 基线判据：取号时就在收件箱里的邮件 = 旧信
                    if mid in baseline:
                        stale_skipped += 1
                        logger.info(f"[outlook-graph] 跳过基线旧信 folder={folder} mid={mid[:20]}...")
                        continue
                    # ② 跨次去重：24h 内已返回过验证码
                    if _is_returned(email, mid):
                        logger.info(f"[outlook-graph] 跳过已返回过验证码的邮件 mid={mid[:20]}... folder={folder}")
                        continue
                    # ③ 时间判据：必须是取号之后到达的（挡掉懒同步带进来的历史邮件）
                    recv = msg.get("receivedDateTime")
                    if not is_fresh_mail(recv, baseline_at):
                        stale_skipped += 1
                        logger.info(
                            f"[outlook-graph] 跳过超时旧信 folder={folder} "
                            f"received={recv} baseline_at={baseline_at}"
                        )
                        continue
                    code = self._extract_graph(msg)
                    if code and code in (skip_codes or set()):
                        # 命中了调用方要求跳过的码，继续看下一封
                        continue

                    # 提取不到 code 也不能丢：z.ai / Slack / Notion 这类站点是
                    # 「点击链接验证」，邮件里只有 URL 没有数字码。以前这里
                    # `if not code: continue` 会把这类邮件直接跳过，客户端等到
                    # 超时都拿不到正文，也就无法自己解析验证链接。
                    # 现在：新鲜+未返回过的新邮件一律捕获，code 允许为空，
                    # 正文照常带回去（客户端自己决定用 code 还是用链接）。
                    _mark_returned(email, mid)
                    sender = ((msg.get("from") or {}).get("emailAddress") or {}).get("address", "")
                    # 列表接口不再带 body（太慢），命中候选后再单独取正文。
                    # 取正文要重试：验证链接只存在于 body 里，取不回来客户端
                    # 就没法解析。给独立预算（2 次 × 15s），不占用等待窗口。
                    body = ""
                    for attempt in range(2):
                        try:
                            full = _call_with_hard_timeout(
                                _graph_get_message_rest, 15, access_token, mid)
                            body = ((full or {}).get("body") or {}).get("content", "") or ""
                            if body:
                                break
                        except Exception as exc:
                            logger.info(
                                f"[outlook-graph] 取正文第 {attempt + 1}/2 次失败: {str(exc)[:80]}"
                            )
                    logger.info(
                        f"[outlook-graph] 命中邮件 folder={folder} mid={mid[:20]}... "
                        f"code={code or '(无,仅链接)'} body={len(body)}B"
                    )
                    return {
                        "code": code,
                        "message_id": mid,
                        "provider": "outlook",
                        "folder": folder,
                        "sender": sender,
                        "subject": str(msg.get("subject") or ""),
                        "body_preview": str(msg.get("bodyPreview") or ""),
                        "body": str(body or ""),
                        "received_at": recv,
                        "code_source": "rule" if code else "none",
                    }
            time.sleep(8 if round_slow else 4)
        logger.info(
            f"[outlook-graph] 轮询超时未取到验证码 email={email}"
            f"（已扫描 {len(seen)} 封，跳过旧信 {stale_skipped} 封）"
        )
        return None

    # ── IMAP ───────────────────────────────────────────────
    def _imap_wait_code(
        self, cred: dict[str, Any], email: str, timeout: float, skip_codes: set[str] | None = None
    ) -> dict[str, Any] | None:
        refresh_token = cred.get("refresh_token") or ""
        client_id = cred.get("client_id") or ""
        use_xoauth2 = bool(client_id and refresh_token)
        if not use_xoauth2:
            # 微软已于 2025-09 全面禁用 Exchange Online 的 IMAP basic auth
            # （实测密码登录返回 'Basic authentication is disabled'），
            # 没有 OAuth 凭据的号没有可用的收件通道，直接 fatal。
            raise ProviderError(
                f"无 OAuth 凭据（client_id/refresh_token），IMAP 密码通道已被微软禁用",
                fatal=True,
            )

        logger.info(
            f"[outlook-imap] 轮询开始 email={email} method=XOAUTH2"
            f" 基线={len(self._baseline_ids)}封 baseline_at={self._baseline_at}"
        )

        cached_token = ""
        cached_at = 0.0
        deadline = time.time() + timeout
        folders = ["INBOX", "Junk", "Junk Email", "Spam"]
        found_folders: list[str] | None = None
        seen: set[tuple] = set()
        stale_skipped = 0

        while time.time() < deadline:
            M = None
            try:
                if not cached_token or time.time() - cached_at > 3000:
                    # 互斥刷新；轮换的新 rt 由 _refresh_token_for 落库。
                    # 刷新失败不再降级密码登录——那是死路，只会白耗 60s
                    # 超时预算并给账号添一次异常登录记录。
                    data = _refresh_token_for(cred, IMAP_SCOPE, email=email)
                    cached_token = data["access_token"]
                    cached_at = time.time()

                last_err: Exception | None = None
                for host in IMAP_SERVERS:
                    if M:
                        break
                    if cached_token:
                        try:
                            M = imaplib.IMAP4_SSL(host, 993, timeout=30)
                            auth_str = (
                                f"user={email}\x01auth=Bearer {cached_token}\x01\x01"
                            )
                            M.authenticate("XOAUTH2", lambda x: auth_str.encode())
                            logger.info(f"[outlook-imap] ✓ XOAUTH2 登录成功 host={host}")
                        except Exception as e:
                            last_err = e
                            logger.info(f"[outlook-imap] ✗ XOAUTH2 登录失败 host={host}: {e}")
                            try:
                                M.logout()
                            except Exception:
                                pass
                            M = None
                if not M:
                    if last_err and _is_fatal_imap_error(last_err):
                        logger.warning(f"[outlook-imap] 致命错误，标记账号失效: {last_err}")
                        raise ProviderError(f"IMAP 登录失败: {last_err}", fatal=True)
                    raise ProviderError(f"IMAP 连接失败: {last_err}")

                if found_folders is None:
                    try:
                        typ, listing = M.list()
                        names_lower: dict[str, str] = {}
                        for raw in listing or []:
                            s = raw.decode(errors="ignore") if isinstance(raw, bytes) else str(raw)
                            m = re.search(r'"([^"]+)"\s*$', s) or re.search(r"\s(\S+)\s*$", s)
                            if m:
                                nm = m.group(1).strip('"')
                                names_lower[nm.lower()] = nm
                        picked: list[str] = []
                        for cand in folders:
                            real = names_lower.get(cand.lower())
                            if real and real not in picked:
                                picked.append(real)
                        for k, v in names_lower.items():
                            if any(x in k for x in ("junk", "spam", "bulk")) and v not in picked:
                                picked.append(v)
                        if "INBOX" not in picked:
                            picked.insert(0, "INBOX")
                        found_folders = picked
                        logger.info(f"[outlook-imap] 发现文件夹: {found_folders}")
                    except Exception:
                        found_folders = list(folders)
                        logger.info(f"[outlook-imap] LIST 失败，使用默认文件夹: {found_folders}")

                for folder in found_folders:
                    if time.time() >= deadline:
                        logger.info(f"[outlook-imap] 时间预算用尽，停止扫描（已扫描 {len(seen)} 封）")
                        break
                    try:
                        sel_arg = f'"{folder}"' if " " in folder else folder
                        typ, _ = M.select(sel_arg, readonly=True)
                        if typ != "OK":
                            continue
                    except Exception:
                        continue
                    try:
                        typ, data = M.search(None, "ALL")
                        ids = data[0].split() if data and data[0] else []
                    except Exception:
                        continue
                    # 先用 SEARCH 结果里最近的 8 封做候选；只 fetch 真正需要的
                    for mid in reversed(ids[-8:]):
                        # 每封都检查预算：一次 IMAP fetch 可能就要几秒，
                        # 原来只在 while 顶部检查，导致 timeout=5 实际跑 18 秒。
                        if time.time() >= deadline:
                            logger.info(f"[outlook-imap] 时间预算用尽（扫描中），停止")
                            break
                        key = (folder, mid)
                        if key in seen:
                            continue
                        seen.add(key)
                        # 跨次去重：跳过 24h 内已返回过验证码的邮件
                        mid_str = mid.decode(errors="ignore") if isinstance(mid, bytes) else str(mid)
                        if _is_returned(email, mid_str):
                            logger.info(f"[outlook-imap] 跳过已返回过验证码的邮件 mid={mid_str[:20]}... folder={folder}")
                            continue
                        # ① 基线判据（不需要 fetch 就能判断，放前面省时间）
                        if mid_str in self._baseline_ids:
                            stale_skipped += 1
                            logger.info(f"[outlook-imap] 跳过基线旧信 folder={folder} mid={mid_str[:20]}...")
                            continue
                        try:
                            typ, raw = M.fetch(mid, "(BODY.PEEK[])")
                            msg = _email.message_from_bytes(raw[0][1])
                            code = self._extract_raw(raw[0][1])
                        except Exception:
                            continue
                        # ② 时间判据：必须是取号之后到达的
                        mail_date = msg.get("Date")
                        if not is_fresh_mail(mail_date, self._baseline_at):
                            stale_skipped += 1
                            logger.info(
                                f"[outlook-imap] 跳过超时旧信 folder={folder} "
                                f"date={mail_date} baseline_at={self._baseline_at}"
                            )
                            continue
                        # 提取不到 code 也要返回：链接验证型邮件（z.ai 等）
                        # 正文里只有 URL，客户端需要原文自己解析。
                        if code and code in (skip_codes or set()):
                            continue
                        _mark_returned(email, mid_str)
                        logger.info(
                            f"[outlook-imap] 命中邮件 folder={folder!r} "
                            f"code={code or '(无,仅链接)'}"
                        )
                        sender = str(msg.get("From") or "")
                        subject = str(msg.get("Subject") or "")
                        body = _raw_to_text(raw[0][1])
                        try:
                            M.logout()
                        except Exception:
                            pass
                        return {
                            "code": code,
                            "message_id": mid_str,
                            "provider": "outlook",
                            "folder": folder,
                            "sender": sender,
                            "subject": subject,
                            "body_preview": body[:500],
                            "body": body,
                            "received_at": mail_date,
                            "code_source": "rule" if code else "none",
                        }
                try:
                    M.logout()
                except Exception:
                    pass
            except ProviderError:
                raise
            except Exception as e:
                if _is_fatal_imap_error(e):
                    raise ProviderError(f"IMAP 不可用 {email}: {e}", fatal=True)
                logger.warning(f"[outlook-imap] 异常 (重试): {e}")
                if M:
                    try:
                        M.logout()
                    except Exception:
                        pass
            time.sleep(4)
        logger.info(f"[outlook-imap] 轮询超时未取到验证码 email={email}（已扫描 {len(seen)} 封，跳过旧信 {stale_skipped} 封）")
        return None

    async def wait_code(
        self, email: str, *, timeout: float = 120, project: dict[str, Any] | None = None
    ) -> str | None:
        mail = await self.wait_mail(email, timeout=timeout, project=project)
        return str(mail.get("code") or "") if mail else None

    async def wait_mail(
        self, email: str, *, timeout: float = 120, project: dict[str, Any] | None = None
    ) -> dict[str, Any] | None:
        cred = self._cred(email)
        skip_codes = {str(code).strip() for code in (project or {}).get("skip_codes", []) if str(code).strip()}
        return await asyncio.to_thread(self._wait_mail_sync, email, cred, timeout, skip_codes)

    def _wait_mail_sync(
        self, email: str, cred: dict[str, Any], timeout: float, skip_codes: set[str]
    ) -> dict[str, Any] | None:
        """先走 Graph，**失败才**降级 IMAP，整体共享一个时间预算。

        踩过的坑：
        1. 原来两条通道各自跑满 timeout → timeout=5 实际用了 16.6 秒。
        2. 更严重的是 Graph 正常跑完（只是没新信）后还会再走一遍 IMAP，
           等于同一个邮箱扫两遍，而 IMAP 刷新 token 本身就要 10~15 秒。
           现在 Graph 成功执行就不再走 IMAP（同一邮箱，内容一样）。
        """
        started = time.time()
        logger.info(f"[mail] 取件开始 email={email} timeout={timeout}s")
        has_graph = bool(cred.get("client_id") and cred.get("refresh_token"))
        graph_error: Exception | None = None
        if has_graph:
            try:
                mail = self._graph_wait_code(cred, email, timeout, skip_codes)
                if mail:
                    return mail
                # Graph 正常完成但没新信：直接返回，不再用 IMAP 重扫同一个箱子
                logger.info(
                    f"[mail] Graph 通道无新信（耗时 {time.time() - started:.1f}s），"
                    f"跳过 IMAP 重复扫描"
                )
                return None
            except Exception as e:
                graph_error = e
                logger.info(f"[mail] Graph 通道异常，降级 IMAP：{type(e).__name__}: {e}")

        remaining = timeout - (time.time() - started)
        if remaining < 1.0:
            logger.info(f"[mail] 时间预算已用尽（Graph 失败且无剩余预算），直接返回 None")
            return None
        try:
            mail = self._imap_wait_code(cred, email, remaining, skip_codes)
            if mail:
                return mail
        except Exception:
            if graph_error is not None:
                raise
            raise
        return None

    def self_test(self) -> dict[str, Any]:
        return {"ok": True, "message": f"{self.name} 通过邮箱池导入账号后可用"}
