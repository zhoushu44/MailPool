"""MailPool 主入口：FastAPI 应用。

启动：
    python run.py
    # 或 uvicorn app.main:app --host 0.0.0.0 --port 8000
"""
from __future__ import annotations

import logging
from datetime import datetime

from fastapi import FastAPI
from fastapi.responses import HTMLResponse, RedirectResponse
from fastapi.staticfiles import StaticFiles

from .config import STATIC_DIR, settings
from .database import init_db
from .api import admin_api, email_api
from .web import routes

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s [%(name)s] %(message)s",
)
logger = logging.getLogger("mailpool")

app = FastAPI(title="MailPool", docs_url="/swagger", openapi_url="/openapi.json")

# HTML/JSON 压缩：账号页 27KB、号池页 427KB、商城 128KB，压缩后体积降到 ~1/6，
# 远程访问时能显著减少传输时间（服务器在公网，2M 上行带宽很吃紧）。
from starlette.middleware.gzip import GZipMiddleware

app.add_middleware(GZipMiddleware, minimum_size=1024, compresslevel=6)

app.include_router(email_api.public_router)
app.include_router(email_api.router)
app.include_router(admin_api.router)
app.include_router(routes.router)

app.mount("/static", StaticFiles(directory=str(STATIC_DIR)), name="static")


# ── 页面级鉴权 ────────────────────────────────────────────────
# /api/admin/* 已有 router 级依赖；但 app/web/routes.py 的页面此前完全没有校验，
# 任何人打开 /accounts、/settings、/pool 就能看到全部真实账号凭证。
# 这里统一加一道中间件：除登录页/文档/静态资源/API 外，全部要求已授权。
_PUBLIC_PREFIXES = ("/static", "/docs", "/api", "/login", "/logout", "/swagger", "/redoc", "/openapi.json")


@app.middleware("http")
async def admin_page_guard(request: Request, call_next):
    from .api.auth import check_admin_access

    path = request.url.path
    if path.startswith(_PUBLIC_PREFIXES):
        return await call_next(request)
    if not check_admin_access(request):
        if settings.admin_password:
            return RedirectResponse("/login", status_code=303)
        client = request.client.host if request.client else "未知"
        return HTMLResponse(
            "<h1>403 仅允许本机访问</h1>"
            f"<p>后台未设置 MAILPOOL_ADMIN_PASSWORD，当前仅允许回环地址访问（当前来源：{client}）。</p>"
            "<p>如需远程访问，请设置环境变量 MAILPOOL_ADMIN_PASSWORD 后重启服务。</p>",
            status_code=403,
        )
    return await call_next(request)


@app.on_event("startup")
def on_startup() -> None:
    init_db()
    logger.info("MailPool 数据库就绪: %s", settings.data_dir)
    _start_heartbeat_scheduler()


def _start_heartbeat_scheduler() -> None:
    """启动心跳保活定时任务（如果已启用）。"""
    try:
        from apscheduler.schedulers.background import BackgroundScheduler
        from apscheduler.triggers.cron import CronTrigger
    except ImportError:
        logger.warning("apscheduler 未安装，心跳保活定时任务未启用（pip install apscheduler）")
        return

    def _in_window(now, hour: int, minute: int, window_minutes: int = 5) -> bool:
        """判断「现在」是否落在配置的 hour:minute 起的 window 分钟内。

        调度器改成每 5 分钟触发一次（原来固定 minute=0 触发，导致
        配置的分钟 > 0 时永远不满足 now.minute < minute，任务永不执行）。
        """
        return now.hour == hour and minute <= now.minute < min(minute + window_minutes, 60)

    def _heartbeat_job():
        from .database import SessionLocal
        from .services import pool_service, settings_service

        db = SessionLocal()
        try:
            cfg = settings_service.get_heartbeat_config(db)
            if cfg.get("heartbeat_enabled") != "true":
                return
            mode = cfg.get("heartbeat_mode", "silent")
            hour = int(cfg.get("heartbeat_hour", 9))
            minute = int(cfg.get("heartbeat_minute", 0))
            now = datetime.now()
            if not _in_window(now, hour, minute):
                return
            logger.info("心跳保活定时任务启动: mode=%s", mode)
            result = pool_service.heartbeat(db, mode=mode, limit=50)
            settings_service.set_heartbeat_last_run(db, {
                "total": result["total"],
                "success": result["success"],
                "failed": result["failed"],
            })
            logger.info("心跳保活完成: 成功 %d / 失败 %d / 总计 %d",
                        result["success"], result["failed"], result["total"])
        except Exception as e:
            logger.error("心跳保活定时任务异常: %s", e)
        finally:
            db.close()

    def _health_check_job():
        from .database import SessionLocal
        from .services import pool_service, settings_service

        db = SessionLocal()
        try:
            cfg = settings_service.get_health_check_config(db)
            if cfg.get("health_check_enabled") != "true":
                return
            hour = int(cfg.get("health_check_hour", 8))
            minute = int(cfg.get("health_check_minute", 0))
            now = datetime.now()
            if not _in_window(now, hour, minute):
                return
            logger.info("健康扫描定时任务启动")
            result = pool_service.check_accounts_health(db, "outlook", limit=50, force=False)
            settings_service.set_health_check_last_run(db, {
                "total": result["total"],
                "checked": result["checked"],
                "healthy": result["healthy"],
                "unhealthy": result["unhealthy"],
            })
            logger.info("健康扫描完成: 检测 %d / 正常 %d / 异常 %d",
                        result["checked"], result["healthy"], result["unhealthy"])
        except Exception as e:
            logger.error("健康扫描定时任务异常: %s", e)
        finally:
            db.close()

    def _refresh_token_job():
        """定时全量刷新 Token 任务（按配置的小时/分钟判断是否本轮执行）。"""
        from .database import SessionLocal
        from .services import pool_service, settings_service

        db = SessionLocal()
        try:
            cfg = settings_service.get_refresh_config(db)
            if cfg.get("refresh_enabled") != "true":
                return
            hour = int(cfg.get("refresh_hour", 3))
            minute = int(cfg.get("refresh_minute", 0))
            now = datetime.now()
            if not _in_window(now, hour, minute):
                return
            logger.info("定时全量刷新 Token 任务启动")
            result = pool_service.start_batch_refresh_token(db, "outlook", trigger="schedule")
            logger.info("定时全量刷新 Token 已启动: total=%s", result.get("total"))
        except Exception as e:
            logger.error("定时全量刷新 Token 任务异常: %s", e)
        finally:
            db.close()

    scheduler = BackgroundScheduler()
    # 每 5 分钟检查一次，由各 job 自己判断是否落在配置窗口内
    scheduler.add_job(_heartbeat_job, CronTrigger(minute="*/5"))
    scheduler.add_job(_health_check_job, CronTrigger(minute="*/5"))
    scheduler.add_job(_refresh_token_job, CronTrigger(minute="*/5"))
    scheduler.start()
    logger.info("调度器已启动（心跳保活 + 健康扫描 + 定时刷新，每 5 分钟检查配置）")


@app.get("/api")
def api_index():
    return {
        "name": "MailPool",
        "version": "0.1.0",
        "endpoints": {
            "product_info": "GET /api/product/info",
            "buy": "POST /api/v1/email/temporary/buy  {project_code, count}",
            "receive": "POST /api/v1/email/receive  {project_code, email, timeout}",
            "release": "POST /api/v1/email/release  {project_code, email}",
            "mails": "GET /api/v1/mails?email=&project_code=&page=&page_size=",
            "health": "GET /api/v1/health",
        },
        "docs": "/docs",
    }
