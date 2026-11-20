"""邮件业务服务：按 project_code 取号 → 等待验证码 → 记录邮件。

这是 MailPool 的核心编排层，语义对齐 MailNest：
    buy(receive) → 用 project_code 路由到 provider，产出一个邮箱
    receive       → 轮询等待验证码（可选：等待期间抓取邮件内容）
    release       → 释放临时邮箱 / 归还池号
"""
from __future__ import annotations

import asyncio
import json
import logging
import time
from datetime import datetime, timezone
from typing import Any

from sqlalchemy import func, select
from sqlalchemy.orm import Session

from ..models import EmailAccount, EmailUsage, MailRecord, Project
from ..providers import ProviderError, create_provider, get_provider_class
from ..timeutil import normalize_utc, to_naive_utc, utc_iso
from . import pool_service
from . import settings_service

logger = logging.getLogger(__name__)


def _now() -> datetime:
    return datetime.now(timezone.utc)


# 判定 fatal 是否属于「该邮箱凭证已彻底失效」，只有这类才允许把号打成 dead。
# 其余 fatal（未找到凭据、配置错误、内部 bug）不应消耗库存。
_CREDENTIAL_FATAL_TOKENS = (
    "invalid_grant",
    "service abuse",
    "service_abuse",
    "aadsts70000",
    "aadsts50173",
    "account is locked",
    "account disabled",
    "unauthorized",
    "refresh token is invalid",
    "imap 登录失败",
)


def _is_credential_fatal(detail: str) -> bool:
    text = str(detail or "").lower()
    return any(token in text for token in _CREDENTIAL_FATAL_TOKENS)


def get_project(db: Session, code: str) -> Project | None:
    return db.scalar(select(Project).where(Project.code == code, Project.enabled == 1))


def list_projects(db: Session) -> list[Project]:
    return list(db.scalars(select(Project).order_by(Project.id.asc())))


def upsert_project(
    db: Session,
    code: str,
    name: str,
    provider: str,
    config_json: str = "{}",
    enabled: int = 1,
    project_id: int | None = None,
) -> Project:
    proj = db.get(Project, project_id) if project_id else db.scalar(
        select(Project).where(Project.code == code)
    )
    if proj:
        proj.code = code
        proj.name = name
        proj.provider = provider
        proj.config_json = config_json
        proj.enabled = enabled
    else:
        proj = Project(code=code, name=name, provider=provider, config_json=config_json, enabled=enabled)
        db.add(proj)
    db.commit()
    db.refresh(proj)
    return proj


def delete_project(db: Session, project_id: int) -> bool:
    proj = db.get(Project, project_id)
    if not proj:
        return False
    db.delete(proj)
    db.commit()
    return True


def build_provider(db: Session, project: Project):
    """按项目的 provider + 全局配置，构建 provider 实例。

    provider 实例 = 全局 provider 配置 + 项目 config 覆盖。
    """
    global_config = settings_service.get_provider_config(db, project.provider)
    merged = dict(global_config)
    try:
        proj_cfg = json.loads(project.config_json or "{}")
        if isinstance(proj_cfg, dict):
            merged.update(proj_cfg)
    except (json.JSONDecodeError, TypeError):
        pass
    prov = create_provider(project.provider, merged)
    return prov, merged


# ────────────────────────────────────────────────────────────────
#  取号
# ────────────────────────────────────────────────────────────────


def acquire_email(db: Session, project_code: str) -> dict[str, Any]:
    """按 project_code 取一个邮箱。

    固定源（outlook）：从号池 claim（跳过**本项目**已用过的邮箱），claim 后绑定。
    按项目一次性：A 项目用过的号，A 取不到，其他项目照常可取。
    """
    project = get_project(db, project_code)
    if not project:
        raise ProviderError(f"未知项目 code: {project_code}", fatal=False)

    prov, merged = build_provider(db, project)

    # 固定号池：claim 账号并注入凭证
    if prov.pooled:
        claimed = pool_service.claim_account(db, project.provider, project.code)
        if not claimed:
            raise ProviderError(
                f"邮箱池 [{project.provider}] 无可用账号（本项目用过的邮箱已排除，"
                "请先导入或检查状态）",
                fatal=False,
                kind="no_stock",
            )
        # claim 后再校验一次绑定（并发兜底），冲突则跳过该号
        ok, other = pool_service.bind_email(db, claimed["email"], project.code)
        if not ok:
            pool_service.release_account(db, account_id=claimed["account_id"])
            raise ProviderError(
                f"邮箱 {claimed['email']} 已被本项目使用过，已跳过该号",
                fatal=False,
                kind="no_stock",
            )
        # 取号瞬间抓收件箱基线：回收号收件箱里的旧信必须被挡掉，
        # 否则 receive 会立刻把 8~14 天前的旧验证码当成新码返回。
        try:
            pool_service.snapshot_mail_baseline(db, claimed["email"], project.code)
        except Exception as exc:  # noqa: BLE001
            logger.warning(f"[acquire] 抓基线失败（不阻断取号）email={claimed['email']}: {exc}")
        cred = pool_service.get_account_credentials(db, claimed["email"]) or {}
        if hasattr(prov, "set_credentials"):
            prov.set_credentials(claimed["email"], cred)
        email = claimed["email"]
        account_id = claimed["account_id"]
    else:
        # 临时源：生成新地址并绑定（随机地址几乎不冲突；冲突则重试）
        email = None
        account_id = None
        for _ in range(3):
            addr = prov.generate_address()
            ok, other = pool_service.bind_email(db, addr, project.code)
            if ok:
                email = addr
                break
        if email is None:
            raise ProviderError(
                "连续生成的邮箱地址均已被其他项目占用，请稍后重试",
                fatal=False,
                kind="no_stock",
            )

    return {
        "email": email,
        "provider": project.provider,
        "project_code": project.code,
        "project_name": project.name,
        "account_id": account_id,
        "status": "holding",
        # 统一带时区的 UTC ISO8601（见 app/timeutil.py）
        "acquired_at": _now().isoformat(),
    }


# ────────────────────────────────────────────────────────────────
#  取码
# ────────────────────────────────────────────────────────────────


def receive_code(
    db: Session,
    project_code: str,
    email: str,
    timeout: float | None = None,
    skip_codes: list[str] | None = None,
) -> dict[str, Any]:
    """轮询等待验证码。

    返回 {"email", "code", "status": "success"|"timeout", "message", "mails": [...]}
    """
    project = get_project(db, project_code)
    if not project:
        raise ProviderError(f"未知项目 code: {project_code}")
    prov, merged = build_provider(db, project)

    # 固定号池：把号池里的凭证注入 provider 实例。
    # receive 是独立请求，build_provider 会新建实例，不注入就会因
    # 「未找到邮箱凭据」(fatal) 被误判 dead —— 必须在这里补上。
    if getattr(prov, "pooled", False):
        cred = pool_service.get_account_credentials(db, email)
        if not cred or not cred.get("refresh_token"):
            raise ProviderError(f"邮箱池无该邮箱的可用凭证: {email}")
        if hasattr(prov, "set_credentials"):
            prov.set_credentials(email, cred)
        # 注入取号瞬间的收件箱基线：没有它就会把回收号里的旧验证码当成新码返回。
        # （号池账号收件箱普遍留着 8~14 天前的旧信，实测 1.9 秒就"成功"返回旧码）
        if hasattr(prov, "set_baseline"):
            baseline = pool_service.get_mail_baseline(db, email)
            if baseline is None:
                # 老数据没有基线记录：用 claim 时刻作为时间窗口起点（拿不到才用当前时刻）
                baseline = pool_service.snapshot_mail_baseline(db, email, project.code)
                acc = db.scalar(select(EmailAccount).where(EmailAccount.email == email.lower()))
                if acc is not None and acc.claimed_at is not None:
                    baseline["baseline_at"] = acc.claimed_at.replace(tzinfo=timezone.utc)
            prov.set_baseline(baseline.get("message_ids"), baseline.get("baseline_at"))

    tmo = timeout or float(merged.get("timeout") or 120)
    tmo = max(10.0, min(float(tmo), 600.0))

    start = time.time()
    try:
        mail = asyncio.run(
            prov.wait_mail(
                email,
                timeout=tmo,
                project={
                    "code": project.code,
                    "skip_codes": skip_codes or [],
                },
            )
        )
    except ProviderError as exc:
        # 仅「凭证确实失效」才置 dead；网络/未找到凭据等不应烧号
        if project.provider and exc.fatal and _is_credential_fatal(str(exc)):
            acc = db.scalar(select(EmailAccount).where(EmailAccount.email == email.lower()))
            if acc:
                pool_service.mark_dead(db, account_id=acc.id)
        raise

    if mail:
        code = str(mail.get("code") or "")
        save_mail_record(
            db,
            email=email,
            project_code=project.code,
            message_id=str(mail.get("message_id") or ""),
            provider=str(mail.get("provider") or project.provider),
            folder=str(mail.get("folder") or ""),
            sender=str(mail.get("sender") or ""),
            subject=str(mail.get("subject") or ""),
            body_preview=str(mail.get("body_preview") or ""),
            body=str(mail.get("body") or ""),
            code=code,
            code_source=str(mail.get("code_source") or ""),
            received_at=mail.get("received_at"),
        )
        return {"email": email, "code": code, "status": "success",
                "elapsed": round(time.time() - start, 2),
                "received_at": utc_iso(mail.get("received_at"))}
    # 空箱/没等到新信 —— 这是**正常轮询结果**，不是错误。
    # 调用方必须拿到 code=00000 + status=timeout，否则会在"对方还没发信"时中断注册。
    return {"email": email, "code": "", "status": "timeout",
            "elapsed": round(time.time() - start, 2),
            "message": f"等待 {tmo:.0f}s 未收到验证码"}


# ────────────────────────────────────────────────────────────────
#  释放
# ────────────────────────────────────────────────────────────────


def release_email(db: Session, project_code: str, email: str) -> dict[str, Any]:
    """释放邮箱。

    ⚠️ 语义（按项目一次性）：
    - **固定号池（pooled=True）**：解除占用、**回池**，返回
      `status="released", reusable_for_other_projects=True`。
      本项目的使用绑定保留 —— 该项目永远取不到这个号；
      其他项目照常可取。
    - 临时源（pooled=False）：真正释放上游地址，返回 `status="released"`。
    """
    project = get_project(db, project_code)
    if not project:
        raise ProviderError(f"未知项目 code: {project_code}")
    normalized_email = email.lower()
    usage = db.scalar(
        select(EmailUsage).where(
            EmailUsage.email == normalized_email,
            EmailUsage.project_code == project.code,
        )
    )
    if not usage:
        return {"email": email, "status": "not_found", "pooled": False}
    prov, _ = build_provider(db, project)
    if prov.pooled:
        acc = db.scalar(select(EmailAccount).where(EmailAccount.email == normalized_email))
        if acc:
            pool_service.release_account(db, account_id=acc.id)
            return {
                "email": email,
                "status": "released",
                "pooled": True,
                "reusable": True,
                "reusable_for_other_projects": True,
                "message": "已释放回池：本项目不可再取该邮箱，其他项目仍可取",
            }
        return {"email": email, "status": "not_found", "pooled": True}
    prov.release(email)
    return {"email": email, "status": "released", "pooled": False, "reusable": True}


# ────────────────────────────────────────────────────────────────
#  邮件记录
# ────────────────────────────────────────────────────────────────


def save_mail_record(
    db: Session,
    *,
    email: str,
    project_code: str,
    message_id: str = "",
    provider: str = "",
    folder: str = "",
    sender: str = "",
    subject: str = "",
    body_preview: str = "",
    body: str = "",
    code: str = "",
    code_source: str = "",
    received_at: datetime | str | None = None,
) -> MailRecord:
    normalized_email = email.lower()
    normalized_message_id = message_id[:512]
    if normalized_message_id:
        existing = db.scalar(
            select(MailRecord).where(
                MailRecord.email == normalized_email,
                MailRecord.message_id == normalized_message_id,
            )
        )
        if existing:
            return existing
    parsed_received_at = received_at
    if isinstance(received_at, str):
        try:
            parsed_received_at = datetime.fromisoformat(received_at.replace("Z", "+00:00"))
        except ValueError:
            parsed_received_at = None
    rec = MailRecord(
        email=normalized_email,
        project_code=project_code,
        message_id=normalized_message_id,
        provider=provider[:32],
        folder=folder[:64],
        sender=sender[:254],
        subject=subject[:512],
        body_preview=body_preview,
        body=body,
        code=code[:32],
        code_source=code_source[:16],
        received_at=parsed_received_at or _now(),
    )
    db.add(rec)
    db.commit()
    db.refresh(rec)
    return rec


def list_mail_records(
    db: Session,
    email: str | None = None,
    project_code: str | None = None,
    limit: int = 100,
    offset: int = 0,
) -> list[MailRecord]:
    stmt = select(MailRecord).order_by(MailRecord.received_at.desc())
    if email:
        stmt = stmt.where(MailRecord.email == email.lower())
    if project_code:
        stmt = stmt.where(MailRecord.project_code == project_code)
    stmt = stmt.limit(limit).offset(offset)
    return list(db.scalars(stmt))


def count_mail_records(db: Session) -> int:
    """mail_records 总条数（分页用）。"""
    from sqlalchemy import func

    return db.scalar(select(func.count()).select_from(MailRecord)) or 0


# ────────────────────────────────────────────────────────────────
#  临时邮箱列表（按邮箱聚合，仿 MailNest /emails/temporary）
# ────────────────────────────────────────────────────────────────


def list_email_summaries(
    db: Session,
    project_code: str | None = None,
    limit: int = 20,
    offset: int = 0,
) -> tuple[list[dict[str, Any]], int]:
    """按邮箱聚合出"临时邮箱列表"（每行一个邮箱地址）。

    数据源：
      - EmailUsage：该邮箱被哪个项目绑定、首次使用时间（获取时间）
      - EmailAccount：固定池账号状态（available/using/dead），临时源无记录
      - MailRecord：最近一封收件的时间与验证码

    返回 (rows, total)。rows 每项：
      {email, project_code, project_name, pooled, account_status,
       first_used_at, last_received_at, code, mail_count}
    """
    base = select(EmailUsage)
    if project_code:
        base = base.where(EmailUsage.project_code == project_code)
    total = db.scalar(select(func.count()).select_from(base.subquery())) or 0

    usages = list(
        db.scalars(
            base.order_by(EmailUsage.first_used_at.desc())
            .limit(limit)
            .offset(offset)
        )
    )
    emails = [u.email.lower() for u in usages]
    if not emails:
        return [], total

    # 项目名
    proj_names = {
        p.code: p.name or p.code
        for p in db.scalars(select(Project))
    }
    # 固定池账号状态
    acc_map = {
        a.email.lower(): a
        for a in db.scalars(select(EmailAccount).where(EmailAccount.email.in_(emails)))
    }
    # 每个邮箱的最新一封邮件（含验证码）：窗口函数按邮箱分组取最新，
    # 避免全局 limit 截断导致部分邮箱取不到最新收件
    latest: dict[str, MailRecord] = {}
    if emails:
        row_num = func.row_number().over(
            partition_by=MailRecord.email,
            order_by=MailRecord.received_at.desc(),
        ).label("_rn")
        subq = (
            select(MailRecord, row_num)
            .where(MailRecord.email.in_(emails))
            .subquery()
        )
        for rec in db.scalars(
            select(MailRecord).join(subq, subq.c.id == MailRecord.id).where(subq.c._rn == 1)
        ):
            latest.setdefault(rec.email.lower(), rec)
    # 每个邮箱收件数
    counts = dict(
        db.execute(
            select(MailRecord.email, func.count(MailRecord.id))
            .where(MailRecord.email.in_(emails))
            .group_by(MailRecord.email)
        ).all()
    )

    rows: list[dict[str, Any]] = []
    for u in usages:
        key = u.email.lower()
        acc = acc_map.get(key)
        rec = latest.get(key)
        rows.append({
            "email": u.email,
            "project_code": u.project_code,
            "project_name": proj_names.get(u.project_code, u.project_code),
            "pooled": acc is not None,
            "account_status": acc.status if acc else "",
            "first_used_at": u.first_used_at,
            "last_received_at": rec.received_at if rec else None,
            "code": rec.code if rec else "",
            "mail_count": counts.get(key, 0),
        })
    return rows, total
