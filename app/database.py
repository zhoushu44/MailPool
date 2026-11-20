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


def _unique_index_columns(conn, table: str) -> list[list[str]]:
    """返回表上所有唯一索引覆盖的列组合（用于检测老约束）。"""
    out = []
    for row in conn.exec_driver_sql(f"PRAGMA index_list({table})").fetchall():
        # PRAGMA index_list: seq, name, unique, origin, partial
        if not row[2]:
            continue
        cols = [r[2] for r in conn.exec_driver_sql(f"PRAGMA index_info({row[1]})").fetchall()]
        out.append(cols)
    return out


def _migrate_usage_constraints(conn) -> None:
    """老库迁移：邮箱使用从「全局一次性」改为「按项目一次性」。

    - email_usages：email 单列唯一 → (email, project_code) 组合唯一
      （A 项目用过的邮箱，其他项目仍可绑定）
    - email_orders：email 单列唯一 → (email, project_code) 组合唯一

    SQLite 没法 DROP 列约束，只能重建表。老数据原样拷贝（老约束保证
    email 无重复行，拷贝不会撞新约束）。
    """
    jobs = [
        (
            "email_usages",
            "id INTEGER PRIMARY KEY AUTOINCREMENT, "
            "email VARCHAR(254) NOT NULL, "
            "project_code VARCHAR(64) NOT NULL, "
            "first_used_at DATETIME, "
            "CONSTRAINT uq_email_usage_project UNIQUE (email, project_code)",
            "id, email, project_code, first_used_at",
        ),
        (
            "email_orders",
            "id INTEGER PRIMARY KEY AUTOINCREMENT, "
            "user_id INTEGER NOT NULL, "
            "email VARCHAR(254) NOT NULL, "
            "project_code VARCHAR(64), "
            "provider VARCHAR(32), "
            "price BIGINT, "
            "billing_status VARCHAR(16), "
            "hold_amount BIGINT, "
            "charged_amount BIGINT, "
            "created_at DATETIME, "
            "charged_at DATETIME, "
            "released_at DATETIME, "
            "CONSTRAINT uq_email_order_project UNIQUE (email, project_code)",
            "id, user_id, email, project_code, provider, price, billing_status, "
            "hold_amount, charged_amount, created_at, charged_at, released_at",
        ),
    ]
    for table, create_cols, copy_cols in jobs:
        exists = conn.exec_driver_sql(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?", (table,)
        ).fetchone()
        if not exists:
            continue  # 新库由 create_all 直接按新约束建
        uniques = _unique_index_columns(conn, table)
        # 已经是 (email, project_code) 组合唯一 → 无需迁移
        if any(set(cols) == {"email", "project_code"} for cols in uniques):
            continue
        conn.exec_driver_sql(f"ALTER TABLE {table} RENAME TO {table}_old")
        conn.exec_driver_sql(f"CREATE TABLE {table} ({create_cols})")
        conn.exec_driver_sql(
            f"INSERT INTO {table} ({copy_cols}) SELECT {copy_cols} FROM {table}_old"
        )
        conn.exec_driver_sql(f"DROP TABLE {table}_old")
        # 补常用索引（RENAME 后跟随旧表被 DROP 了）
        conn.exec_driver_sql(f"CREATE INDEX IF NOT EXISTS ix_{table}_email ON {table}(email)")
        conn.exec_driver_sql(
            f"CREATE INDEX IF NOT EXISTS ix_{table}_project_code ON {table}(project_code)"
        )
    for table, create_cols, copy_cols in jobs:
        exists = conn.exec_driver_sql(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?", (table,)
        ).fetchone()
        if not exists:
            continue  # 新库由 create_all 直接按新约束建
        uniques = _unique_index_columns(conn, table)
        # 已经是 (email, project_code) 组合唯一 → 无需迁移
        if any(set(cols) == {"email", "project_code"} for cols in uniques):
            continue
        conn.exec_driver_sql(f"ALTER TABLE {table} RENAME TO {table}_old")
        conn.exec_driver_sql(f"CREATE TABLE {table} ({create_cols})")
        conn.exec_driver_sql(
            f"INSERT INTO {table} ({copy_cols}) SELECT {copy_cols} FROM {table}_old"
        )
        conn.exec_driver_sql(f"DROP TABLE {table}_old")
        # 补常用索引（RENAME 后跟随旧表被 DROP 了）
        conn.exec_driver_sql(f"CREATE INDEX IF NOT EXISTS ix_{table}_email ON {table}(email)")
        conn.exec_driver_sql(
            f"CREATE INDEX IF NOT EXISTS ix_{table}_project_code ON {table}(project_code)"
        )

    # 老语义把「被用过」的号全部退役了；新语义下绑定只挡对应项目，
    # 健康（非 dead）的 retired 号全部回池。幂等：每次启动跑一遍，
    # 新代码里没有任何路径会把健康号置 retired，所以此后的 rowcount 恒为 0。
    res = conn.exec_driver_sql(
        "UPDATE email_accounts SET pool_status='available', claimed_at=NULL "
        "WHERE pool_status='retired' AND health_status IN ('healthy', 'unknown')"
    )
    conn.exec_driver_sql(
        "UPDATE email_accounts SET status = "
        "CASE WHEN health_status IN ('healthy','unhealthy','dead') "
        "THEN health_status ELSE 'available' END "
        "WHERE pool_status='available'"
    )
    if res.rowcount:
        print(f"[migrate] 按项目一次性：回收 retired 号 {res.rowcount} 个回池")


def init_db() -> None:
    from . import models  # noqa: F401  确保模型已注册

    Base.metadata.create_all(bind=engine)
    # 高频排序字段补索引（下单流水按首次使用时间分页）
    with engine.begin() as conn:
        _migrate_usage_constraints(conn)
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
