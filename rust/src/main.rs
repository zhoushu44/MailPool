//! MailPool — Outlook 邮箱池 / 取号收码服务（Rust 版）。
//!
//! 单二进制、单进程、单 SQLite 文件。启动流程：
//! ```text
//! 读配置 → 建/迁移库 → 预编译模板 → 起 HTTP → 起定时任务
//! ```
//!
//! **与 Python 版共存**：共用同一个 `mailpool.db` 和同一个 `.session_secret`，
//! 会话 cookie 与密码哈希双向兼容，因此可以并行跑、按路径分流灰度。

#![recursion_limit = "512"]

mod api;
mod auth;
mod config;
mod db;
mod error;
mod models;
mod money;
mod providers;
mod services;
#[cfg(test)]
mod testing;
mod util;
mod web;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::Router;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

use crate::auth::AppState;
use crate::config::Settings;

/// 应用装配（测试也复用这个入口）。
pub async fn build_app(state: Arc<AppState>) -> Router {
    let static_dir = Settings::get().static_dir.clone();

    // ── 对外 API（/api/v1、/api/product）──
    let public_api = Router::new()
        .route("/api/product/info", get(api::public::product_info))
        .route(
            "/api/v1/email/temporary/buy",
            axum::routing::post(api::public::buy_email),
        )
        .route(
            "/api/v1/email/receive",
            axum::routing::post(api::public::receive_code),
        )
        .route(
            "/api/v1/email/release",
            axum::routing::post(api::public::release_email),
        )
        .route("/api/v1/mails", get(api::public::list_mails))
        .route("/api/v1/health", get(api::public::health))
        .route("/api", get(api::public::api_index));

    Router::new()
        .merge(public_api)
        .merge(api::recharge::router())
        .merge(api::auth::router())
        .merge(api::app_mailbox::router())
        .merge(api::exclusive::router())
        .merge(api::backup::router())
        .merge(api::admin::router())
        .merge(web::pages::router(state.templates.clone()))
        // 静态资源
        .nest_service("/static", ServeDir::new(static_dir))
        .layer(
            ServiceBuilder::new()
                .layer(CompressionLayer::new())
                .layer(TraceLayer::new_for_http()),
        )
        // 导入账号时可能提交很长的文本（千行 CSV），放宽 body 上限
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 健康检查模式（Dockerfile 的 HEALTHCHECK 用，避免镜像里装 curl）
    if std::env::args().any(|a| a == "--healthcheck") {
        let port: u16 = Settings::get().port;
        let url = format!("http://127.0.0.1:{port}/login");
        return match reqwest::get(&url).await {
            Ok(r) if r.status().is_success() => Ok(()),
            Ok(r) => {
                eprintln!("healthcheck 失败: HTTP {}", r.status());
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("healthcheck 失败: {e}");
                std::process::exit(1);
            }
        };
    }

    // 可选 .env（部署时以环境变量为主，本地开发方便）
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mailpool=info,tower_http=warn,sqlx=warn".into()),
        )
        .init();

    let settings = Settings::get();

    // 1) 数据库
    let pool = db::connect(&settings.db_path).await?;
    db::init_db(&pool).await?;
    tracing::info!("数据库就绪: {}", settings.db_path.display());

    // 2) 模板（启动期预编译，不兼容当场失败）
    let templates = web::templates::Templates::load(&settings.template_dir)
        .map_err(|e| anyhow::anyhow!("模板加载失败: {e}"))?;

    // 3) 状态
    let state = Arc::new(AppState {
        pool: pool.clone(),
        http: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?,
        stats: Arc::new(auth::Stats::default()),
        templates: Arc::new(templates),
    });

    // 4) 定时任务
    web::scheduler::start(state.clone()).await?;

    // 5) HTTP
    let app = build_app(state).await;
    let addr: SocketAddr = format!("{}:{}", settings.host, settings.port).parse()?;
    tracing::info!("MailPool 监听 http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}
