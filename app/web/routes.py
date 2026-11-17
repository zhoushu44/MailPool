"""管理后台页面路由（Jinja2 服务端渲染）。"""
from __future__ import annotations

from fastapi import APIRouter, Depends, Form, HTTPException, Request
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.templating import Jinja2Templates
from sqlalchemy.orm import Session

from ..config import PROJECT_LOGOS, STATIC_DIR, TEMPLATE_DIR, project_logo_src
from ..database import get_db
from ..providers import get_provider_class, list_providers
from ..services import billing_service, mail_service, pool_service, settings_service
from .session import clear_session, get_session, set_session

router = APIRouter()
# auto_reload=False：模板只在首次编译，之后直接走缓存。
# 开着 auto_reload 时每个请求都要 stat 一遍全部模板文件（bind mount / 网络盘上很慢）。
templates = Jinja2Templates(directory=str(TEMPLATE_DIR))
templates.env.auto_reload = False


def _current_user_id(request: Request, db: Session) -> int | None:
    """当前登录用户 id（会话里的 user_id）。

    没登录返回 None —— 计费页面按 0 处理（查不到流水）。
    """
    uid = get_session(request).get("user_id")
    if not uid:
        return None
    try:
        return int(uid)
    except (TypeError, ValueError):
        return None


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


def _user_name_map(db: Session, limit: int = 500) -> dict[int, str]:
    """用户 id → 用户名 的映射（管理页显示用）。"""
    users, _ = billing_service.list_users(db, limit=limit)
    return {u.id: u.username for u in users}


def _is_admin(request: Request) -> bool:
    """当前请求是不是管理员（会话登录 或 带了正确的管理员口令头）。

    管理员能看全局数据（所有客户的订单/流水），客户只能看自己的。
    """
    from ..api.auth import check_admin_access

    return check_admin_access(request)


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
        # 合并而不是覆盖：管理员与客户可能共用同一个 cookie
        sess = get_session(request)
        sess["admin_ok"] = True
        set_session(response, sess)
        return response
    return _render(request, "login.html", error="口令错误")


@router.get("/logout")
def logout(request: Request):
    response = RedirectResponse("/login", status_code=303)
    clear_session(response)
    return response


# ── 用户登录（客户自助查余额 / 充值）───────────────────────────


@router.get("/user/login", response_class=HTMLResponse)
def user_login_page(request: Request):
    return _render(request, "user_login.html")


@router.post("/user/login")
def user_login_submit(
    request: Request,
    username: str = Form(...),
    password: str = Form(...),
    db: Session = Depends(get_db),
):
    """客户登录。成功后会话里记 user_id，充值/流水页按这个身份查。"""
    from ..api.billing_admin_api import verify_password

    user = billing_service.get_user_by_name(db, username.strip())
    if user is None or user.status != 1 or not user.password_hash:
        return _render(request, "user_login.html", error="用户名或密码错误", username=username)
    if not verify_password(password, user.password_hash):
        return _render(request, "user_login.html", error="用户名或密码错误", username=username)
    sess = get_session(request)
    sess["user_id"] = user.id
    sess["username"] = user.username
    response = RedirectResponse("/recharges", status_code=303)
    set_session(response, sess)
    return response


@router.get("/user/logout")
def user_logout(request: Request):
    """退出客户登录（不影响管理员身份）。"""
    sess = get_session(request)
    sess.pop("user_id", None)
    sess.pop("username", None)
    response = RedirectResponse("/user/login", status_code=303)
    set_session(response, sess)
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
    billing_on = billing_service.is_billing_enabled(db)
    for p in projects:
        _attach_project_meta(db, p, avail_cache)
        try:
            cfg = _json.loads(p.config_json or "{}")
        except Exception:
            cfg = {}
        p.card_query = cfg.get("query", "") or cfg.get("sender_domains", "")
        p.card_keywords = cfg.get("mail_keywords", "") or ""
        p.card_duration = (cfg.get("timeout") or "") or (settings.default_timeout if hasattr(settings, "default_timeout") else "")
        # 真实单价（计费关闭时为空串 → 页面显示「免费」）
        price_milli = billing_service.project_price(db, p) if billing_on else 0
        p.card_price = billing_service.fmt_milli(price_milli) if billing_on else ""
        orig = cfg.get("original_price") if billing_on else ""
        p.card_original_price = (
            billing_service.fmt_milli(billing_service.to_milli(orig)) if orig else ""
        )
    providers = list_providers()
    return _render(
        request, "buy_email.html", active="buy-email",
        projects=projects, providers=providers, billing_on=billing_on,
    )


@router.get("/orders", response_class=HTMLResponse)
def orders_page(
    request: Request,
    project_code: str = "",
    page: int = 1,
    page_size: int = 20,
    db: Session = Depends(get_db),
):
    """下单流水页（仿 MailNest /orders）。

    计费开启时读 email_orders（真订单：价格 + 计费状态 pending/charged/free）；
    计费关闭时回落到 email_usages（纯取号记录）。

    管理员看全部客户的订单；客户只看自己的。
    """
    page = max(1, page)
    page_size = min(max(1, page_size), 100)
    projects = mail_service.list_projects(db)
    billing_on = billing_service.is_billing_enabled(db)
    uid = _current_user_id(request, db) or 0
    is_admin = _is_admin(request)

    if billing_on:
        orders, total = billing_service.list_email_orders(
            db, None if is_admin else uid,
            limit=page_size, offset=(page - 1) * page_size,
            project_code=project_code or None,
        )
        proj_names = {p.code: p.name or p.code for p in projects}
        user_names = {}
        if is_admin:
            user_names = _user_name_map(db)
        rows = [
            {
                "email": o.email,
                "project_code": o.project_code,
                "project_name": proj_names.get(o.project_code, o.project_code),
                "price": billing_service.fmt_milli(o.price),
                "billing_status": o.billing_status,
                "charged_amount": billing_service.fmt_milli(o.charged_amount),
                "first_used_at": o.created_at,
                "last_received_at": o.charged_at,
                "code": "",
                "mail_count": 0,
                "user_id": o.user_id,
                "username": user_names.get(o.user_id, ""),
            }
            for o in orders
        ]
    else:
        rows, total = mail_service.list_email_summaries(
            db,
            project_code=project_code or None,
            limit=page_size,
            offset=(page - 1) * page_size,
        )
    total_pages = max(1, (total + page_size - 1) // page_size)
    return _render(
        request, "orders.html", active="orders",
        rows=rows, total=total, page=page, page_size=page_size,
        total_pages=total_pages, projects=projects,
        filter_project=project_code, billing_on=billing_on, is_admin=is_admin,
    )


@router.get("/balances", response_class=HTMLResponse)
def balances_page(
    request: Request,
    page: int = 1,
    page_size: int = 20,
    log_type: str = "",
    db: Session = Depends(get_db),
):
    """费用明细页（仿 MailNest /balances）：真余额流水。

    计费开启时读 balance_logs（带变动前后余额/冻结额）；
    计费关闭时回落到 mail_records（纯收件流水，全部免费）。

    管理员看全部客户的流水；客户只看自己的。
    """
    page = max(1, page)
    page_size = min(max(1, page_size), 100)
    from ..config import settings

    billing_on = billing_service.is_billing_enabled(db)
    is_admin = _is_admin(request)
    uid = _current_user_id(request, db) or 0
    if billing_on:
        rows, total = billing_service.list_balance_logs(
            db, None if is_admin else uid,
            limit=page_size, offset=(page - 1) * page_size,
            log_type=log_type or None,
        )
        user_names = {}
        if is_admin:
            user_names = _user_name_map(db)
        records = []
        for r in rows:
            records.append({
                "id": r.id,
                "log_type": r.log_type,
                "amount": billing_service.fmt_milli(r.amount),
                "balance_before": billing_service.fmt_milli(r.balance_before),
                "balance_after": billing_service.fmt_milli(r.balance_after),
                "frozen_before": billing_service.fmt_milli(r.frozen_before),
                "frozen_after": billing_service.fmt_milli(r.frozen_after),
                "remark": r.remark,
                "user_id": r.user_id,
                "username": user_names.get(r.user_id, ""),
                "created_at": r.created_at,
            })
    else:
        recs = mail_service.list_mail_records(
            db, limit=page_size, offset=(page - 1) * page_size,
        )
        total = mail_service.count_mail_records(db)
        records = [
            {
                "id": r.id,
                "log_type": "",
                "amount": "免费",
                "balance_before": "—",
                "balance_after": "—",
                "frozen_before": "—",
                "frozen_after": "—",
                "remark": f"{r.email} {r.project_code}".strip(),
                "created_at": r.received_at,
            }
            for r in recs
        ]
    total_pages = max(1, (total + page_size - 1) // page_size)
    snapshot = billing_service.balance_snapshot(db, _current_user_id(request, db) or 0)
    return _render(
        request, "balances.html", active="balances",
        records=records, total=total, page=page, page_size=page_size,
        total_pages=total_pages, billing_on=billing_on, snapshot=snapshot,
        log_type=log_type, is_admin=is_admin,
    )


@router.get("/recharges", response_class=HTMLResponse)
def recharges_page(
    request: Request,
    page: int = 1,
    page_size: int = 20,
    db: Session = Depends(get_db),
):
    """余额与充值页（仿 MailNest /recharges）。

    客户看自己的余额和充值订单；管理员看全部客户的充值订单。
    """
    page = max(1, page)
    page_size = min(max(1, page_size), 100)
    uid = _current_user_id(request, db) or 0
    is_admin = _is_admin(request)
    snapshot = billing_service.balance_snapshot(db, uid)
    rows, total = billing_service.list_recharge_orders(
        db, user_id=None if is_admin else (uid or None),
        limit=page_size, offset=(page - 1) * page_size
    )
    total_pages = max(1, (total + page_size - 1) // page_size)
    from ..services import payment_service

    user_names = {}
    if is_admin:
        user_names = _user_name_map(db)
    for r in rows:
        r.username = user_names.get(r.user_id, "")

    return _render(
        request, "recharges.html", active="recharges",
        snapshot=snapshot, rows=rows, total=total, page=page,
        page_size=page_size, total_pages=total_pages,
        channels=payment_service.list_channels(db),
        billing_config=settings_service.get_billing_config(db),
        is_admin=is_admin,
    )


@router.get("/billing/users", response_class=HTMLResponse)
def billing_users_page(
    request: Request,
    page: int = 1,
    page_size: int = 50,
    db: Session = Depends(get_db),
):
    """用户余额管理（后台）。"""
    page = max(1, page)
    page_size = min(max(1, page_size), 200)
    rows, total = billing_service.list_users(db, limit=page_size, offset=(page - 1) * page_size)
    return _render(
        request, "billing_users.html", active="billing-users",
        rows=rows, total=total, page=page, page_size=page_size,
        total_pages=max(1, (total + page_size - 1) // page_size),
    )


@router.get("/billing/cards", response_class=HTMLResponse)
def billing_cards_page(request: Request, db: Session = Depends(get_db)):
    """充值卡管理（后台）。"""
    from ..services import payment_service

    return _render(
        request, "billing_cards.html", active="billing-cards",
        billing_config=settings_service.get_billing_config(db),
        payment_service=payment_service,
    )


@router.get("/account", response_class=HTMLResponse)
def account_page(request: Request, db: Session = Depends(get_db)):
    """账户资料页（仿 MailNest /account）：基础信息 + API Key。

    管理员看全局 Key；客户看自己名下的 Key（计费 key）。
    """
    keys = settings_service.get_api_keys(db)
    uid = _current_user_id(request, db)
    user = billing_service.get_user(db, uid) if uid else None
    user_keys = settings_service.list_user_api_keys(db, uid) if uid else []
    return _render(
        request, "account.html", active="account",
        api_key=keys[0] if keys else "",
        api_keys=keys,
        user=user,
        user_keys=user_keys,
        snapshot=billing_service.balance_snapshot(db, uid or 0),
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
