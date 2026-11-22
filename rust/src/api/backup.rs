//! S3 备份 API + 定时调度。
//!
//! 端点与 sub2api 对齐：
//!
//! | 方法 | 路径 | 说明 |
//! |---|---|---|
//! | GET  | `/api/admin/backup/config`      | 读配置（secret 脱敏） |
//! | POST | `/api/admin/backup/config`      | 存配置 |
//! | POST | `/api/admin/backup/test`        | 测试连接 |
//! | POST | `/api/admin/backup/run`         | 立即备份 |
//! | GET  | `/api/admin/backup/records`     | 备份记录列表 |
//! | DELETE | `/api/admin/backup/records/{id}` | 删除一条备份 |
//! | GET  | `/api/admin/backup/status`      | 当前状态（是否在跑） |

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{AppState, Identity};
use crate::error::{admin_ok, admin_msg, api_err, ok, AppError, AppResult, CODE_OP_FAILED};
use crate::services::backup;

/// 备份路由。
pub fn router() -> axum::Router<Arc<AppState>> {
    use axum::routing::{delete, get, post};
    axum::Router::new()
        .route(
            "/api/admin/backup/config",
            get(get_config).post(save_config),
        )
        .route("/api/admin/backup/test", post(test_connection))
        .route("/api/admin/backup/run", post(run_backup))
        .route("/api/admin/backup/records", get(list_records))
        .route(
            "/api/admin/backup/records/{id}",
            delete(delete_record),
        )
        .route("/api/admin/backup/status", get(get_status))
        .route("/api/admin/backup/verify", post(verify_record))
}

fn ensure_admin(identity: &Identity) -> AppResult<()> {
    if identity.is_admin {
        Ok(())
    } else {
        Err(AppError::unauthorized(
            "需要管理员口令（X-Admin-Password 或后台登录）",
        ))
    }
}

async fn get_config(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = backup::load_config(&st.pool).await?;
    let state = backup::load_state();
    Ok(admin_ok(json!({
        "config": cfg.masked(),
        "last_run_at": state.last_run_at,
        "last_run_status": state.last_run_status,
        "last_run_error": state.last_run_error,
        "running": state.running,
        "record_count": state.records.len(),
    })))
}

async fn save_config(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(patch): Json<Value>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = backup::save_config(&st.pool, &patch).await?;
    Ok(admin_ok(cfg.masked()))
}

async fn test_connection(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = backup::load_config(&st.pool).await?;
    match backup::test_connection(&cfg).await {
        Ok(msg) => Ok(ok(json!({"ok": true, "message": msg}))),
        Err(e) => Ok(api_err(CODE_OP_FAILED, format!("{e}"))),
    }
}

async fn run_backup(
    State(st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = backup::load_config(&st.pool).await?;
    if !cfg.is_configured() {
        return Ok(api_err(CODE_OP_FAILED, "S3 未配置完整"));
    }

    // 同步执行。备份本身只要几百毫秒（8MB 库），不需要后台任务。
    // 大库（GB 级）才需要改成异步 + 轮询，届时参考 batch.rs 的模式。
    match backup::run_backup(&st.pool, &cfg).await {
        Ok(rec) => Ok(ok(json!({
            "id": rec.id,
            "key": rec.key,
            "filename": rec.filename,
            "raw_size": rec.raw_size,
            "gz_size": rec.gz_size,
            "upload_size": rec.upload_size,
            "encrypted": rec.encrypted,
            "sha256": rec.sha256,
            "duration_ms": rec.duration_ms,
            "status": rec.status,
        }))),
        Err(e) => Ok(api_err(CODE_OP_FAILED, format!("备份失败: {e}"))),
    }
}

#[derive(Debug, Deserialize)]
struct RecordsQuery {
    #[serde(default)]
    limit: Option<usize>,
}

async fn list_records(
    State(_st): State<Arc<AppState>>,
    identity: Identity,
    axum::extract::Query(q): axum::extract::Query<RecordsQuery>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let state = backup::load_state();
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    // 最新的在前
    let mut records: Vec<_> = state.records.clone();
    records.reverse();
    records.truncate(limit);
    Ok(admin_ok(json!({
        "total": state.records.len(),
        "items": records,
    })))
}

async fn delete_record(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = backup::load_config(&st.pool).await?;
    backup::delete_backup(&cfg, &id).await?;
    Ok(admin_msg("已删除"))
}

async fn get_status(
    State(_st): State<Arc<AppState>>,
    identity: Identity,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let state = backup::load_state();
    Ok(admin_ok(json!({
        "running": state.running,
        "last_run_at": state.last_run_at,
        "last_run_status": state.last_run_status,
        "last_run_error": state.last_run_error,
        "record_count": state.records.len(),
        "latest": state.records.last(),
    })))
}

/// `POST /api/admin/backup/verify` — 下载并校验一个备份的完整性。
///
/// 备份最容易出的问题是「上传成功但内容坏了」，必须能读回来验证。
#[derive(Debug, Deserialize)]
struct VerifyBody {
    id: String,
}

async fn verify_record(
    State(st): State<Arc<AppState>>,
    identity: Identity,
    Json(body): Json<VerifyBody>,
) -> AppResult<Json<Value>> {
    ensure_admin(&identity)?;
    let cfg = backup::load_config(&st.pool).await?;
    let state = backup::load_state();
    let Some(rec) = state.records.iter().find(|r| r.id == body.id) else {
        return Ok(api_err(CODE_OP_FAILED, "备份记录不存在"));
    };

    let key = rec.key.clone();
    match backup::verify_backup(&cfg, &key).await {
        Ok((size, tables, integ)) => Ok(ok(json!({
            "id": body.id,
            "key": key,
            "decompressed_size": size,
            "table_count": tables,
            "integrity_check": integ,
            "ok": integ == "ok",
        }))),
        Err(e) => Ok(api_err(CODE_OP_FAILED, format!("校验失败: {e}"))),
    }
}