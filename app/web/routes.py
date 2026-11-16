"""管理后台页面路由（Jinja2 服务端渲染）。"""
from __future__ import annotations

from fastapi import APIRouter, Depends, Form, HTTPException, Request
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.templating import Jinja2Templates
from sqlalchemy.orm import Session

from ..config import PROJECT_LOGOS, STATIC_DIR, TEMPLATE_DIR, project_logo_src
from ..database import get_db
from ..providers import get_provider_class, list_providers
from ..services import mail_service, pool_service, settings_service
from .session import clear_session, get_session, set_session

router = APIRouter()
# auto_reload=False：模板只在首次编译，之后直接走缓存。
# 开着 auto_reload 时每个请求都要 stat 一遍全部模板文件（bind mount / 网络盘上很慢）。
templates = Jinja2Templates(directory=str(TEMPLATE_DIR))
templates.env.auto_reload = False


def asset_version() -> str:
    """静态资源版本号：用 style.css 的 mtime，内容不变则 URL 不变（可正常缓存）。"""
    try:
        return str(int((STATIC_DIR / "css" / "style.css").stat().st_mtime))
    except OSError:
        return "1"


templates.env.globals["asset_version"] = asset_version

# 不需要登录的公开路由（登录页、logout、文档站、对外 API）
PUBLIC_PATHS = {"/login", "/logout"}


def _admin_ok(request: Request) -> bool:
    return bool(get_session(request).get("admin_ok"))


def _render(request: Request, template: str, **ctx):
    from ..config import settings

    ctx.setdefault("request", request)
    ctx.setdefault("admin_ok", _admin_ok(request))
    ctx.setdefault("active", "")
    ctx.setdefault("has_password", bool(settings.admin_password))
    # 新式调用（request 在前）：starlette >= 0.29 与 1.x 都支持；
    # 旧式 TemplateResponse(name, ctx) 在 starlette 1.x 会把 dict 当模板名而报
    # TypeError: unhashable type: 'dict'（部署到新环境时踩过）。
    return templates.TemplateResponse(request, template, ctx)


# ── 登录 ──────────────────────────────────────────────────────


@router.get("/login", response_class=HTMLResponse)
def login_page(request: Request):
    return _render(request, "login.html")


@router.post("/login")
def login_submit(
    request: Request,
    password: str = Form(...),
):
    import secrets

    from ..config import settings

    if settings.admin_password and secrets.compare_digest(password, settings.admin_password):
        response = RedirectResponse("/", status_code=303)
        set_session(response, {"admin_ok": True})
        return response
    return _render(request, "login.html", error="口令错误")


@router.get("/logout")
def logout(request: Request):
    response = RedirectResponse("/login", status_code=303)
    clear_session(response)
    return response


# ── 主页面 ────────────────────────────────────────────────────


def _project_available(db: Session, provider_id: str) -> int | None:
    """项目可用邮箱数量：固定号池返回"可 claim"（未被其他项目占用）账号数，临时源返回 None。"""
    try:
        pooled = bool(get_provider_class(provider_id).pooled)
    except Exception:
        pooled = False
    if not pooled:
        return None
    return pool_service.count_claimable(db, provider=provider_id)


def _attach_project_meta(db: Session, project, avail_cache: dict[str, int] | None = None) -> None:
    """给项目附加页面展示字段：available 数量与 logo 地址。

    avail_cache：同一请求内按 provider 缓存"可 claim 数"。53 个项目共用 outlook 号池时，
    原来每个项目都跑一次 count_claimable（53 次全表扫描 → /projects 要 5 秒）。
    """
    try:
        pooled = bool(get_provider_class(project.provider).pooled)
    except Exception:
        pooled = False
    if not pooled:
        project.available = None
    else:
        cache = avail_cache if avail_cache is not None else {}
        if project.provider not in cache:
            cache[project.provider] = pool_service.count_claimable(db, provider=project.provider)
        project.available = cache[project.provider]
    project.is_pooled = project.available is not None
    try:
        import json
        cfg = json.loads(project.config_json or "{}")
    except Exception:
        cfg = {}
    project.logo_src = project_logo_src(cfg.get("logo"))


def _logos_with_src() -> list[dict[str, str]]:
    """内置 logo 库附加解析后的图片地址（.png / .svg 自动匹配）。"""
    return [
        {"id": l["id"], "name": l["name"], "src": project_logo_src(l["id"])}
        for l in PROJECT_LOGOS
    ]


@router.get("/", response_class=HTMLResponse)
def dashboard(request: Request, db: Session = Depends(get_db)):
    counts = pool_service.count_accounts(db)
    projects = mail_service.list_projects(db)
    prov_meta = {p["id"]: p for p in list_providers()}
    avail_cache: dict[str, int] = {}
    for p in projects:
        _attach_project_meta(db, p, avail_cache)
        p.provider_name = prov_meta.get(p.provider, {}).get("name", p.provider)
    recent = mail_service.list_mail_records(db, limit=10)
    return _render(
        request, "dashboard.html", active="dashboard",
        counts=counts, projects=projects, recent=recent,
        logos=_logos_with_src(),
    )


@router.get("/overview", response_class=HTMLResponse)
def overview_page(request: Request, db: Session = Depends(get_db)):
    return _render(request, "overview.html", active="overview")



@router.get("/accounts", response_class=HTMLResponse)
def accounts_page(request: Request, db: Session = Depends(get_db)):
    return _render(request, "accounts.html", active="accounts")


@router.get("/mailbox", response_class=HTMLResponse)
def mailbox_page(request: Request, email: str = ""):
    return _render(request, "mailbox.html", active="mailbox", selected_email=email)



@router.get("/projects", response_class=HTMLResponse)
def projects_page(request: Request, db: Session = Depends(get_db)):
    projects = mail_service.list_projects(db)
    avail_cache: dict[str, int] = {}
    for p in projects:
        _attach_project_meta(db, p, avail_cache)
    providers = list_providers()
    return _render(
        request, "projects.html", active="projects",
        projects=projects, providers=providers,
        logos=_logos_with_src(),
    )


@router.get("/pool", response_class=HTMLResponse)
def pool_page(request: Request, db: Session = Depends(get_db), limit: int = 200, offset: int = 0):
    """邮箱池页。

    limit 默认 200（原来固定 500）：500 行 HTML 403KB / 6600 个 DOM 节点，
    传输+解析要 2 秒以上。分页后首屏明显更快，需要的可以用 ?limit=500 或翻页。
    """
    limit = max(20, min(int(limit or 200), 1000))
    offset = max(0, int(offset or 0))
    accounts = pool_service.list_accounts(db, limit=limit, offset=offset)
    counts = pool_service.count_accounts(db)
    health = pool_service.health_summary(db)
    cid_stats = pool_service.stats_by_client_id(db, provider="outlook")
    providers = list_providers()
    used_by = pool_service.email_usage_map(db)
    for acc in accounts:
        acc.used_by = used_by.get(acc.email.lower(), "")
    return _render(
        request, "pool.html", active="pool",
        accounts=accounts, counts=counts, health=health,
        cid_stats=cid_stats, providers=providers,
        page_limit=limit, page_offset=offset,
    )


@router.get("/buy-email", response_class=HTMLResponse)
def buy_email_page(request: Request, db: Session = Depends(get_db)):
    """商城页（仿 MailNest /buy-email）：项目商品卡片，购买=取号。"""
    import json as _json

    from ..config import settings

    projects = mail_service.list_projects(db)
    avail_cache: dict[str, int] = {}
    for p in projects:
        _attach_project_meta(db, p, avail_cache)
        try:
            cfg = _json.loads(p.config_json or "{}")
        except Exception:
            cfg = {}
        p.card_query = cfg.get("query", "") or cfg.get("sender_domains", "")
        p.card_keywords = cfg.get("mail_keywords", "") or ""
        p.card_duration = (cfg.get("timeout") or "") or (settings.default_timeout if hasattr(settings, "default_timeout") else "")
    providers = list_providers()
    return _render(
        request, "buy_email.html", active="buy-email",
        projects=projects, providers=providers,
    )


@router.get("/orders", response_class=HTMLResponse)
def orders_page(
    request: Request,
    project_code: str = "",
    page: int = 1,
    page_size: int = 20,
    db: Session = Depends(get_db),
):
    """下单流水页（仿 MailNest /orders）：按邮箱聚合的取号记录。"""
    page = max(1, page)
    page_size = min(max(1, page_size), 100)
    rows, total = mail_service.list_email_summaries(
        db,
        project_code=project_code or None,
        limit=page_size,
        offset=(page - 1) * page_size,
    )
    projects = mail_service.list_projects(db)
    total_pages = max(1, (total + page_size - 1) // page_size)
    return _render(
        request, "orders.html", active="orders",
        rows=rows, total=total, page=page, page_size=page_size,
        total_pages=total_pages, projects=projects,
        filter_project=project_code,
    )


@router.get("/balances", response_class=HTMLResponse)
def balances_page(
    request: Request,
    page: int = 1,
    page_size: int = 20,
    db: Session = Depends(get_db),
):
    """费用明细页（仿 MailNest /balances）：未接入计费，展示收件流水。"""
    page = max(1, page)
    page_size = min(max(1, page_size), 100)
    records = mail_service.list_mail_records(
        db, limit=page_size, offset=(page - 1) * page_size,
    )
    from sqlalchemy import func, select
    from ..models import MailRecord
    total = db.scalar(select(func.count()).select_from(MailRecord)) or 0
    total_pages = max(1, (total + page_size - 1) // page_size)
    return _render(
        request, "balances.html", active="balances",
        records=records, total=total, page=page, page_size=page_size,
        total_pages=total_pages,
    )


@router.get("/account", response_class=HTMLResponse)
def account_page(request: Request, db: Session = Depends(get_db)):
    """账户资料页（仿 MailNest /account）：基础信息 + API Key。"""
    keys = settings_service.get_api_keys(db)
    return _render(
        request, "account.html", active="account",
        api_key=keys[0] if keys else "",
        api_keys=keys,
    )


@router.get("/settings", response_class=HTMLResponse)
def settings_page(request: Request, db: Session = Depends(get_db)):
    providers = []
    for meta in list_providers():
        cfg = settings_service.get_provider_config(db, meta["id"])
        providers.append({**meta, "config": cfg})
    keys = settings_service.get_api_keys(db)
    heartbeat_cfg = settings_service.get_heartbeat_config(db)
    heartbeat_last = settings_service.get_heartbeat_last_run(db)
    health_check_cfg = settings_service.get_health_check_config(db)
    health_check_last = settings_service.get_health_check_last_run(db)
    refresh_cfg = settings_service.get_refresh_config(db)
    refresh_last = settings_service.get_refresh_last_run(db)
    return _render(
        request, "settings.html", active="settings",
        providers=providers, api_keys=keys,
        heartbeat_cfg=heartbeat_cfg, heartbeat_last=heartbeat_last,
        health_check_cfg=health_check_cfg, health_check_last=health_check_last,
        refresh_cfg=refresh_cfg, refresh_last=refresh_last,
    )


# ── 开发文档（仿迈巢 docs 站）─────────────────────


def _doc_render(request: Request, template: str, doc_active: str, title: str):
    return templates.TemplateResponse(
        request, template,
        {"request": request, "doc_active": doc_active, "doc_title": title},
    )


@router.get("/docs", response_class=HTMLResponse)
def docs_index():
    return RedirectResponse("/docs/quick-start", status_code=302)


@router.get("/docs/quick-start", response_class=HTMLResponse)
def docs_quick_start(request: Request):
    return _doc_render(request, "docs/quick-start.html", "quick-start", "快速开始")


@router.get("/docs/temporary-email", response_class=HTMLResponse)
def docs_temporary_email(request: Request):
    return _doc_render(request, "docs/temporary-email.html", "temporary", "临时邮箱")


@router.get("/docs/api-overview", response_class=HTMLResponse)
def docs_api_overview(request: Request):
    return _doc_render(request, "docs/api-overview.html", "api", "API 文档")


@router.get("/docs/errors", response_class=HTMLResponse)
def docs_errors(request: Request):
    return _doc_render(request, "docs/errors.html", "errors", "错误码")
