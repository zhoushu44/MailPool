"""邮箱池服务：固定号池（outlook 等）的导入 / claim / 释放 / 健康检查 / 心跳保活 / Token 刷新。

邮箱池记录存在 email_accounts 表。claim 逻辑：
    available -> using（占用中）；release 后永久 retired；
    失效/凭证错误标记 dead 并 retired。
"""
from __future__ import annotations

import json
import logging
import random
import re
import threading
import time
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from typing import Any

from sqlalchemy import func, select, update
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Session

from ..models import EmailAccount, EmailUsage, MailBaseline, TokenRefreshLog
from ..providers import ProviderError
from ..timeutil import normalize_utc, to_naive_utc

logger = logging.getLogger(__name__)

# Thunderbird 桌面版公开 client_id
THUNDERBIRD_CLIENT_ID = "9e5f94bc-e8a4-4e73-b8be-63364c29d753"
REFRESH_SCOPE = "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access"
TOKEN_URL = "https://login.microsoftonline.com/common/oauth2/v2.0/token"
DEVICE_CODE_URL = "https://login.microsoftonline.com/common/oauth2/v2.0/devicecode"

# Device Code Flow 进行中的状态（进程内内存，重启丢失，仅用于轮询等待）
_device_flows: dict[int, dict[str, Any]] = {}
_device_flows_lock = threading.Lock()


def _now() -> datetime:
    return datetime.now(timezone.utc)


def _naive_utc(dt: datetime | None) -> datetime | None:
    """统一成 naive UTC 再比较（见 app/timeutil.py 的详细说明）。"""
    return to_naive_utc(dt)


# 连续失败阈值：达到自动降级
FAIL_UNHEALTHY_THRESHOLD = 3   # 连续失败 3 次 → 降为 unhealthy
FAIL_DEAD_THRESHOLD = 6        # 连续失败 6 次 → 降为 dead（彻底失效）
CLAIM_LEASE_TIMEOUT = timedelta(minutes=15)
# 可对外发放的健康状态：未知（新导入未检测）与已确认健康；unhealthy/dead 一律不发
CLAIMABLE_HEALTH_STATUSES = ("healthy", "unknown")
# 最近一次导入复活的账号数（供 admin 接口展示）
_IMPORT_LAST_REVIVED: dict[str, int] = {"count": 0}
_OPERATION_STATS = {
    "acquire_success": 0,
    "acquire_failed": 0,
    "receive_success": 0,
    "receive_timeout": 0,
    "release_count": 0,
    "health_failure": 0,
}
_OPERATION_STATS_LOCK = threading.Lock()


def record_operation(name: str, amount: int = 1) -> None:
    if name in _OPERATION_STATS:
        with _OPERATION_STATS_LOCK:
            _OPERATION_STATS[name] += amount


def operation_stats() -> dict[str, int]:
    with _OPERATION_STATS_LOCK:
        return dict(_OPERATION_STATS)


def _sync_legacy_status(acc: EmailAccount) -> str:
    """同步旧 status 字段，保持已有 API/UI 兼容。"""
    if acc.pool_status == "using":
        acc.status = "using"
    elif acc.pool_status == "retired":
        acc.status = "retired"
    elif acc.health_status in ("healthy", "unhealthy", "dead"):
        acc.status = acc.health_status
    else:
        acc.status = "available"
    return acc.status


# 真正的风控（封号）特征：只有这些才允许触发"自动删除 / 直接置 dead"。
# 注意：429（限流）、403（可能只是临时拒绝）属于临时性错误，绝不能当风控，
# 否则批量刷新时一次限流就能把大批账号判死甚至物理删除。
_RISK_CONTROL_TOKENS = (
    "service abuse mode",
    "service_abuse",
    "aadsts70000",
    "service abuse",
)
# 临时性/限流类错误：只累加失败次数，不升级为风控
_TRANSIENT_TOKENS = (
    "429", "too many requests", "rate limit", "throttl",
    "403", "forbidden", "timeout", "timed out", "connection",
    "network", "503", "502", "504", "temporarily", "try again",
    "连接", "超时",
)


def _classify_health_error(detail: str) -> str:
    """根据错误详情分类健康状态原因。

    优先级：risk_control > credential > transient > check_failed
    （transient 包含限流/网络/5xx，属可恢复错误）
    """
    text = str(detail or "").lower()

    # 1. 真风控（service abuse）—— 唯一的"严重"分类
    if any(token in text for token in _RISK_CONTROL_TOKENS):
        return "risk_control"

    # 2. 凭证错误（token 失效等）—— 可通过重新授权修复
    if any(token in text for token in ("invalid_grant", "credential", "401", "unauthorized")):
        return "credential"

    # 3. 临时性错误（限流 / 网络 / 网关）—— 不应升级为 dead
    if any(token in text for token in _TRANSIENT_TOKENS):
        return "transient"

    return "check_failed"


def is_risk_control(detail: str) -> bool:
    """严格判断是否命中真风控（用于自动删除决策）。"""
    text = str(detail or "").lower()
    return any(token in text for token in _RISK_CONTROL_TOKENS)


def _apply_check_result(
    acc: EmailAccount,
    ok: bool,
    detail: str,
    db: Session | None = None,
    auto_delete_risk: bool = False,
) -> str:
    """根据检测结果更新健康维度。返回最终 health_status。

    auto_delete_risk 默认 False：检测到风控（service abuse）只做标记（dead + retired），
    不再物理删除。物理删除不可逆，一旦误判（历史 bug 曾把 429 限流当风控）就是批量丢号，
    因此改为"默认只标记"，需要清理时由人工在界面/脚本确认。

    临时性错误（429 限流 / 网络超时 / 5xx）不累加到 dead 阈值，只标记 unhealthy，
    避免一次限流把大批账号判死。
    """
    acc.check_result = detail[:255]
    acc.health_reason = "" if ok else _classify_health_error(detail)
    acc.last_check_at = _now()

    # 真风控：标记 dead（默认不删；只有显式传 auto_delete_risk=True 才物理删除）
    if not ok and is_risk_control(detail):
        logger.warning(f"[health] {acc.email} 检测到风控 (service abuse)")
        if auto_delete_risk and db is not None:
            logger.warning(f"[health] {acc.email} auto_delete_risk=True，物理删除")
            delete_account(db, account_id=acc.id)
            return "deleted"
        acc.health_status = "dead"
        acc.health_reason = "risk_control"
        acc.pool_status = "retired"
        acc.claimed_at = None
        _sync_legacy_status(acc)
        return "dead"

    if ok:
        prev_fail = acc.fail_count or 0
        acc.fail_count = 0
        acc.health_status = "healthy"
        # 之前因失败被退役的账号：恢复正常后允许重新入池
        if acc.pool_status == "retired":
            acc.pool_status = "available"
            logger.info(f"[health] {acc.email} 恢复健康，pool_status retired → available")
        if prev_fail > 0:
            logger.info(f"[health] {acc.email} 恢复正常（fail_count {prev_fail} → 0）")
    else:
        record_operation("health_failure")
        prev_fail = acc.fail_count or 0
        acc.fail_count = prev_fail + 1
        reason = acc.health_reason

        if reason == "transient":
            # 临时错误：只标记 unhealthy，不计入 dead 阈值
            acc.health_status = "unhealthy"
            logger.info(
                f"[health] {acc.email} 临时性错误（{detail[:60]}），"
                f"标记 unhealthy 但不累计失效（fail_count={acc.fail_count}）"
            )
        elif acc.fail_count >= FAIL_DEAD_THRESHOLD:
            acc.health_status = "dead"
            acc.pool_status = "retired"
            acc.claimed_at = None
            logger.info(f"[health] {acc.email} 累计失败 {acc.fail_count} 次 → dead")
        elif acc.fail_count >= FAIL_UNHEALTHY_THRESHOLD or acc.health_status == "healthy":
            acc.health_status = "unhealthy"
        logger.info(f"[health] {acc.email} 检测失败 fail_count={acc.fail_count} health={acc.health_status} reason={reason}")
    _sync_legacy_status(acc)
    return acc.health_status


def parse_account_line(line: str, provider: str) -> dict | None:
    """解析一行号池文本为 account dict。

    outlook 格式（3 种）：
      4 段：email----password----client_id----refresh_token（Graph API）
      3 段：email----password----refresh_token（用默认 client_id）
      2 段：email----password（IMAP 密码登录，微软可能禁用）
    其他 provider 格式：email----extra...
    """
    text = str(line or "").strip()
    if not text or text.startswith("#"):
        return None
    parts = text.split("----")
    email = str(parts[0] or "").strip().lower()
    if not re.match(r"^[^\s@]+@[^\s@]+\.[^\s@]+$", email):
        return None
    if provider == "outlook":
        if len(parts) == 4:
            return {
                "email": email,
                "password": parts[1].strip(),
                "client_id": parts[2].strip(),
                "refresh_token": parts[3].strip(),
            }
        elif len(parts) == 3:
            return {
                "email": email,
                "password": parts[1].strip(),
                "client_id": "",
                "refresh_token": parts[2].strip(),
            }
        elif len(parts) == 2:
            return {
                "email": email,
                "password": parts[1].strip(),
                "client_id": "",
                "refresh_token": "",
            }
        return None
    # 通用：多余段保留为 seg1...
    extra: dict[str, str] = {}
    for i, p in enumerate(parts[1:], start=1):
        extra[f"seg{i}"] = p.strip()
    return {"email": email, **extra}


def parse_account_text(text: str, provider: str) -> tuple[list[dict], list[str]]:
    """批量解析导入文本。返回 (ok_rows, errors)。"""
    ok_rows: list[dict] = []
    errors: list[str] = []
    for n, raw in enumerate(str(text or "").splitlines(), 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        row = parse_account_line(line, provider)
        if row is None:
            errors.append(f"第 {n} 行：格式无效")
            continue
        ok_rows.append(row)
    return ok_rows, errors


def import_accounts(
    db: Session,
    provider: str,
    rows: list[dict[str, Any]],
    remark: str = "",
) -> tuple[int, int, int]:
    """批量导入账号。返回 (成功数, 跳过重复数, 默认 client_id 套用数)。"""
    logger.info(f"[import] 开始导入 provider={provider} rows={len(rows)} remark={remark!r}")
    # outlook 3 段格式：套用 provider_config.outlook.default_client_id
    default_cid = ""
    if provider == "outlook":
        from .settings_service import get_provider_config
        default_cid = str(get_provider_config(db, "outlook").get("default_client_id") or "").strip()
        if default_cid:
            logger.info(f"[import] 已配置默认 client_id={default_cid[:8]}...{default_cid[-4:]}，3 段号将自动套用")
        else:
            logger.info("[import] 未配置默认 client_id，3 段号入库时 client_id 为空（无法走 Graph/IMAP-XOAUTH2）")

    inserted = 0
    skipped = 0
    applied_default_cid = 0
    revived = 0
    for row in rows:
        email = row["email"]
        exists = db.scalar(
            select(EmailAccount).where(EmailAccount.email == email)
        )
        if exists:
            # dead / retired 的账号重新导入：视为「人工补了可用凭证」，
            # 更新凭证并复活，而不是简单跳过（否则死号永远救不回来）。
            if exists.pool_status == "retired" or exists.health_status in ("dead", "unhealthy"):
                new_cred = {k: v for k, v in row.items() if k != "email"}
                if provider == "outlook" and default_cid and not new_cred.get("client_id") and new_cred.get("refresh_token"):
                    new_cred["client_id"] = default_cid
                    applied_default_cid += 1
                exists.provider = provider
                exists.account_json = __import__("json").dumps(new_cred, ensure_ascii=False)
                exists.pool_status = "available"
                exists.health_status = "unknown"
                exists.status = "available"
                exists.fail_count = 0
                exists.health_reason = ""
                exists.check_result = "重新导入恢复"
                exists.claimed_at = None
                if remark:
                    exists.remark = remark
                revived += 1
                logger.info(
                    f"[import] 已复活账号 email={email}（原 retired/dead → available/unknown）"
                )
                continue
            skipped += 1
            continue
        # outlook 3 段号（无 client_id 但有 refresh_token）→ 套用默认 client_id
        if provider == "outlook" and default_cid:
            if not row.get("client_id") and row.get("refresh_token"):
                row["client_id"] = default_cid
                applied_default_cid += 1
        db.add(EmailAccount(
            email=email,
            provider=provider,
            account_json=__import__("json").dumps(
                {k: v for k, v in row.items() if k != "email"}, ensure_ascii=False
            ),
            status="available",
            pool_status="available",
            health_status="unknown",
            remark=remark,
        ))
        inserted += 1
    db.commit()
    logger.info(
        f"[import] 导入完成 inserted={inserted} revived={revived} skipped={skipped} "
        f"applied_default_cid={applied_default_cid}"
    )
    # 第三个返回值沿用「默认 client_id 套用数」契约，复活数通过日志/接口统计呈现
    _IMPORT_LAST_REVIVED["count"] = revived
    return inserted, skipped, applied_default_cid


def list_accounts(
    db: Session,
    provider: str | None = None,
    status: str | None = None,
    pool_status: str | None = None,
    health_status: str | None = None,
    keyword: str | None = None,
    tag: str | None = None,
    limit: int = 200,
    offset: int = 0,
) -> list[EmailAccount]:
    stmt = select(EmailAccount).order_by(EmailAccount.id.desc())
    if provider:
        stmt = stmt.where(EmailAccount.provider == provider)
    if status:
        if status in ("available", "using"):
            stmt = stmt.where(EmailAccount.pool_status == status)
        elif status in ("unknown", "healthy", "unhealthy", "dead"):
            stmt = stmt.where(EmailAccount.health_status == status)
        elif status == "retired":
            stmt = stmt.where(EmailAccount.pool_status == "retired")
        else:
            stmt = stmt.where(EmailAccount.status == status)
    if pool_status:
        stmt = stmt.where(EmailAccount.pool_status == pool_status)
    if health_status:
        stmt = stmt.where(EmailAccount.health_status == health_status)
    if keyword:
        stmt = stmt.where(EmailAccount.email.like(f"%{keyword}%"))
    if tag:
        # tag 存在逗号分隔字段里，SQL 无法精确匹配，只能先全量取再过滤
        accounts = list(db.scalars(stmt))
        tag = tag.strip()
        accounts = [a for a in accounts if tag in [t.strip() for t in (a.tags or "").split(",")]]
        accounts = accounts[offset:offset + limit]
    else:
        # 分页下推到 SQL：原来先把整表 995 行全部 ORM 实例化再切片，
        # 每次 /pool 请求都要反序列化近千个对象（约 700ms）。
        stmt = stmt.limit(limit).offset(offset)
        accounts = list(db.scalars(stmt))
    return accounts


def count_accounts(db: Session, provider: str | None = None) -> dict[str, int]:
    stmt = select(
        EmailAccount.provider,
        EmailAccount.pool_status,
        EmailAccount.health_status,
    )
    if provider:
        stmt = stmt.where(EmailAccount.provider == provider)
    rows = db.execute(stmt).all()
    out = {
        "total": len(rows), "available": 0, "using": 0, "retired": 0, "dead": 0,
        "healthy": 0, "unhealthy": 0, "unknown": 0, "by_provider": {},
    }
    for provider_id, pool_status, health_status in rows:
        out["by_provider"].setdefault(provider_id, 0)
        out["by_provider"][provider_id] += 1
        if pool_status in ("available", "using", "retired"):
            out[pool_status] += 1
        if health_status in ("healthy", "unhealthy", "dead", "unknown"):
            out[health_status] += 1
    return out


# ────────────────────────────────────────────────────────────────
#  邮箱使用绑定（防跨项目复用）
# ────────────────────────────────────────────────────────────────


def email_usage_map(db: Session) -> dict[str, str]:
    """返回 {email: "项目A,项目B"} 的绑定映射（一个邮箱可能绑了多个项目）。"""
    out: dict[str, list[str]] = {}
    for u in db.scalars(select(EmailUsage)):
        out.setdefault(u.email.lower(), []).append(u.project_code)
    return {email: ",".join(codes) for email, codes in out.items()}


def email_usage_for(db: Session, emails: list[str]) -> dict[str, list[str]]:
    """按邮箱列表查绑定映射 {email: [project_code, ...]}。"""
    keys = [e.strip().lower() for e in emails if e and e.strip()]
    if not keys:
        return {}
    out: dict[str, list[str]] = {}
    for u in db.scalars(select(EmailUsage).where(EmailUsage.email.in_(keys))):
        out.setdefault(u.email.lower(), []).append(u.project_code)
    return out


def bind_email(db: Session, email: str, project_code: str) -> tuple[bool, str | None]:
    """绑定邮箱到项目。返回 (ok, conflict_project)。

    按项目一次性：同一邮箱同一项目只能绑一次（重复取 → 冲突）；
    其他项目绑同一邮箱不受影响。
    """
    key = (email or "").strip().lower()
    if not key:
        return False, None
    try:
        db.add(EmailUsage(email=key, project_code=project_code))
        db.commit()
        return True, None
    except IntegrityError:
        db.rollback()
        usage = db.scalar(
            select(EmailUsage).where(
                EmailUsage.email == key, EmailUsage.project_code == project_code
            )
        )
        return False, usage.project_code if usage else project_code


def used_by_other_project(db: Session, email: str, project_code: str) -> bool:
    """该邮箱是否已被**本项目**使用过（按项目一次性）。"""
    key = (email or "").strip().lower()
    if not key:
        return False
    return db.scalar(
        select(EmailUsage.id).where(
            EmailUsage.email == key, EmailUsage.project_code == project_code
        )
    ) is not None


def _probe_account(acc: "EmailAccount") -> tuple[bool, str]:
    """取号前的实时凭据预检：刷新一次 token，判断号还活着没。

    返回 (alive, reason)。网络抖动等临时错误视为 alive=True
    （不放死号进池，但也不因为一次网络问题就误杀号）。

    只对 outlook（OAuth 号池）生效：其它 provider 的账号格式不同，
    强行按 OAuth 解析会全部误判为死号。
    """
    import json as _json

    if (acc.provider or "").lower() != "outlook":
        return True, ""

    try:
        cred = _json.loads(acc.account_json or "{}")
    except (_json.JSONDecodeError, TypeError):
        return False, "credential_invalid: account_json 解析失败"
    client_id = (cred.get("client_id") or "").strip()
    refresh_token = (cred.get("refresh_token") or "").strip()
    if not client_id or not refresh_token:
        return False, "credential_invalid: 缺 client_id 或 refresh_token"
    if acc.proxy:
        cred["proxy"] = acc.proxy

    try:
        from ..providers.outlook import GRAPH_SCOPE, _refresh_token_for

        cred["email"] = acc.email
        _refresh_token_for(cred, GRAPH_SCOPE, email=acc.email)
        # 轮换出的新 rt 需要落库
        if cred.get("refresh_token") and cred["refresh_token"] != refresh_token:
            acc.account_json = _json.dumps(cred, ensure_ascii=False)
        return True, ""
    except Exception as exc:
        msg = str(exc)
        low = msg.lower()
        if "service abuse" in low or "aadsts70000" in low:
            return False, "risk_control: service abuse mode"
        if "invalid_grant" in low or "invalid_client" in low:
            return False, f"credential_invalid: {msg[:100]}"
        # 429/超时/连接类 → 不判死
        logger.info(f"[claim] 预检遇到临时错误，放行 email={acc.email} err={msg[:80]}")
        return True, ""


def claim_account(db: Session, provider: str, project_code: str = "") -> dict[str, Any] | None:
    """从号池 claim 一个 available 账号，标记 using。返回凭证 dict。

    按项目一次性：只跳过**本项目**用过（已绑定）的邮箱，
    其他项目用过的照样能取。
    只发「healthy / unknown」的号：unhealthy（检测失败过）和 dead 一律不发，
    避免把已知有问题的账号发给客户造成取件失败。
    """
    logger.info(f"[claim] 取号请求 provider={provider} project={project_code}")
    recover_abandoned_claims(db)
    taken = select(EmailUsage.email).where(EmailUsage.project_code == project_code)
    stmt = (
        select(EmailAccount)
        .where(
            EmailAccount.provider == provider,
            EmailAccount.pool_status == "available",
            EmailAccount.health_status.in_(CLAIMABLE_HEALTH_STATUSES),
            EmailAccount.email.not_in(taken),
        )
        # 优先发最近验证过健康的号：last_check_at 越新越靠前，
        # 避免 id 升序导致老号（更容易被微软盯上）一直排在最前面。
        .order_by(
            EmailAccount.last_check_at.desc().nulls_last(),
            EmailAccount.id.asc(),
        )
        .limit(50)
    )
    # 微软会静默把号打成 service abuse mode（token 全废但账号还在），
    # 而健康扫描按天轮转覆盖不到全池。所以取号时**实时验一次 token**：
    # 死号当场标记并跳过，绝不把读不了信的号发给客户。
    #
    # 顺序很关键：**先原子占位，再验号**。
    # 反过来（先验再占）在并发下会退化成「N 个线程一起验同一批候选、
    # 只有一个抢得到」：白烧 N 倍 token 刷新，抢输的线程若就此放弃，
    # 库存充足也会误报 D0002。实测（828 库存的池子）：
    #   串行 12 → 12/12；并发 5 → 5/5；并发 9 → 8/9；并发 10 → 8/10；
    #   并发 20 → 8/20。上限恰好是 PROBE_PER_ROUND(8)，不是库存。
    PROBE_PER_ROUND = 8
    MAX_ROUNDS = 20
    for _round in range(MAX_ROUNDS):
        candidates = list(db.scalars(stmt.limit(PROBE_PER_ROUND)))
        if not candidates:
            break
        for acc in candidates:
            # 原子占位：仅当账号仍是 available 时才置为 using，
            # 靠 SQLite 写锁保证并发下不会重复取走同一账号（防止丢失更新）。
            res = db.execute(
                update(EmailAccount)
                .where(EmailAccount.id == acc.id, EmailAccount.pool_status == "available")
                .values(pool_status="using", status="using", claimed_at=_now())
            )
            db.commit()
            if res.rowcount != 1:
                # 已被别的线程/进程抢走 → 不浪费一次 token 刷新，直接看下一个
                continue

            # 占位成功，这个号归我了，只需要验这一次。
            #
            # 关键：预检是一次**网络调用**（刷新 token，几秒级）。这里必须先
            # 把 ORM 对象从 session 上摘下来（expunge），否则 refresh/懒加载
            # 会在预检期间一直占着一条池连接 —— 50 并发时 5+10 的连接池被
            # 占满，其余请求等 30 秒后抛
            # `QueuePool limit of size 5 overflow 10 reached` → HTTP 500。
            # commit 本身已把连接还池，所以摘掉之后预检期间不占连接。
            db.expunge(acc)
            alive, reason = _probe_account(acc)
            if not alive:
                logger.warning(
                    f"[claim] 预检失败，标记死号并跳过 email={acc.email} reason={reason[:80]}"
                )
                db.execute(
                    update(EmailAccount)
                    .where(EmailAccount.id == acc.id)
                    .values(
                        pool_status="retired",
                        status="retired",
                        health_status="dead",
                        health_reason=reason[:120],
                        last_check_at=_now(),
                    )
                )
                db.commit()
                continue
            # 预检可能轮换出新的 refresh_token，必须落库（否则下次刷新用旧 rt 会失效）
            values: dict[str, Any] = {"last_check_at": _now()}
            if acc.account_json:
                values["account_json"] = acc.account_json
            db.execute(update(EmailAccount).where(EmailAccount.id == acc.id).values(**values))
            db.commit()
            logger.info(f"[claim] ✓ 取号成功 email={acc.email} account_id={acc.id}")
            return {"email": acc.email, "account_id": acc.id}
    logger.info(f"[claim] ✗ 无可用号 provider={provider}（已扫 {MAX_ROUNDS} 轮候选）")
    return None


def count_claimable(db: Session, provider: str, project_code: str = "") -> int:
    """该项目可 claim 数量：available + healthy/unknown 且**未被本项目用过**的邮箱数。

    与 claim_account 的口径必须一致，否则页面显示"有 1000 个"但实际取不到。
    按项目算：A 项目用过的号只从 A 的库存里扣，其他项目库存不变。
    用 SQL COUNT 聚合，不再把全部 id 拉到 Python 里数（995 行 × 53 次调用 = 秒级）。
    """
    taken = select(EmailUsage.email).where(EmailUsage.project_code == project_code)
    return int(
        db.scalar(
            select(func.count(EmailAccount.id)).where(
                EmailAccount.provider == provider,
                EmailAccount.pool_status == "available",
                EmailAccount.health_status.in_(CLAIMABLE_HEALTH_STATUSES),
                EmailAccount.email.not_in(taken),
            )
        )
        or 0
    )


def recover_abandoned_claims(db: Session) -> dict[str, int]:
    """处理超时占用：一律回池 available。

    按项目一次性语义下，被某项目绑定的邮箱对其他项目仍可用，
    所以不再退役 —— 绑定记录（email_usages）保留，本项目取不到即可。
    """
    cutoff = _naive_utc(_now()) - CLAIM_LEASE_TIMEOUT
    accounts = list(db.scalars(
        select(EmailAccount).where(
            EmailAccount.pool_status == "using",
            EmailAccount.claimed_at.is_not(None),
            EmailAccount.claimed_at < cutoff,
        )
    ))
    if not accounts:
        return {"recovered": 0, "retired": 0}

    for acc in accounts:
        acc.pool_status = "available"
        acc.claimed_at = None
        _sync_legacy_status(acc)
    db.commit()
    logger.info(f"[claim-recovery] recovered={len(accounts)}")
    return {"recovered": len(accounts), "retired": 0}


def release_account(db: Session, account_id: int | None = None, email: str | None = None) -> bool:
    """释放已领取账号，**回池**给其他项目继续用。

    按项目一次性：释放只解除占用（using → available），
    本项目的使用绑定（email_usages）保留 —— 该项目再也取不到这个号，
    但其他项目照常可取。账号不退役。
    """
    conditions = []
    if account_id:
        conditions.append(EmailAccount.id == account_id)
    elif email:
        conditions.append(EmailAccount.email == email.lower())
    else:
        logger.info("[release] 参数为空（无 account_id 和 email），跳过")
        return False
    res = db.execute(
        update(EmailAccount)
        .where(*conditions, EmailAccount.pool_status == "using")
        .values(pool_status="available", claimed_at=None)
    )
    db.commit()
    if res.rowcount == 1:
        # 同步旧 status 字段（跟随 health_status），保持 UI/API 兼容
        acc = db.scalar(select(EmailAccount).where(*conditions))
        if acc is not None:
            _sync_legacy_status(acc)
            db.commit()
        logger.info(f"[release] 释放回池成功 account_id={account_id} email={email}")
        return True
    # 未命中「using」状态：账号存在但本来就不在占用中，不能假装释放成功。
    acc = db.scalar(select(EmailAccount).where(*conditions))
    if acc is not None:
        logger.info(
            f"[release] 账号 {acc.email} 当前 pool_status={acc.pool_status}，非 using，跳过"
        )
        return False
    return False


def revive_account(
    db: Session,
    account_id: int | None = None,
    email: str | None = None,
    health_status: str = "unknown",
) -> dict[str, Any] | None:
    """把 dead / retired 的账号复活为可领取状态（人工确认后使用）。

    - 清空 fail_count / health_reason / check_result
    - pool_status → available，health_status → unknown（需重新检测确认）
    - 如果该邮箱已被某个项目使用过（email_usages 有记录），仍然复活，
      因为这属于人工干预（例如重新授权拿到了可用凭证），但会在返回值里标记 used_by。

    返回 {email, account_id, pool_status, health_status, used_by}；账号不存在返回 None。
    """
    stmt = select(EmailAccount)
    if account_id:
        stmt = stmt.where(EmailAccount.id == account_id)
    elif email:
        stmt = stmt.where(EmailAccount.email == email.lower())
    else:
        return None
    acc = db.scalar(stmt)
    if acc is None:
        return None

    prev = (acc.pool_status, acc.health_status)
    acc.pool_status = "available"
    acc.health_status = health_status if health_status in ("unknown", "healthy") else "unknown"
    acc.fail_count = 0
    acc.health_reason = ""
    acc.check_result = "人工恢复"
    acc.claimed_at = None
    _sync_legacy_status(acc)
    db.commit()

    used_by = db.scalar(
        select(EmailUsage.project_code).where(EmailUsage.email == acc.email.lower())
    )
    logger.info(
        f"[revive] 账号已恢复 email={acc.email} {prev[0]}/{prev[1]} → "
        f"{acc.pool_status}/{acc.health_status} used_by={used_by or '-'}"
    )
    return {
        "email": acc.email,
        "account_id": acc.id,
        "pool_status": acc.pool_status,
        "health_status": acc.health_status,
        "used_by": used_by or "",
    }


def revive_dead_accounts(
    db: Session,
    provider: str = "outlook",
    health_reason: str | None = None,
    dry_run: bool = True,
    limit: int = 500,
) -> dict[str, Any]:
    """批量把 dead 账号恢复为 available/unknown（默认只预览，不落库）。

    典型用途：一次限流把大批号判成 dead 后批量救回，再重新做健康检测。
    """
    stmt = (
        select(EmailAccount)
        .where(
            EmailAccount.provider == provider,
            EmailAccount.health_status == "dead",
        )
        .order_by(EmailAccount.id.asc())
        .limit(limit)
    )
    if health_reason:
        stmt = stmt.where(EmailAccount.health_reason == health_reason)
    accounts = list(db.scalars(stmt))
    preview = [
        {
            "id": a.id,
            "email": a.email,
            "health_reason": a.health_reason,
            "pool_status": a.pool_status,
            "fail_count": a.fail_count,
        }
        for a in accounts
    ]
    if dry_run:
        return {"dry_run": True, "candidates": len(preview), "accounts": preview[:200]}

    for acc in accounts:
        acc.pool_status = "available"
        acc.health_status = "unknown"
        acc.status = "available"
        acc.fail_count = 0
        acc.health_reason = ""
        acc.check_result = "批量人工恢复"
        acc.claimed_at = None
    db.commit()
    logger.info(f"[revive-batch] 已恢复 {len(accounts)} 个 dead 账号（provider={provider}）")
    return {"dry_run": False, "revived": len(accounts), "accounts": preview[:200]}


def delete_account(db: Session, account_id: int | None = None, email: str | None = None) -> bool:
    """永久删除账号（用于风控账号清理）。"""
    stmt = select(EmailAccount)
    if account_id:
        stmt = stmt.where(EmailAccount.id == account_id)
    elif email:
        stmt = stmt.where(EmailAccount.email == email.lower())
    else:
        logger.info("[delete-account] 参数为空，跳过")
        return False
    
    acc = db.scalar(stmt)
    if acc:
        db.delete(acc)
        db.commit()
        logger.info(f"[delete-account] 已删除 email={acc.email} reason=service_abuse")
        return True
    logger.info(f"[delete-account] ✗ 账号不存在 account_id={account_id} email={email}")
    return False


def mark_dead(db: Session, account_id: int | None = None, email: str | None = None) -> bool:
    stmt = select(EmailAccount)
    if account_id:
        stmt = stmt.where(EmailAccount.id == account_id)
    elif email:
        stmt = stmt.where(EmailAccount.email == email.lower())
    else:
        logger.info("[mark-dead] 参数为空，跳过")
        return False
    acc = db.scalar(stmt)
    if acc:
        prev_status = acc.health_status
        acc.health_status = "dead"
        acc.health_reason = "provider_fatal"
        acc.pool_status = "retired"
        acc.claimed_at = None
        acc.last_check_at = _now()
        _sync_legacy_status(acc)
        db.commit()
        logger.info(f"[mark-dead] 标记失效 email={acc.email} {prev_status} → dead")
        return True
    logger.info(f"[mark-dead] ✗ 账号不存在 account_id={account_id} email={email}")
    return False


def get_account_credentials(db: Session, email: str) -> dict[str, Any] | None:
    acc = db.scalar(select(EmailAccount).where(EmailAccount.email == email.lower()))
    if not acc:
        return None
    import json

    try:
        cred = json.loads(acc.account_json or "{}")
    except (json.JSONDecodeError, TypeError):
        cred = {}
    if acc.proxy:
        cred["proxy"] = acc.proxy
    return {"email": acc.email, "account_id": acc.id, "provider": acc.provider, **cred}


def update_refresh_token(email: str, new_refresh_token: str) -> None:
    """把 provider 刷新时轮换出的新 refresh_token 落库（独立短事务）。

    收码线程与调度任务共享库里的 rt；微软 IMAP scope 刷新会轮换 rt，
    不落库的话其他组件拿旧 rt 必然 invalid_grant。
    """
    import json as _json

    from ..database import SessionLocal

    with SessionLocal() as s:
        acc = s.scalar(
            select(EmailAccount).where(EmailAccount.email == (email or "").lower())
        )
        if acc is None:
            return
        try:
            cred = _json.loads(acc.account_json or "{}")
        except (json.JSONDecodeError, TypeError):
            cred = {}
        if cred.get("refresh_token") != new_refresh_token:
            cred["refresh_token"] = new_refresh_token
            acc.account_json = _json.dumps(cred, ensure_ascii=False)
            s.commit()


def snapshot_mail_baseline(db: Session, email: str, project_code: str = "") -> dict[str, Any]:
    """取号瞬间抓一次收件箱快照，作为"旧信基线"。

    号池里的号都是回收号，收件箱往往留着几天前的旧信。这里把当前所有
    message_id 记下来，receive 时凡是命中基线的邮件一律视为旧信丢弃，
    同时记录 baseline_at，双重判据挡掉懒同步带进来的历史邮件。

    抓取失败不阻断取号（记 captured=0，receive 退化为只按时间窗口过滤）。
    """
    key = (email or "").strip().lower()
    started = _now()
    ids: list[str] = []
    latest: datetime | None = None
    ok = False
    try:
        cred = get_account_credentials(db, key)
        if cred and cred.get("refresh_token") and cred.get("client_id"):
            from ..providers.outlook import GRAPH_FOLDERS, _graph_list_messages, _request_access_token

            # 先结束当前事务把连接还池：下面是一次网络调用（Graph 拉全文件夹），
            # 拿着连接做网络 I/O 会在并发取号时把 pool_size 耗干（见 claim_account）。
            db.commit()
            data = _request_access_token(
                cred["refresh_token"], cred["client_id"], "https://graph.microsoft.com/.default",
                proxy=cred.get("proxy") or None,
            )
            token = data["access_token"]
            for folder in GRAPH_FOLDERS:
                try:
                    messages = _graph_list_messages(token, folder)
                except Exception:
                    continue
                for msg in messages:
                    mid = str(msg.get("id") or "")
                    if mid:
                        ids.append(mid)
                    recv = normalize_utc(msg.get("receivedDateTime"))
                    if recv and (latest is None or recv > latest):
                        latest = recv
            ok = True
    except Exception as exc:  # noqa: BLE001
        logger.info(f"[baseline] 抓取失败（降级为时间窗口判据）email={key}: {type(exc).__name__}: {exc}")

    row = save_mail_baseline(db, key, project_code, ids, baseline_at=started, latest_received_at=latest)
    if not ok:
        row.captured = 0
        db.commit()
    logger.info(
        f"[baseline] email={key} ids={len(ids)} latest={latest} captured={ok} "
        f"at={started.isoformat()}"
    )
    return {
        "email": key,
        "message_ids": set(ids),
        "baseline_at": started,
        "latest_received_at": latest,
        "captured": ok,
    }


def save_mail_baseline(
    db: Session,
    email: str,
    project_code: str,
    message_ids: list[str] | set[str] | None,
    baseline_at: datetime | None = None,
    latest_received_at: datetime | str | None = None,
) -> MailBaseline:
    """记录「取号瞬间」的收件箱基线，供 receive 时挡掉旧信。

    号池里的号都是回收号，收件箱往往留着几天前的旧信。没有基线就只能靠
    时间窗口，而时间窗口依赖服务器时钟与微软一致；双判据更稳。
    """
    key = (email or "").strip().lower()
    ids = [str(x) for x in (message_ids or []) if x]
    at = normalize_utc(baseline_at) or _now()
    latest = normalize_utc(latest_received_at)
    row = db.scalar(select(MailBaseline).where(MailBaseline.email == key))
    if row is None:
        row = MailBaseline(email=key)
        db.add(row)
    row.project_code = project_code
    row.baseline_at = _naive_utc(at)
    row.latest_received_at = _naive_utc(latest) if latest else None
    row.message_ids = ",".join(ids)[:200000]
    row.captured = 1
    db.commit()
    logger.info(f"[baseline] 已记录基线 email={key} ids={len(ids)} at={row.baseline_at}")
    return row


def get_mail_baseline(db: Session, email: str) -> dict[str, Any] | None:
    """读取邮箱基线；没记录过返回 None（receive 会退化到只用时间窗口）。"""
    key = (email or "").strip().lower()
    row = db.scalar(select(MailBaseline).where(MailBaseline.email == key))
    if row is None:
        return None
    ids = {x for x in (row.message_ids or "").split(",") if x}
    baseline_at = row.baseline_at.replace(tzinfo=timezone.utc) if row.baseline_at else None
    latest = row.latest_received_at.replace(tzinfo=timezone.utc) if row.latest_received_at else None
    return {
        "email": key,
        "project_code": row.project_code,
        "message_ids": ids,
        "baseline_at": baseline_at,
        "latest_received_at": latest,
        "captured": bool(row.captured),
    }


def _purge_dead_risk_control(db: Session, provider: str) -> list[dict[str, Any]]:
    """报告已标记 dead + risk_control 的账号（**不再物理删除**）。

    历史实现会在每次健康扫描时把它们物理删除；由于 429/403 曾被误判为风控，
    这会导致一次限流批量丢号。现在默认只统计、不下手，由人工在界面确认后再删。
    """
    rows = list(db.scalars(
        select(EmailAccount).where(
            EmailAccount.provider == provider,
            EmailAccount.health_status == "dead",
            EmailAccount.health_reason == "risk_control",
        )
    ))
    purged: list[dict[str, Any]] = []
    for acc in rows:
        purged.append({
            "email": acc.email, "status": "dead",
            "detail": acc.check_result or "risk_control",
            "checked_at": acc.last_check_at.isoformat() if acc.last_check_at else None,
            "fail_count": acc.fail_count,
            "action": "marked_only_需人工确认删除",
        })
    if purged:
        logger.warning(
            f"[health-scan] 有 {len(purged)} 个 dead+risk_control 账号待人工处理"
            f"（已改为只标记不删除）"
        )
    return purged


def check_accounts_health(
    db: Session,
    provider: str,
    limit: int = 50,
    force: bool = False,
) -> dict[str, Any]:
    """对指定 provider 的账号做健康扫描（低频串行 + 风控自动暂停）。

    返回 {total, checked, healthy, unhealthy, skipped, paused, details: [...]}
    """
    logger.info(f"[health-scan] 开始扫描 provider={provider} limit={limit} force={force}")
    # 取所有 available / healthy / unhealthy 状态的账号（不含 using/dead）。
    # 排序：最久没检测的优先（last_check_at 升序，NULL 最先），
    # 保证每个号都能被轮到，而不是永远只扫 id 最小的那批。
    stmt = (
        select(EmailAccount)
        .where(
            EmailAccount.provider == provider,
            EmailAccount.pool_status == "available",
            EmailAccount.health_status.in_(["unknown", "healthy", "unhealthy"]),
        )
        .order_by(
            EmailAccount.last_check_at.asc().nulls_first(),
            EmailAccount.id.asc(),
        )
        # 候选窗口放大：7 天缓存会跳过大量刚检过的号，
        # 太小会导致 limit 填不满（970 个号永远只扫前 100 个）。
        .limit(limit * 4 if not force else limit)
    )
    accounts = list(db.scalars(stmt))

    # 7 天缓存：非 force 模式下跳过最近检测过的
    cache_cutoff = _naive_utc(_now()) - timedelta(days=7)
    to_check = []
    skipped = 0
    for acc in accounts:
        last_check = _naive_utc(acc.last_check_at)
        if not force and last_check and last_check > cache_cutoff:
            skipped += 1
            continue
        to_check.append(acc)
        if len(to_check) >= limit:
            break

    logger.info(
        f"[health-scan] 候选 {len(accounts)} 个，待检测 {len(to_check)} 个，7天缓存跳过 {skipped} 个"
    )

    checked = 0
    healthy_count = 0
    unhealthy_count = 0
    consecutive_risk = 0
    paused = False
    details: list[dict[str, Any]] = []

    # 兜底：清理历史已标记风控（service abuse）的 dead 账号
    purged = _purge_dead_risk_control(db, provider)
    deleted_count = len(purged)
    details.extend(purged)

    for acc in to_check:
        checked += 1
        logger.info(f"[health-scan] ({checked}/{len(to_check)}) 检测 {acc.email}（pool={acc.pool_status} health={acc.health_status} fail_count={acc.fail_count}）")
        try:
            cred = json.loads(acc.account_json or "{}")
            if acc.proxy:
                cred["proxy"] = acc.proxy
            cred["email"] = acc.email  # provider 内 rt 轮换回写需要
            old_rt = cred.get("refresh_token")
            if provider == "outlook":
                from ..providers.outlook import verify_account
                status, detail = verify_account(cred)
                ok = (status == "healthy")
            else:
                ok, detail = True, "无连通性检查（临时源）"
            if cred.get("refresh_token") != old_rt:
                # verify 期间微软轮换了 rt（IMAP scope 降级分支），落库
                acc.account_json = json.dumps(cred, ensure_ascii=False)
                logger.info(f"[health-scan] {acc.email} refresh_token 已轮换落库")

            final_status = _apply_check_result(acc, ok, detail, db=db, auto_delete_risk=False)
            
            # 风控账号：已物理删除
            if final_status == "deleted":
                logger.warning(f"[health-scan] {acc.email} 检测到风控，已自动删除")
                details.append({
                    "email": acc.email, "status": "deleted",
                    "detail": detail, "checked_at": acc.last_check_at.isoformat() if acc.last_check_at else None,
                    "fail_count": acc.fail_count, "action": "deleted_risk_control",
                })
                continue
            
            db.commit()
            logger.info(f"[health-scan] {acc.email} 结果：ok={ok} final_status={final_status} detail={detail[:100]}")

            if final_status == "healthy":
                healthy_count += 1
                consecutive_risk = 0
            else:
                unhealthy_count += 1
                # 检测到「真风控」关键词 → 连续计数（429/403 是限流，不应触发暂停）
                if is_risk_control(detail):
                    consecutive_risk += 1
                    logger.info(f"[health-scan] {acc.email} 检测到风控关键词，连续风控计数={consecutive_risk}")
                else:
                    consecutive_risk = 0

            details.append({
                "email": acc.email, "status": final_status,
                "detail": detail, "checked_at": acc.last_check_at.isoformat(),
                "fail_count": acc.fail_count,
            })

        except Exception as exc:
            logger.exception(f"[health-scan] {acc.email} 检测抛异常")
            final_status = _apply_check_result(acc, False, str(exc), db=db, auto_delete_risk=False)
            
            # 风控账号：已物理删除
            if final_status == "deleted":
                logger.warning(f"[health-scan] {acc.email} 检测到风控，已自动删除")
                details.append({
                    "email": acc.email, "status": "deleted",
                    "detail": str(exc)[:200], "checked_at": acc.last_check_at.isoformat() if acc.last_check_at else None,
                    "fail_count": acc.fail_count, "action": "deleted_risk_control",
                })
                continue
            
            db.commit()
            unhealthy_count += 1
            details.append({
                "email": acc.email, "status": final_status,
                "detail": str(exc)[:200], "checked_at": acc.last_check_at.isoformat(),
                "fail_count": acc.fail_count,
            })

        # 连续 3 个风控 → 自动暂停
        if consecutive_risk >= 3:
            paused = True
            logger.warning(f"[health-scan] 连续 {consecutive_risk} 个风控响应，自动暂停扫描（已检测 {checked}/{len(to_check)}）")
            break

        # 串行间隔 2-3 秒（最后一个不等待）
        if checked < len(to_check):
            time.sleep(random.uniform(2, 3))

    return {
        "total": len(accounts),
        "checked": checked,
        "healthy": healthy_count,
        "unhealthy": unhealthy_count,
        "deleted": deleted_count,
        "skipped": skipped,
        "paused": paused,
        "need_review": deleted_count,   # dead+risk_control 待人工确认（不再自动删除）
        "details": details,
    }


def health_summary(db: Session, provider: str | None = None) -> dict[str, Any]:
    """邮箱池健康概览（只读统计，不触发检测）。"""
    stmt = select(
        EmailAccount.provider,
        EmailAccount.pool_status,
        EmailAccount.health_status,
        EmailAccount.last_check_at,
    )
    if provider:
        stmt = stmt.where(EmailAccount.provider == provider)
    rows = db.execute(stmt).all()

    out = {"total": 0, "healthy": 0, "available": 0, "unhealthy": 0, "unknown": 0, "using": 0, "retired": 0, "dead": 0}
    last_scan = None
    for _prov, pool_status, health_status, last_check in rows:
        out["total"] += 1
        if pool_status in ("available", "using", "retired"):
            out[pool_status] += 1
        if health_status in ("healthy", "unhealthy", "dead", "unknown"):
            out[health_status] += 1
        if last_check and (last_scan is None or last_check > last_scan):
            last_scan = last_check

    out["last_scan_at"] = last_scan.isoformat() if last_scan else ""
    return out


def stats_by_client_id(db: Session, provider: str = "outlook") -> list[dict[str, Any]]:
    """按 client_id 维度统计号池健康分布。

    用于识别被微软封禁的 client_id（同一 client_id 的号集体失效）。
    返回 [{client_id, total, healthy, unhealthy, dead, available, using, healthy_rate}]
    """
    rows = db.execute(
        select(EmailAccount.account_json, EmailAccount.pool_status, EmailAccount.health_status)
        .where(EmailAccount.provider == provider)
    ).all()

    bucket: dict[str, dict[str, int]] = {}
    for account_json, pool_status, health_status in rows:
        try:
            cred = json.loads(account_json or "{}")
        except (json.JSONDecodeError, TypeError):
            cred = {}
        cid = str(cred.get("client_id") or "").strip()
        # 脱敏：只展示前 8 位 + 后 4 位
        if len(cid) > 16:
            cid_display = cid[:8] + "..." + cid[-4:]
        elif cid:
            cid_display = cid
        else:
            cid_display = "(空)"
        b = bucket.setdefault(cid_display, {
            "client_id": cid_display, "total": 0,
            "healthy": 0, "unhealthy": 0, "dead": 0, "unknown": 0,
            "available": 0, "using": 0,
        })
        b["total"] += 1
        if pool_status in ("available", "using"):
            b[pool_status] += 1
        if health_status in ("healthy", "unhealthy", "dead", "unknown"):
            b[health_status] += 1

    out = []
    for b in bucket.values():
        b["healthy_rate"] = round(b["healthy"] / b["total"] * 100, 1) if b["total"] else 0.0
        out.append(b)
    # 按 total 倒序
    out.sort(key=lambda x: x["total"], reverse=True)
    return out


# ────────────────────────────────────────────────────────────────
#  心跳保活
# ────────────────────────────────────────────────────────────────


def heartbeat(
    db: Session,
    mode: str = "silent",
    limit: int = 50,
) -> dict[str, Any]:
    """对池内 healthy 账号执行心跳保活。

    mode = "silent"：只读拉取收件箱
    mode = "interactive"：额外标记未读邮件为已读

    返回 {total, success, failed, details: [...]}
    """
    logger.info(f"[heartbeat] 开始保活 mode={mode} limit={limit}")
    stmt = (
        select(EmailAccount)
        .where(
            EmailAccount.provider == "outlook",
            EmailAccount.pool_status == "available",
            EmailAccount.health_status == "healthy",
        )
        .order_by(EmailAccount.id.asc())
        .limit(limit)
    )
    accounts = list(db.scalars(stmt))
    logger.info(f"[heartbeat] 待保活 {len(accounts)} 个 healthy 账号")

    success = 0
    failed = 0
    details: list[dict[str, Any]] = []
    interactive = mode == "interactive"

    for idx, acc in enumerate(accounts, 1):
        logger.info(f"[heartbeat] ({idx}/{len(accounts)}) 保活 {acc.email}（mode={mode}）")
        try:
            cred = json.loads(acc.account_json or "{}")
            if acc.proxy:
                cred["proxy"] = acc.proxy
            cred["email"] = acc.email  # provider 内 rt 轮换回写需要
            from ..providers.outlook import silent_keepalive
            ok, msg = silent_keepalive(cred, interactive=interactive)

            # 更新 refresh_token（微软可能轮换）
            acc.account_json = json.dumps(cred, ensure_ascii=False)

            # 用统一函数处理 fail_count + 自动降级 + 风控自动删除
            final_status = _apply_check_result(acc, ok, msg, db=db, auto_delete_risk=False)
            
            # 风控账号：已物理删除
            if final_status == "deleted":
                logger.warning(f"[heartbeat] {acc.email} 检测到风控，已自动删除")
                details.append({
                    "email": acc.email, "result": "deleted",
                    "detail": msg, "status": "deleted", "fail_count": acc.fail_count,
                    "action": "deleted_risk_control",
                })
                continue
            
            db.commit()
            logger.info(f"[heartbeat] {acc.email} 结果：ok={ok} final_status={final_status} detail={msg[:100]}")

            if ok:
                success += 1
            else:
                failed += 1
            details.append({
                "email": acc.email, "result": "success" if ok else "failed",
                "detail": msg, "status": final_status, "fail_count": acc.fail_count,
            })

        except Exception as exc:
            logger.exception(f"[heartbeat] {acc.email} 保活抛异常")
            final_status = _apply_check_result(acc, False, str(exc), db=db, auto_delete_risk=False)
            
            # 风控账号：已物理删除
            if final_status == "deleted":
                details.append({
                    "email": acc.email, "result": "deleted",
                    "detail": str(exc)[:200], "status": "deleted", "fail_count": acc.fail_count,
                    "action": "deleted_risk_control",
                })
                continue
            
            db.commit()
            failed += 1
            details.append({
                "email": acc.email, "result": "failed",
                "detail": str(exc)[:200], "status": final_status, "fail_count": acc.fail_count,
            })

        # 串行间隔 5-15 秒（随机抖动，防规律性请求）
        if idx < len(accounts):
            time.sleep(random.uniform(5, 15))

    logger.info(f"[heartbeat] 保活完成 total={len(accounts)} success={success} failed={failed}")
    return {"total": len(accounts), "success": success, "failed": failed, "details": details}


# ── Device Code Flow 刷新 Token ──

def start_device_code_flow(db: Session, account_id: int) -> dict[str, Any]:
    """为指定账号启动 Device Code Flow，返回 {verification_uri, user_code, expires_in}。"""
    acc = db.get(EmailAccount, account_id)
    if not acc:
        raise ValueError("账号不存在")
    if acc.provider != "outlook":
        raise ValueError("仅支持 Outlook 账号")
    cred = json.loads(acc.account_json or "{}")
    if not cred.get("password"):
        raise ValueError("账号缺少密码信息")

    body = urllib.parse.urlencode({
        "client_id": THUNDERBIRD_CLIENT_ID,
        "scope": REFRESH_SCOPE,
    }).encode("utf-8")
    req = urllib.request.Request(DEVICE_CODE_URL, data=body, method="POST")
    req.add_header("Content-Type", "application/x-www-form-urlencoded")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"Device Code 请求失败: HTTP {exc.code} {raw[:300]}")

    if "device_code" not in data:
        raise RuntimeError(f"Device Code 响应异常: {data.get('error_description', str(data))[:300]}")

    with _device_flows_lock:
        _device_flows[account_id] = {
            "device_code": data["device_code"],
            "user_code": data["user_code"],
            "verification_uri": data.get("verification_uri", "https://microsoft.com/devicelogin"),
            "expires_at": time.time() + data.get("expires_in", 900),
            "interval": data.get("interval", 5),
            "email": acc.email,
        }

    logger.info(f"[deviceflow] 启动 Device Code Flow account_id={account_id} email={acc.email}")
    return {
        "verification_uri": _device_flows[account_id]["verification_uri"],
        "user_code": _device_flows[account_id]["user_code"],
        "expires_in": data.get("expires_in", 900),
        "email": acc.email,
    }


def poll_device_code_flow(db: Session, account_id: int) -> dict[str, Any]:
    """轮询 Device Code Flow 结果。返回 {status, message, new_refresh_token?}。"""
    with _device_flows_lock:
        flow = _device_flows.get(account_id)
    if not flow:
        return {"status": "not_found", "message": "未找到进行中的授权流程"}

    if time.time() > flow["expires_at"]:
        with _device_flows_lock:
            _device_flows.pop(account_id, None)
        return {"status": "expired", "message": "授权已超时，请重新发起"}

    # 轮询微软 token 端点（不阻塞，单次尝试）
    body = urllib.parse.urlencode({
        "client_id": THUNDERBIRD_CLIENT_ID,
        "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        "device_code": flow["device_code"],
    }).encode("utf-8")
    req = urllib.request.Request(TOKEN_URL, data=body, method="POST")
    req.add_header("Content-Type", "application/x-www-form-urlencoded")
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        try:
            data = json.loads(raw)
        except Exception:
            return {"status": "error", "message": f"HTTP {exc.code}: {raw[:200]}"}

    if "access_token" in data:
        new_rt = data.get("refresh_token", "")
        if not new_rt:
            return {"status": "error", "message": "授权成功但未返回 refresh_token"}

        # 更新账号
        acc = db.get(EmailAccount, account_id)
        if acc:
            cred = json.loads(acc.account_json or "{}")
            cred["refresh_token"] = new_rt
            cred["client_id"] = THUNDERBIRD_CLIENT_ID
            acc.account_json = json.dumps(cred, ensure_ascii=False)
            acc.health_status = "unknown"
            acc.fail_count = 0
            acc.check_result = ""
            acc.health_reason = ""
            # 重新授权 = 凭证已修复：把因凭证失效而退役的账号放回池子，
            # 否则授权成功了账号却仍是 retired，永远取不到（历史 bug）。
            revived = False
            if acc.pool_status == "retired":
                # 按项目一次性：使用记录只挡对应项目，号本身可以回池
                acc.pool_status = "available"
                acc.claimed_at = None
                revived = True
            _sync_legacy_status(acc)
            db.commit()
            logger.info(
                f"[deviceflow] Token 刷新成功 account_id={account_id} email={acc.email}"
                f"{'（已重新入池）' if revived else ''}"
            )

        with _device_flows_lock:
            _device_flows.pop(account_id, None)
        return {"status": "success", "message": "Token 刷新成功"}

    err = data.get("error", "")
    if err == "authorization_pending":
        return {"status": "pending", "message": "等待用户授权…"}
    if err in ("authorization_declined", "expired_token"):
        with _device_flows_lock:
            _device_flows.pop(account_id, None)
        return {"status": "declined", "message": f"授权被拒绝或已过期: {err}"}
    return {"status": "error", "message": f"轮询异常: {err} {data.get('error_description', '')[:200]}"}


# ── 全量刷新 Token（一键刷新全部可刷新账号，串行 + 限流保护）──────────────
_batch_tasks: dict[str, dict[str, Any]] = {}
_batch_tasks_lock = threading.Lock()
BATCH_REFRESH_INTERVAL = 3.0  # 每个账号间隔秒数，避免触发微软限流


def _mark_batch_result(task_id: str, email: str, kind: str, message: str) -> None:
    with _batch_tasks_lock:
        task = _batch_tasks.get(task_id)
        if not task:
            return
        task[kind] += 1
        task["done"] += 1
        task["results"].append({"email": email, "status": kind, "message": message})


def start_batch_refresh_token(db: Session, provider: str = "outlook", trigger: str = "manual") -> dict[str, Any]:
    """启动全量刷新任务：后台线程逐个刷新所有可刷新账号的 refresh_token。

    与原版 outlookEmail「全量刷新」一致：串行执行、按账号数量持续较长时间，
    执行期间禁止重复发起（running 状态直接拒绝）。
    trigger: manual / schedule，用于刷新历史记录区分触发方式。
    """
    with _batch_tasks_lock:
        for t in _batch_tasks.values():
            if t.get("status") == "running":
                return {
                    "task_id": t["task_id"],
                    "running": True,
                    "message": "已有刷新任务运行中，请勿重复发起刷新",
                }
        # 先统计可刷新账号数（有 refresh_token 且非 dead）
        candidates = db.scalars(
            select(EmailAccount).where(
                EmailAccount.provider == provider,
                EmailAccount.health_status != "dead",
                EmailAccount.account_json.like("%refresh_token%"),
            )
        ).all()
        total = 0
        for acc in candidates:
            try:
                cred = json.loads(acc.account_json or "{}")
                if cred.get("refresh_token"):
                    total += 1
            except Exception:
                continue

        task_id = f"bt{int(time.time())}"
        _batch_tasks[task_id] = {
            "task_id": task_id,
            "status": "running",
            "total": total,
            "done": 0,
            "success": 0,
            "failed": 0,
            "skipped": 0,
            "current_email": "",
            "results": [],
            "trigger": trigger,
            "provider": provider,
            "started_at": _now().isoformat(),
            "finished_at": "",
        }

    threading.Thread(target=_run_batch_refresh, args=(task_id, provider), daemon=True).start()
    return {"task_id": task_id, "running": True, "total": total,
            "message": f"全量刷新已启动，共 {total} 个账号"}


def _run_batch_refresh(task_id: str, provider: str) -> None:
    """后台执行批量刷新：串行刷新，每次间隔 BATCH_REFRESH_INTERVAL 秒防限流。"""
    from ..database import SessionLocal  # 独立会话，避免与请求会话冲突
    from ..providers.outlook import GRAPH_SCOPE, _request_access_token

    db = SessionLocal()
    try:
        stmt = (
            select(EmailAccount)
            .where(
                EmailAccount.provider == provider,
                EmailAccount.health_status != "dead",
            )
            .order_by(EmailAccount.id.asc())
        )
        accounts = list(db.scalars(stmt))

        for idx, acc in enumerate(accounts, 1):
            with _batch_tasks_lock:
                _batch_tasks[task_id]["current_email"] = acc.email

            try:
                cred = json.loads(acc.account_json or "{}")
                rt = cred.get("refresh_token") or ""
                cid = cred.get("client_id") or ""
                if not rt or not cid:
                    _mark_batch_result(task_id, acc.email, "skipped", "缺少 refresh_token，跳过")
                    continue

                # 互斥刷新（防止与收码/健康扫描并发轮换同一 rt）；
                # 轮换的新 rt 由 _refresh_token_for 写 cred + hook 落库
                from ..providers.outlook import _refresh_token_for
                cred["email"] = acc.email
                _refresh_token_for(cred, GRAPH_SCOPE)
                if cred.get("refresh_token") != rt:
                    acc.account_json = json.dumps(cred, ensure_ascii=False)
                    logger.info(f"[batch-refresh] {acc.email} refresh_token 已轮换更新")

                acc.health_status = "healthy"
                acc.health_reason = ""
                acc.fail_count = 0
                acc.check_result = "全量刷新成功"
                acc.last_check_at = _now()
                db.commit()
                _mark_batch_result(task_id, acc.email, "success", "刷新成功")
                logger.info(f"[batch-refresh] ({idx}/{len(accounts)}) {acc.email} ✓")
            except Exception as exc:
                db.rollback()
                reason = _classify_health_error(str(exc))
                if reason == "risk_control":
                    # 风控（service abuse）：物理删除
                    logger.warning(f"[batch-refresh] {acc.email} 检测到风控，自动删除")
                    delete_account(db, account_id=acc.id)
                    _mark_batch_result(task_id, acc.email, "failed", f"[deleted-risk] {str(exc)[:180]}")
                    continue
                acc.health_status = "unhealthy"
                acc.health_reason = reason
                acc.fail_count = (acc.fail_count or 0) + 1
                acc.check_result = str(exc)[:200]
                acc.last_check_at = _now()
                db.commit()
                logger.info(f"[batch-refresh] ({idx}/{len(accounts)}) {acc.email} ✗ {str(exc)[:120]}")
                _mark_batch_result(task_id, acc.email, "failed", str(exc)[:200])

            # 串行限流保护
            if idx < len(accounts):
                time.sleep(BATCH_REFRESH_INTERVAL)

    except Exception as exc:
        logger.exception("[batch-refresh] 任务异常")
        with _batch_tasks_lock:
            _batch_tasks[task_id]["error"] = str(exc)
    finally:
        with _batch_tasks_lock:
            task = _batch_tasks.get(task_id)
            if task:
                task["status"] = "finished"
                task["finished_at"] = _now().isoformat()
                task["current_email"] = ""
        _save_refresh_log(task_id)
        db.close()


def _save_refresh_log(task_id: str) -> None:
    """将批量刷新任务结果写入历史表（TokenRefreshLog）。"""
    from ..database import SessionLocal

    with _batch_tasks_lock:
        task = _batch_tasks.get(task_id)
        if not task or task.get("logged"):
            return
        snapshot = dict(task)
        snapshot["logged"] = True

    try:
        db = SessionLocal()
        try:
            started = datetime.fromisoformat(snapshot.get("started_at") or "")
            finished = datetime.fromisoformat(snapshot.get("finished_at") or "")
            duration = (finished - started).total_seconds() if finished > started else 0.0
        except Exception:
            duration = 0.0
        # 结果明细：只保留异常项 + 少量成功样本，保证 JSON 始终完整可解析。
        # 旧实现是 json.dumps(...)[:4000] 硬截断，1000+ 账号时会切在半截，
        # 存进去的就是非法 JSON（实测 detail 长度恰好 4000、json.loads 报错）。
        results = snapshot.get("results", []) or []
        abnormal = [r for r in results if r.get("status") != "success"]
        success_sample = [r for r in results if r.get("status") == "success"][:20]
        payload = {
            "summary": {
                "total": snapshot.get("total", 0),
                "success": snapshot.get("success", 0),
                "failed": snapshot.get("failed", 0),
                "skipped": snapshot.get("skipped", 0),
                "truncated": len(results) > len(abnormal) + len(success_sample),
            },
            "results": abnormal + success_sample,
        }
        detail = json.dumps(payload, ensure_ascii=False)
        # 兜底：极端情况下（几万条失败）仍超长时再收缩，但始终保持合法 JSON
        if len(detail) > 200000:
            payload["results"] = abnormal[:200]
            payload["summary"]["truncated"] = True
            detail = json.dumps(payload, ensure_ascii=False)
        log = TokenRefreshLog(
            trigger=snapshot.get("trigger", "manual"),
            provider=snapshot.get("provider", "outlook"),
            status="finished" if snapshot.get("status") == "finished" else "failed",
            total=snapshot.get("total", 0),
            success=snapshot.get("success", 0),
            failed=snapshot.get("failed", 0),
            skipped=snapshot.get("skipped", 0),
            duration=round(duration, 1),
            detail=detail,
            created_at=_now(),
        )
        try:
            from ..services import settings_service
            settings_service.set_refresh_last_run(db, {
                "total": snapshot.get("total", 0),
                "success": snapshot.get("success", 0),
                "failed": snapshot.get("failed", 0),
                "skipped": snapshot.get("skipped", 0),
                "duration": round(duration, 1),
                "trigger": snapshot.get("trigger", "manual"),
            })
        except Exception:
            pass
        db.add(log)
        db.commit()
        db.close()
    except Exception as exc:
        logger.error("[batch-refresh] 写入刷新历史失败: %s", exc)


def list_refresh_logs(db: Session, limit: int = 50, offset: int = 0) -> list[TokenRefreshLog]:
    """查询 Token 刷新历史（倒序）。"""
    return list(db.scalars(
        select(TokenRefreshLog).order_by(TokenRefreshLog.id.desc()).limit(limit).offset(offset)
    ))


def get_batch_refresh_status(task_id: str) -> dict[str, Any]:
    """查询批量刷新任务进度。"""
    with _batch_tasks_lock:
        task = _batch_tasks.get(task_id)
        return dict(task) if task else {"status": "not_found", "message": "任务不存在"}


# ────────────────────────────────────────────────────────────────
#  账号标签 / 备注 / 代理 更新与导出
# ────────────────────────────────────────────────────────────────


def update_account_meta(
    db: Session,
    account_id: int,
    tags: str | None = None,
    remark: str | None = None,
    proxy: str | None = None,
) -> EmailAccount | None:
    """更新账号的标签 / 备注 / 代理（任一非 None 生效）。"""
    acc = db.get(EmailAccount, account_id)
    if not acc:
        return None
    if tags is not None:
        acc.tags = tags.strip()[:255]
    if proxy is not None:
        acc.proxy = proxy.strip()[:255]
    if remark is not None:
        acc.remark = remark.strip()[:255]
    db.commit()
    return acc


def list_tags(db: Session, provider: str | None = None) -> list[str]:
    """聚合所有账号已有标签（去重，按使用频率排序）。"""
    stmt = select(EmailAccount.tags)
    if provider:
        stmt = stmt.where(EmailAccount.provider == provider)
    counter: dict[str, int] = {}
    for (tags,) in db.execute(stmt):
        for tag in (tags or "").split(","):
            tag = tag.strip()
            if tag:
                counter[tag] = counter.get(tag, 0) + 1
    return sorted(counter, key=lambda t: (-counter[t], t))


def export_accounts(
    db: Session,
    provider: str | None = None,
    health_status: str | None = None,
    tag: str | None = None,
    keyword: str | None = None,
    limit: int = 2000,
) -> list[dict[str, Any]]:
    """导出账号（含凭证），可过滤 provider / 健康状态 / 标签 / 关键词。

    返回每行：{email, password, client_id, refresh_token, health_status, tags, remark, proxy}
    """
    stmt = select(EmailAccount).order_by(EmailAccount.id.asc()).limit(limit)
    if provider:
        stmt = stmt.where(EmailAccount.provider == provider)
    if health_status:
        stmt = stmt.where(EmailAccount.health_status == health_status)
    if keyword:
        kw = keyword.strip()
        stmt = stmt.where(EmailAccount.email.like(f"%{kw}%"))
    accounts = list(db.scalars(stmt))
    if tag:
        tag = tag.strip()
        accounts = [a for a in accounts if tag in [t.strip() for t in (a.tags or "").split(",")]]
    rows: list[dict[str, Any]] = []
    for acc in accounts:
        try:
            cred = json.loads(acc.account_json or "{}")
        except Exception:
            cred = {}
        rows.append({
            "email": acc.email,
            "password": cred.get("password", ""),
            "client_id": cred.get("client_id", ""),
            "refresh_token": cred.get("refresh_token", ""),
            "health_status": acc.health_status or "",
            "health_reason": acc.health_reason or "",
            "tags": acc.tags or "",
            "remark": acc.remark or "",
            "proxy": acc.proxy or "",
        })
    return rows
