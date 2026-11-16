"""管理 API：项目 CRUD / 邮箱池管理 / provider 配置 / API Key 管理。"""
from __future__ import annotations

import json
from datetime import datetime, timezone
from typing import Any

from fastapi import APIRouter, Depends, HTTPException
from pydantic import BaseModel
from sqlalchemy import func, select
from sqlalchemy.orm import Session

from ..database import get_db
from ..models import EmailAccount, MailRecord, Project, TokenRefreshLog
from ..providers import get_provider_class, list_providers
from ..services import mail_service, pool_service, settings_service
from .auth import require_admin

router = APIRouter(prefix="/api/admin", dependencies=[Depends(require_admin)])


# ────────────────────────────────────────────────────────────────
#  总览与 Outlook 收件箱
# ────────────────────────────────────────────────────────────────


@router.get("/overview/summary")
def admin_overview_summary(db: Session = Depends(get_db)) -> dict[str, Any]:
    """返回总览统计。"""
    counts = pool_service.count_accounts(db)
    health = pool_service.health_summary(db)
    today = datetime.now(timezone.utc).date()
    mails_today = db.scalar(select(func.count(MailRecord.id)).where(func.date(MailRecord.received_at) == today.isoformat())) or 0
    codes_today = db.scalar(select(func.count(MailRecord.id)).where(func.date(MailRecord.received_at) == today.isoformat(), MailRecord.code != "")) or 0
    projects_total = db.scalar(select(func.count(Project.id))) or 0
    projects_enabled = db.scalar(select(func.count(Project.id)).where(Project.enabled == 1)) or 0
    return {"code": 0, "data": {
        "accounts": {**counts, "healthy": health["healthy"], "unhealthy": health["unhealthy"], "health": health},
        "pool": {"available": pool_service.count_claimable(db, "outlook"), "claimed": counts["using"], "dead": counts["dead"]},
        "today": {"mails": mails_today, "verification_codes": codes_today},
        "projects": {"total": projects_total, "enabled": projects_enabled},
        "monitoring": {
            "last_scan_at": health.get("last_scan_at", ""),
            "health_rate": round(health["healthy"] / health["total"] * 100, 1) if health["total"] else 0,
            "operations": pool_service.operation_stats(),
        },
    }}


def _outlook_cred(db: Session, email: str) -> dict[str, Any]:
    cred = pool_service.get_account_credentials(db, email.strip().lower())
    if not cred or cred.get("provider") != "outlook":
        raise HTTPException(status_code=404, detail="Outlook 账号不存在")
    return cred


@router.get("/mailbox/messages")
def admin_mailbox_messages(email: str, folder: str = "inbox", top: int = 20, db: Session = Depends(get_db)) -> dict[str, Any]:
    from ..providers.outlook import graph_list_messages
    try:
        messages = graph_list_messages(_outlook_cred(db, email), folder, top)
    except Exception as exc:
        raise HTTPException(status_code=502, detail=f"Graph 获取邮件失败: {exc}")
    return {"code": 0, "data": messages}


@router.get("/mailbox/message")
def admin_mailbox_message(email: str, message_id: str, db: Session = Depends(get_db)) -> dict[str, Any]:
    from ..providers.outlook import graph_get_message
    try:
        message = graph_get_message(_outlook_cred(db, email), message_id)
    except Exception as exc:
        raise HTTPException(status_code=502, detail=f"Graph 获取邮件失败: {exc}")
    return {"code": 0, "data": message}


class AiCodeBody(BaseModel):
    email: str
    message: dict[str, Any] = {}
    use_ai: bool = True


@router.post("/mailbox/ai-code")
def admin_mailbox_ai_code(body: AiCodeBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    _outlook_cred(db, body.email)
    from ..services.code_service import extract_ai
    return {"code": 0, "data": extract_ai(body.message, use_ai=body.use_ai)}




class ProjectBody(BaseModel):
    code: str
    name: str = ""
    provider: str
    config: dict[str, Any] = {}
    enabled: int = 1
    project_id: int | None = None


@router.get("/projects")
def admin_projects(db: Session = Depends(get_db)) -> dict[str, Any]:
    projects = mail_service.list_projects(db)
    data: list[dict[str, Any]] = []
    avail_cache: dict[str, int] = {}
    for p in projects:
        try:
            pooled = bool(get_provider_class(p.provider).pooled)
        except Exception:
            pooled = False
        if pooled:
            if p.provider not in avail_cache:
                avail_cache[p.provider] = pool_service.count_claimable(db, p.provider)
            available = avail_cache[p.provider]
        else:
            available = None
        data.append({
            "id": p.id,
            "code": p.code,
            "name": p.name,
            "provider": p.provider,
            "config": json.loads(p.config_json or "{}"),
            "enabled": p.enabled,
            "pooled": pooled,
            "available": available,
        })
    return {"code": 0, "data": data}


@router.post("/projects")
def admin_save_project(body: ProjectBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    try:
        proj = mail_service.upsert_project(
            db,
            code=body.code.strip(),
            name=body.name.strip(),
            provider=body.provider.strip(),
            config_json=json.dumps(body.config, ensure_ascii=False),
            enabled=body.enabled,
            project_id=body.project_id,
        )
    except Exception as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    return {"code": 0, "data": {"id": proj.id, "code": proj.code}}


@router.delete("/projects/{project_id}")
def admin_delete_project(project_id: int, db: Session = Depends(get_db)) -> dict[str, Any]:
    if not mail_service.delete_project(db, project_id):
        raise HTTPException(status_code=404, detail="项目不存在")
    return {"code": 0, "msg": "deleted"}


# ────────────────────────────────────────────────────────────────
#  邮箱池
# ────────────────────────────────────────────────────────────────


class ImportBody(BaseModel):
    provider: str
    text: str
    remark: str = ""


@router.get("/emails")
def admin_emails(
    provider: str | None = None,
    status: str | None = None,
    pool_status: str | None = None,
    health_status: str | None = None,
    keyword: str | None = None,
    tag: str | None = None,
    limit: int = 200,
    offset: int = 0,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    accounts = pool_service.list_accounts(
        db, provider=provider, status=status, pool_status=pool_status,
        health_status=health_status, keyword=keyword, tag=tag,
        limit=limit, offset=offset,
    )
    counts = pool_service.count_accounts(db, provider=provider)
    used_by = pool_service.email_usage_map(db)
    return {"code": 0, "data": {
        "accounts": [{
            "id": a.id,
            "email": a.email,
            "provider": a.provider,
            "status": a.status,
            "pool_status": a.pool_status or ("using" if a.status == "using" else "available"),
            "health_status": a.health_status or (a.status if a.status in {"healthy", "unhealthy", "dead"} else "unknown"),
            "health_reason": a.health_reason,
            "check_result": a.check_result,
            "fail_count": a.fail_count,
            "remark": a.remark,
            "tags": a.tags,
            "proxy": a.proxy,
            "used_by": used_by.get(a.email.lower(), ""),
            "last_check_at": a.last_check_at.isoformat() if a.last_check_at else "",
            "claimed_at": a.claimed_at.isoformat() if a.claimed_at else "",
        } for a in accounts],
        "counts": counts,
    }}


@router.post("/emails/import")
def admin_import_emails(body: ImportBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    provider = body.provider.strip()
    input_lines = [line.strip() for line in body.text.splitlines() if line.strip() and not line.strip().startswith("#")]
    rows, errors = pool_service.parse_account_text(body.text, provider)
    unique_rows: list[dict[str, Any]] = []
    seen: set[str] = set()
    duplicate_input = 0
    for row in rows:
        email = row["email"]
        if email in seen:
            duplicate_input += 1
            continue
        seen.add(email)
        unique_rows.append(row)
    inserted, skipped, applied_default_cid = pool_service.import_accounts(db, provider, unique_rows, body.remark)
    skipped += duplicate_input
    revived = pool_service._IMPORT_LAST_REVIVED.get("count", 0)
    return {"code": 0, "data": {
        "total": len(input_lines),
        "valid": len(rows),
        "imported": inserted,
        "revived": revived,
        "skipped": skipped,
        "duplicate_input": duplicate_input,
        "applied_default_client_id": applied_default_cid,
        "error_count": len(errors),
        "errors": errors[:20],
        "message": (
            f"导入完成：新增 {inserted} 条"
            + (f"，复活 {revived} 条（原失效/退役）" if revived else "")
            + f"，跳过 {skipped} 条，格式错误 {len(errors)} 条，默认 Client ID 套用 {applied_default_cid} 条"
        ),
    }}


@router.delete("/emails/{account_id}")
def admin_delete_email(account_id: int, db: Session = Depends(get_db)) -> dict[str, Any]:
    """永久删除账号（号池管理 / 风控清理共用）。

    注意：SQLAlchemy 的 session 是 identity map，db.delete 后若再查同一 id 可能
    命中已删除对象，因此统一走 pool_service.delete_account（内部重新 select）。
    """
    if not pool_service.delete_account(db, account_id=account_id):
        raise HTTPException(status_code=404, detail="账号不存在")
    return {"code": 0, "msg": "账号已删除", "message": "账号已删除"}


@router.post("/emails/release/{account_id}")
def admin_release_email(account_id: int, db: Session = Depends(get_db)) -> dict[str, Any]:
    if not pool_service.release_account(db, account_id=account_id):
        raise HTTPException(status_code=404, detail="账号不存在或不在占用中")
    return {"code": 0, "msg": "retired"}


@router.post("/emails/{account_id}/revive")
def admin_revive_email(account_id: int, db: Session = Depends(get_db)) -> dict[str, Any]:
    """恢复 dead / retired 账号为可领取状态（人工确认后使用）。

    只重置状态与失败计数；账号凭证本身不变，恢复后仍是 health_status=unknown，
    建议随后单独跑一次检测确认。
    """
    result = pool_service.revive_account(db, account_id=account_id)
    if not result:
        raise HTTPException(status_code=404, detail="账号不存在")
    return {"code": 0, "data": result, "msg": "账号已恢复为可用"}


@router.post("/emails/health")
def admin_health_check(
    body: dict[str, Any], db: Session = Depends(get_db)
) -> dict[str, Any]:
    provider = str(body.get("provider") or "").strip()
    if not provider:
        raise HTTPException(status_code=400, detail="provider 必填")
    limit = int(body.get("limit") or 50)
    force = bool(body.get("force") or False)
    result = pool_service.check_accounts_health(db, provider, limit=limit, force=force)
    return {"code": 0, "data": result}


class BatchReviveBody(BaseModel):
    provider: str = "outlook"
    health_reason: str | None = None   # 只恢复指定原因（如 credential），空=全部 dead
    dry_run: bool = True               # 默认只预览，需显式 dry_run=false 才真正执行
    limit: int = 500


@router.post("/emails/revive-batch")
def admin_revive_batch(
    body: BatchReviveBody, db: Session = Depends(get_db)
) -> dict[str, Any]:
    """批量恢复 dead 账号（人工确认后使用）。默认 dry_run=true 只预览。

    典型场景：一次性限流把大批号判成 dead，恢复后重新检测即可。
    """
    result = pool_service.revive_dead_accounts(
        db,
        provider=body.provider,
        health_reason=body.health_reason,
        dry_run=body.dry_run,
        limit=min(max(int(body.limit or 500), 1), 5000),
    )
    return {"code": 0, "data": result}


@router.post("/emails/refresh-all")
def admin_batch_refresh(
    body: dict[str, Any], db: Session = Depends(get_db)
) -> dict[str, Any]:
    """一键全量刷新所有可刷新 Outlook 账号的 Token（后台任务 + 串行限流）。

    与原版 outlookEmail 全量刷新一致：执行期间请勿重复发起刷新。
    """
    provider = str(body.get("provider") or "outlook").strip() or "outlook"
    result = pool_service.start_batch_refresh_token(db, provider)
    return {"code": 0, "data": result}


@router.get("/emails/refresh-all/status")
def admin_batch_refresh_status(
    task_id: str, db: Session = Depends(get_db)
) -> dict[str, Any]:
    """查询全量刷新任务进度。"""
    return {"code": 0, "data": pool_service.get_batch_refresh_status(task_id)}


@router.post("/emails/{account_id}/refresh-token")
def admin_refresh_token(account_id: int, db: Session = Depends(get_db)) -> dict[str, Any]:
    """启动 Device Code Flow 刷新 Outlook 账号的 refresh_token。
    返回 verification_uri 和 user_code，用户需在浏览器打开链接并输入代码完成授权。
    """
    try:
        result = pool_service.start_device_code_flow(db, account_id)
    except ValueError as exc:
        raise HTTPException(status_code=400, detail=str(exc))
    except RuntimeError as exc:
        raise HTTPException(status_code=502, detail=str(exc))
    return {"code": 0, "data": result}


@router.get("/emails/{account_id}/refresh-token/status")
def admin_refresh_token_status(account_id: int, db: Session = Depends(get_db)) -> dict[str, Any]:
    """轮询 Device Code Flow 授权状态。"""
    result = pool_service.poll_device_code_flow(db, account_id)
    return {"code": 0, "data": result}


# 说明：DELETE /emails/{account_id} 只在文件上方注册一次
# （admin_delete_email，内部走 pool_service.delete_account）。
# 曾在此处重复注册同名路由，导致「永久删除」分支永远不可达、OpenAPI 也只暴露一个。


class AccountMetaBody(BaseModel):
    tags: str | None = None
    remark: str | None = None
    proxy: str | None = None


@router.patch("/emails/{account_id}")
def admin_update_account_meta(
    account_id: int, body: AccountMetaBody, db: Session = Depends(get_db)
) -> dict[str, Any]:
    """更新账号标签 / 备注 / 代理。"""
    acc = pool_service.update_account_meta(
        db, account_id, tags=body.tags, remark=body.remark, proxy=body.proxy
    )
    if not acc:
        raise HTTPException(status_code=404, detail="账号不存在")
    return {"code": 0, "data": {"id": acc.id, "email": acc.email, "tags": acc.tags, "remark": acc.remark, "proxy": acc.proxy}}


@router.get("/emails/tags")
def admin_email_tags(
    provider: str | None = None, db: Session = Depends(get_db)
) -> dict[str, Any]:
    """聚合去重的标签列表（按使用频率排序）。"""
    return {"code": 0, "data": pool_service.list_tags(db, provider=provider)}


@router.get("/emails/export")
def admin_export_emails(
    provider: str | None = None,
    health_status: str | None = None,
    tag: str | None = None,
    keyword: str | None = None,
    limit: int = 2000,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    """导出账号（含凭证），四段式文本格式。

    返回 {lines: [...], rows: [...]}，lines 可直接下载保存。
    """
    rows = pool_service.export_accounts(
        db,
        provider=provider or None,
        health_status=health_status or None,
        tag=tag or None,
        keyword=keyword or None,
        limit=min(max(limit, 1), 5000),
    )
    lines = [
        f"{r['email']}----{r['password']}----{r['client_id']}----{r['refresh_token']}"
        for r in rows
    ]
    return {"code": 0, "data": {"count": len(lines), "lines": lines, "rows": rows}}


@router.get("/emails/refresh-logs")
def admin_refresh_logs(
    limit: int = 50, offset: int = 0, db: Session = Depends(get_db)
) -> dict[str, Any]:
    """Token 全量/定时刷新历史。"""
    logs = pool_service.list_refresh_logs(db, limit=min(max(limit, 1), 200), offset=max(offset, 0))
    total = db.scalar(select(func.count()).select_from(TokenRefreshLog)) or 0
    return {"code": 0, "data": {
        "logs": [{
            "id": log.id,
            "trigger": log.trigger,
            "provider": log.provider,
            "status": log.status,
            "total": log.total,
            "success": log.success,
            "failed": log.failed,
            "skipped": log.skipped,
            "duration": log.duration,
            "detail": log.detail,
            "created_at": log.created_at.isoformat() if log.created_at else "",
        } for log in logs],
        "total": total,
    }}


@router.get("/codes")
def admin_code_logs(
    email: str | None = None,
    project_code: str | None = None,
    limit: int = 50,
    offset: int = 0,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    """验证码提取日志：按收件时间倒序返回带验证码的邮件。"""
    stmt = select(MailRecord).where(MailRecord.code != "")
    if email:
        stmt = stmt.where(MailRecord.email == email.strip().lower())
    if project_code:
        stmt = stmt.where(MailRecord.project_code == project_code.strip())
    stmt = stmt.order_by(MailRecord.received_at.desc()).limit(min(max(limit, 1), 200)).offset(max(offset, 0))
    rows = list(db.scalars(stmt))
    total_stmt = select(func.count(MailRecord.id)).where(MailRecord.code != "")
    if email:
        total_stmt = total_stmt.where(MailRecord.email == email.strip().lower())
    if project_code:
        total_stmt = total_stmt.where(MailRecord.project_code == project_code.strip())
    total = db.scalar(total_stmt) or 0
    return {"code": 0, "data": {
        "records": [{
            "id": r.id,
            "email": r.email,
            "project_code": r.project_code,
            "provider": r.provider,
            "sender": r.sender,
            "subject": r.subject,
            "code": r.code,
            "code_source": r.code_source,
            "received_at": r.received_at.isoformat() if r.received_at else "",
            "body_preview": (r.body_preview or "")[:200],
        } for r in rows],
        "total": total,
    }}


@router.get("/overview/refreshes")
def admin_overview_refreshes(db: Session = Depends(get_db)) -> dict[str, Any]:
    """概览看板：Token 刷新概况（最近一次 + 今日/全部汇总）。"""
    today = datetime.now(timezone.utc).date().isoformat()
    logs = db.scalars(select(TokenRefreshLog).order_by(TokenRefreshLog.id.desc()).limit(200)).all()
    recent = logs[0] if logs else None
    today_count = sum(1 for l in logs if l.created_at and l.created_at.date().isoformat() == today)
    return {"code": 0, "data": {
        "last": {
            "trigger": recent.trigger if recent else "",
            "total": recent.total if recent else 0,
            "success": recent.success if recent else 0,
            "failed": recent.failed if recent else 0,
            "duration": recent.duration if recent else 0,
            "created_at": recent.created_at.isoformat() if recent and recent.created_at else "",
        },
        "today_count": today_count,
        "history_count": len(logs),
        "config": settings_service.get_refresh_config(db),
        "last_run": settings_service.get_refresh_last_run(db),
    }}


@router.get("/pool/health-summary")
def admin_health_summary(
    provider: str | None = None,
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    return {"code": 0, "data": pool_service.health_summary(db, provider=provider)}


@router.get("/pool/client-id-stats")
def admin_client_id_stats(
    provider: str = "outlook",
    db: Session = Depends(get_db),
) -> dict[str, Any]:
    """按 client_id 维度统计号池健康分布（识别被微软封的 client_id）。"""
    return {"code": 0, "data": pool_service.stats_by_client_id(db, provider=provider)}


@router.get("/pool/health-check-config")
def admin_health_check_config(db: Session = Depends(get_db)) -> dict[str, Any]:
    cfg = settings_service.get_health_check_config(db)
    last_run = settings_service.get_health_check_last_run(db)
    return {"code": 0, "data": {"config": cfg, "last_run": last_run}}


@router.put("/pool/health-check-config")
def admin_save_health_check_config(
    body: dict[str, Any], db: Session = Depends(get_db)
) -> dict[str, Any]:
    cfg = settings_service.save_health_check_config(db, body)
    return {"code": 0, "data": cfg}


# ────────────────────────────────────────────────────────────────
#  心跳保活
# ────────────────────────────────────────────────────────────────


class HeartbeatBody(BaseModel):
    mode: str = "silent"
    limit: int = 50


@router.post("/pool/heartbeat")
def admin_heartbeat(body: HeartbeatBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    result = pool_service.heartbeat(db, mode=body.mode, limit=body.limit)
    settings_service.set_heartbeat_last_run(db, {
        "total": result["total"], "success": result["success"], "failed": result["failed"],
    })
    return {"code": 0, "data": result}


@router.get("/pool/heartbeat-status")
def admin_heartbeat_status(db: Session = Depends(get_db)) -> dict[str, Any]:
    cfg = settings_service.get_heartbeat_config(db)
    last_run = settings_service.get_heartbeat_last_run(db)
    return {"code": 0, "data": {"config": cfg, "last_run": last_run}}


@router.put("/pool/heartbeat-config")
def admin_heartbeat_config(
    body: dict[str, Any], db: Session = Depends(get_db)
) -> dict[str, Any]:
    cfg = settings_service.save_heartbeat_config(db, body)
    return {"code": 0, "data": cfg}


@router.get("/pool/refresh-config")
def admin_refresh_config(db: Session = Depends(get_db)) -> dict[str, Any]:
    cfg = settings_service.get_refresh_config(db)
    last_run = settings_service.get_refresh_last_run(db)
    return {"code": 0, "data": {"config": cfg, "last_run": last_run}}


@router.put("/pool/refresh-config")
def admin_save_refresh_config(
    body: dict[str, Any], db: Session = Depends(get_db)
) -> dict[str, Any]:
    cfg = settings_service.save_refresh_config(db, body)
    return {"code": 0, "data": cfg}


# ────────────────────────────────────────────────────────────────
#  收件（后台详情页刷新用，走会话鉴权）
# ────────────────────────────────────────────────────────────────


class AdminReceiveBody(BaseModel):
    project_code: str
    email: str
    timeout: float = 10


class AdminBuyBody(BaseModel):
    project_code: str


@router.post("/mails/buy")
def admin_buy_email(body: AdminBuyBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    """商城页"购买"按钮：按项目取号（无计费，等价于一次取号）。

    成功返回 {email}，前端随即跳转该邮箱详情页等待验证码。
    """
    from ..providers import ProviderError

    project = mail_service.get_project(db, body.project_code.strip())
    if not project:
        return {"code": 1, "msg": "项目不存在或已停用", "data": None}
    try:
        email = mail_service.acquire_email(db, project.code)
    except ProviderError as exc:
        return {"code": 1, "msg": str(exc) or "取号失败", "data": None}
    except Exception as exc:
        return {"code": 1, "msg": str(exc) or "取号失败", "data": None}
    return {"code": 0, "data": {"email": email}}


@router.post("/mails/receive")
def admin_receive_mail(body: AdminReceiveBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    """详情页"刷新"按钮：后台轮询一次验证码（默认 10s）。

    成功/超时均返回 code=0（超时属正常轮询结果），
    携带 status=success|timeout 与 code。
    """
    try:
        result = mail_service.receive_code(
            db,
            body.project_code.strip(),
            body.email.strip(),
            timeout=body.timeout,
        )
    except Exception as exc:
        return {"code": 1, "msg": str(exc), "data": None}
    return {"code": 0, "data": result}


# ────────────────────────────────────────────────────────────────
#  Provider 配置
# ────────────────────────────────────────────────────────────────


@router.get("/providers")
def admin_providers() -> dict[str, Any]:
    return {"code": 0, "data": list_providers()}


@router.get("/providers/{provider_id}/config")
def admin_get_provider_config(provider_id: str, db: Session = Depends(get_db)) -> dict[str, Any]:
    cfg = settings_service.get_provider_config(db, provider_id)
    return {"code": 0, "data": cfg}


class ProviderConfigBody(BaseModel):
    config: dict[str, Any]


@router.post("/providers/{provider_id}/config")
def admin_save_provider_config(
    provider_id: str, body: ProviderConfigBody, db: Session = Depends(get_db)
) -> dict[str, Any]:
    settings_service.save_provider_config(db, provider_id, body.config)
    return {"code": 0, "msg": "saved"}


@router.post("/providers/{provider_id}/test")
def admin_test_provider(provider_id: str, db: Session = Depends(get_db)) -> dict[str, Any]:
    from ..providers import create_provider

    cfg = settings_service.get_provider_config(db, provider_id)
    try:
        prov = create_provider(provider_id, cfg)
        result = prov.self_test()
    except Exception as exc:
        return {"code": 1, "data": {"ok": False, "message": str(exc)}}
    return {"code": 0, "data": result}


# ────────────────────────────────────────────────────────────────
#  API Key
# ────────────────────────────────────────────────────────────────


@router.get("/api-keys")
def admin_api_keys(db: Session = Depends(get_db)) -> dict[str, Any]:
    return {"code": 0, "data": {"keys": settings_service.get_api_keys(db)}}


class ApiKeyBody(BaseModel):
    key: str | None = None


@router.post("/api-keys")
def admin_add_api_key(body: ApiKeyBody, db: Session = Depends(get_db)) -> dict[str, Any]:
    key = body.key.strip() if body.key else settings_service.generate_api_key()
    keys = settings_service.add_api_key(db, key)
    return {"code": 0, "data": {"keys": keys, "created": key}}


@router.delete("/api-keys/{key}")
def admin_delete_api_key(key: str, db: Session = Depends(get_db)) -> dict[str, Any]:
    keys = settings_service.remove_api_key(db, key)
    return {"code": 0, "data": {"keys": keys}}


# ────────────────────────────────────────────────────────────────
#  验证码正则测试（页面直接验证）
# ────────────────────────────────────────────────────────────────


class CodeTestBody(BaseModel):
    text: str = ""
    regex: str = ""


@router.post("/code-parse-test")
def admin_code_parse_test(body: CodeTestBody) -> dict[str, Any]:
    from ..providers.base import parse_code_patterns, extract_code_by_regex

    text = body.text or ""
    patterns = parse_code_patterns(body.regex)
    result = {
        "ok": False,
        "matched": "",
        "patterns": len(patterns),
        "invalid": len([l for l in str(body.regex or "").splitlines() if l.strip() and not l.strip().startswith("#")]) - len(patterns),
    }
    if text.strip() and patterns:
        result["matched"] = extract_code_by_regex(text, patterns)
        result["ok"] = bool(result["matched"])
    return {"code": 0, "data": result}


