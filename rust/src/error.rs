//! 错误类型与统一响应。
//!
//! 对齐 Python 版的两套约定：
//! - **对外 API**（`/api/v1/*`、`/api/product/*`）：`{"code":"00000","msg":"","type":"","data":...}`
//! - **管理 API**（`/api/admin/*`）：`{"code":0,"data":...}` / `{"code":0,"msg":"..."}`

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

// ── 对外 API 业务码（对齐 MailNest）──────────────────────────
pub const CODE_OK: &str = "00000";
pub const CODE_BALANCE: &str = "D0001";
pub const CODE_NO_STOCK: &str = "D0002";
pub const CODE_NO_PROJECT: &str = "D0003";
pub const CODE_OP_FAILED: &str = "D0004";
pub const CODE_FETCH_FAILED: &str = "D0005";
pub const CODE_BANNED: &str = "D0006";
pub const CODE_PAYMENT: &str = "D0007";
pub const CODE_SYSTEM: &str = "99999";

/// 对外 API 成功响应体。
pub fn ok(data: serde_json::Value) -> Json<serde_json::Value> {
    Json(json!({"code": CODE_OK, "msg": "", "type": "", "data": data}))
}

/// 对外 API 业务错误响应体（HTTP 200，错误在 code 里）。
pub fn api_err(code: &str, msg: impl Into<String>) -> Json<serde_json::Value> {
    Json(json!({"code": code, "msg": msg.into(), "type": "error", "data": null}))
}

/// 管理 API 成功响应体（`code: 0`）。
pub fn admin_ok(data: serde_json::Value) -> Json<serde_json::Value> {
    Json(json!({"code": 0, "data": data}))
}

/// 管理 API 带 msg 的成功响应体。
pub fn admin_msg(msg: impl Into<String>) -> Json<serde_json::Value> {
    Json(json!({"code": 0, "msg": msg.into()}))
}

/// 应用错误。`IntoResponse` 输出与 Python 版一致的 HTTP 状态 + JSON detail。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Unauthorized(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Conflict(String),

    #[error("{0}")]
    BadGateway(String),

    #[error("{0}")]
    Internal(String),

    #[error(transparent)]
    Db(#[from] sqlx::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("余额不足（需要 {need}，可用 {available}）")]
    InsufficientBalance { need: String, available: String },
}

impl AppError {
    pub fn bad_request(m: impl Into<String>) -> Self {
        AppError::BadRequest(m.into())
    }
    pub fn unauthorized(m: impl Into<String>) -> Self {
        AppError::Unauthorized(m.into())
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        AppError::Forbidden(m.into())
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        AppError::NotFound(m.into())
    }
    pub fn internal(m: impl Into<String>) -> Self {
        AppError::Internal(m.into())
    }
    pub fn bad_gateway(m: impl Into<String>) -> Self {
        AppError::BadGateway(m.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::BadGateway(_) => StatusCode::BAD_GATEWAY,
            AppError::InsufficientBalance { .. } => StatusCode::BAD_REQUEST,
            AppError::Db(_) | AppError::Json(_) | AppError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let detail = match &self {
            // 数据库/JSON 内部错误不把原文抛给前端（可能含 SQL 与路径）
            AppError::Db(e) => {
                tracing::error!("数据库错误: {e}");
                "系统异常".to_string()
            }
            AppError::Json(e) => {
                tracing::error!("JSON 错误: {e}");
                "系统异常".to_string()
            }
            other => other.to_string(),
        };
        (status, Json(json!({"detail": detail}))).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn business_codes_match_python() {
        // 这些码是与客户端契约的一部分，改动即为破坏性变更
        assert_eq!(CODE_BALANCE, "D0001");
        assert_eq!(CODE_NO_STOCK, "D0002");
        assert_eq!(CODE_NO_PROJECT, "D0003");
        assert_eq!(CODE_OP_FAILED, "D0004");
        assert_eq!(CODE_FETCH_FAILED, "D0005");
        assert_eq!(CODE_BANNED, "D0006");
        assert_eq!(CODE_PAYMENT, "D0007");
        assert_eq!(CODE_SYSTEM, "99999");
    }

    #[test]
    fn status_mapping() {
        assert_eq!(
            AppError::unauthorized("x").status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(AppError::not_found("x").status(), StatusCode::NOT_FOUND);
    }
}
