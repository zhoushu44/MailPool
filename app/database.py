"""SQLite 数据层：engine / session / 建表。"""
from __future__ import annotations

from pathlib import Path

from sqlalchemy import create_engine, event
from sqlalchemy.orm import DeclarativeBase, sessionmaker

from .config import DB_PATH, DATA_DIR


class Base(DeclarativeBase):
    pass


def _ensure_data_dir() -> None:
    Path(DATA_DIR).mkdir(parents=True, exist_ok=True)


def build_engine(db_path: str | Path | None = None):
    _ensure_data_dir()
    path = db_path or DB_PATH
    engine = create_engine(
        f"sqlite:///{path}",
        connect_args={"check_same_thread": False},
    )

    @event.listens_for(engine, "connect")
    def _set_sqlite_pragma(dbapi_connection, connection_record):
        cursor = dbapi_connection.cursor()
        try:
            cursor.execute("PRAGMA journal_mode=WAL")
        except Exception:
            try:
                cursor.execute("PRAGMA journal_mode=MEMORY")
            except Exception:
                pass
        cursor.execute("PRAGMA foreign_keys=ON")
        cursor.close()

    return engine


engine = build_engine()
SessionLocal = sessionmaker(bind=engine, autoflush=False, expire_on_commit=False)


def get_db():
    """FastAPI 依赖：每请求一个 session。"""
    db = SessionLocal()
    try:
        yield db
    finally:
        db.close()


def init_db() -> None:
    from . import models  # noqa: F401  确保模型已注册

    Base.metadata.create_all(bind=engine)
    # 高频排序字段补索引（下单流水按首次使用时间分页）
    with engine.begin() as conn:
        conn.exec_driver_sql(
            "CREATE INDEX IF NOT EXISTS ix_email_usages_first_used_at "
            "ON email_usages (first_used_at)"
        )
        # 计费相关索引（幂等迁移；老库没有这些表时由 create_all 建好）
        for sql in (
            "CREATE INDEX IF NOT EXISTS ix_email_orders_user_billing "
            "ON email_orders (user_id, billing_status)",
            "CREATE INDEX IF NOT EXISTS ix_balance_logs_user_created "
            "ON balance_logs (user_id, created_at)",
            "CREATE INDEX IF NOT EXISTS ix_recharge_orders_user_status "
            "ON recharge_orders (user_id, status)",
        ):
            try:
                conn.exec_driver_sql(sql)
            except Exception:
                pass  # 表还没建（首次启动前）→ 忽略
        # 逐列幂等迁移；记录新增列，只有首次拆分时才从旧 status 回填。
        account_columns = {
            row[1] for row in conn.exec_driver_sql("PRAGMA table_info(email_accounts)")
        }
        migration_sql = (
            "check_result TEXT DEFAULT ''",
            "fail_count INTEGER DEFAULT 0",
            "pool_status VARCHAR(16) DEFAULT 'available'",
            "health_status VARCHAR(16) DEFAULT 'unknown'",
            "health_reason VARCHAR(32) DEFAULT ''",
            "claimed_at DATETIME",
            "tags VARCHAR(255) DEFAULT ''",
            "proxy VARCHAR(255) DEFAULT ''",
        )
        added_account_columns = set()
        for definition in migration_sql:
            column = definition.split()[0]
            if column in account_columns:
                continue
            conn.exec_driver_sql(f"ALTER TABLE email_accounts ADD COLUMN {definition}")
            added_account_columns.add(column)

        for sql in (
            "ALTER TABLE mail_records ADD COLUMN message_id VARCHAR(512) DEFAULT ''",
            "ALTER TABLE mail_records ADD COLUMN provider VARCHAR(32) DEFAULT ''",
            "ALTER TABLE mail_records ADD COLUMN folder VARCHAR(64) DEFAULT ''",
            "ALTER TABLE mail_records ADD COLUMN body_preview TEXT DEFAULT ''",
            "ALTER TABLE mail_records ADD COLUMN code_source VARCHAR(16) DEFAULT ''",
        ):
            column = sql.split()[5]
            existing = {
                row[1] for row in conn.exec_driver_sql("PRAGMA table_info(mail_records)")
            }
            if column not in existing:
                conn.exec_driver_sql(sql)

        conn.exec_driver_sql(
            "CREATE INDEX IF NOT EXISTS ix_mail_records_email_message_id "
            "ON mail_records (email, message_id)"
        )
        if {"pool_status", "health_status"} & added_account_columns:
            conn.exec_driver_sql(
                "UPDATE email_accounts SET "
                "pool_status = CASE WHEN status = 'using' THEN 'using' ELSE 'available' END, "
                "health_status = CASE "
                "WHEN status IN ('healthy', 'unhealthy', 'dead') THEN status "
                "ELSE 'unknown' END, "
                "health_reason = CASE WHEN status IN ('unhealthy', 'dead') THEN status ELSE '' END"
            )
