//! 定时任务：对齐 Python 版 `main.py` 的 APScheduler 五个 job。
//!
//! | 任务 | 频率 | 作用 |
//! |---|---|---|
//! | 心跳保活 | 每 5 分钟检查一次，落在配置窗口内才跑 | 模拟真人看信，防微软回收 |
//! | 健康扫描 | 每 5 分钟检查一次 | 找出 token 失效/被风控的号 |
//! | 定时全量刷新 | 每 5 分钟检查一次 | 提前刷新 close-to-expiry 的 rt |
//! | 超时释放 | 每分钟 | 解冻超时的 pending 订单（别把用户的钱锁死） |
//! | 超时占用回收 | 每分钟 | using 超租约回池（池子闲下来时靠它） |
//!
//! ## 为什么每个 job 都要自己判断窗口
//!
//! APScheduler 里配的是 `CronTrigger(minute="*/5")`，真正执行时在 job 内部
//! 检查「现在是否落在配置的 hour:minute 起 5 分钟内」。这样改配置不需要重建
//! 调度器，也避免「配置了 9:07 但调度器只在整 5 分触发」导致永不执行。
//!
//! 这个坑 Python 版踩过：原实现 `now.minute < minute` 在 `minute > 0` 时永远为假。

use std::sync::Arc;
use std::time::Duration;

use crate::auth::AppState;
use crate::services::{billing, pool, settings};

/// 配置窗口宽度（分钟）
const WINDOW_MINUTES: u32 = 5;

/// 判断「现在」是否落在 `hour:minute` 起的窗口内。
fn in_window(now: chrono::DateTime<chrono::Local>, hour: u32, minute: u32) -> bool {
    now.hour() == hour && minute <= now.minute() && now.minute() < minute + WINDOW_MINUTES
}

use chrono::Timelike;

/// 启动全部定时任务。
pub async fn start(state: Arc<AppState>) -> anyhow::Result<()> {
    // ── 每 5 分钟：配置窗口内的三个任务 ──
    let s1 = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(5 * 60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let now = chrono::Local::now();
            if let Err(e) = run_heartbeat(&s1, now).await {
                tracing::error!("心跳保活任务异常: {e}");
            }
            if let Err(e) = run_health_check(&s1, now).await {
                tracing::error!("健康扫描任务异常: {e}");
            }
            if let Err(e) = run_refresh_tokens(&s1, now).await {
                tracing::error!("定时刷新任务异常: {e}");
            }
        }
    });

    // ── 每分钟：超时释放 + 占用回收 ──
    let s2 = state.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 第一 tick 立刻返回，跳过（避免启动瞬间就跑）
        ticker.tick().await;
        loop {
            ticker.tick().await;
            if let Err(e) = run_release_expired(&s2).await {
                tracing::error!("超时释放任务异常: {e}");
            }
            if let Err(e) = run_reclaim_claims(&s2).await {
                tracing::error!("超时占用回收异常: {e}");
            }
        }
    });

    tracing::info!(
        "调度器已启动（心跳保活 + 健康扫描 + 定时刷新 + 超时释放 + 超时占用回收，每 5 分钟检查配置）"
    );
    Ok(())
}

/// 心跳保活：模拟真人看信。
async fn run_heartbeat(state: &Arc<AppState>, now: chrono::DateTime<chrono::Local>) -> anyhow::Result<()> {
    let cfg = settings::get_heartbeat_config(&state.pool).await?;
    if cfg.heartbeat_enabled != "true" {
        return Ok(());
    }
    let hour: u32 = cfg.heartbeat_hour.parse().unwrap_or(9);
    let minute: u32 = cfg.heartbeat_minute.parse().unwrap_or(0);
    if !in_window(now, hour, minute) {
        return Ok(());
    }

    tracing::info!("心跳保活启动: mode={}", cfg.heartbeat_mode);
    let result = pool::heartbeat(&state.pool, &state.http, &cfg.heartbeat_mode, 300).await?;
    settings::set_last_run(
        &state.pool,
        "heartbeat_last_run",
        "heartbeat_last_summary",
        &serde_json::json!({
            "total": result.total,
            "success": result.success,
            "failed": result.failed,
        }),
    )
    .await?;
    tracing::info!(
        "心跳保活完成: 成功 {} / 失败 {} / 总计 {}",
        result.success,
        result.failed,
        result.total
    );
    Ok(())
}

/// 健康扫描：找出 token 失效 / 被风控的号。
async fn run_health_check(state: &Arc<AppState>, now: chrono::DateTime<chrono::Local>) -> anyhow::Result<()> {
    let cfg = settings::get_health_check_config(&state.pool).await?;
    if cfg.health_check_enabled != "true" {
        return Ok(());
    }
    let hour: u32 = cfg.health_check_hour.parse().unwrap_or(8);
    let minute: u32 = cfg.health_check_minute.parse().unwrap_or(0);
    if !in_window(now, hour, minute) {
        return Ok(());
    }

    tracing::info!("健康扫描启动");
    let result = pool::check_accounts_health(&state.pool, &state.http, "outlook", 300, false).await?;
    settings::set_last_run(
        &state.pool,
        "health_check_last_run",
        "health_check_last_summary",
        &serde_json::json!({
            "total": result.total,
            "checked": result.checked,
            "healthy": result.healthy,
            "unhealthy": result.unhealthy,
        }),
    )
    .await?;
    tracing::info!(
        "健康扫描完成: 检测 {} / 正常 {} / 异常 {}",
        result.checked,
        result.healthy,
        result.unhealthy
    );
    Ok(())
}

/// 定时全量刷新 token。
async fn run_refresh_tokens(state: &Arc<AppState>, now: chrono::DateTime<chrono::Local>) -> anyhow::Result<()> {
    let cfg = settings::get_refresh_config(&state.pool).await?;
    if cfg.refresh_enabled != "true" {
        return Ok(());
    }
    let hour: u32 = cfg.refresh_hour.parse().unwrap_or(3);
    let minute: u32 = cfg.refresh_minute.parse().unwrap_or(0);
    if !in_window(now, hour, minute) {
        return Ok(());
    }

    tracing::info!("定时全量刷新 Token 启动");
    let result = pool::refresh_all_tokens(&state.pool, &state.http, "outlook").await?;
    settings::set_last_run(
        &state.pool,
        "refresh_last_run",
        "refresh_last_summary",
        &serde_json::json!({
            "total": result.total,
            "success": result.success,
            "failed": result.failed,
            "duration": result.duration_secs,
            "trigger": "schedule",
        }),
    )
    .await?;
    tracing::info!("定时刷新完成: 成功 {} / 失败 {}", result.success, result.failed);
    Ok(())
}

/// 超时释放：解冻超时的 pending 订单（不扣费）。
async fn run_release_expired(state: &Arc<AppState>) -> anyhow::Result<()> {
    let cfg = settings::get_billing_config(&state.pool).await?;
    if !cfg.is_enabled() {
        return Ok(());
    }
    let count = billing::release_expired(&state.pool, cfg.hold_timeout_secs(), 500).await?;
    if count > 0 {
        tracing::info!("超时订单自动释放: {count} 单（解冻不扣费）");
    }
    Ok(())
}

/// 超时占用回收：using 超租约回池。
///
/// 为什么要有独立任务：`claim_account` 里也会顺带调 recovery，但那只在
/// 「有新取号」时触发。池子一没人取号，挂住的 using 就一直占着库存 ——
/// 实测 49 个 using 里 38 个早该回收。
async fn run_reclaim_claims(state: &Arc<AppState>) -> anyhow::Result<()> {
    let n = pool::recover_abandoned_claims(&state.pool).await?;
    if n > 0 {
        tracing::info!(
            "超时占用回收: {n} 个号回池（租约 {} 分钟）",
            pool::CLAIM_LEASE_TIMEOUT_MINUTES
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32) -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 11, 22, h, m, 0)
            .unwrap()
    }

    #[test]
    fn window_matches_configured_minute() {
        // 配置 9:00，窗口 5 分钟
        assert!(in_window(at(9, 0), 9, 0));
        assert!(in_window(at(9, 4), 9, 0));
        assert!(!in_window(at(9, 5), 9, 0));
        assert!(!in_window(at(8, 59), 9, 0));
        assert!(!in_window(at(10, 0), 9, 0));
    }

    #[test]
    fn window_with_nonzero_minute() {
        // 回归：Python 版 `now.minute < minute` 在 minute>0 时永远为假
        assert!(in_window(at(9, 7), 9, 7), "配置 9:07 时 9:07 必须命中");
        assert!(in_window(at(9, 9), 9, 7));
        assert!(!in_window(at(9, 6), 9, 7));
        assert!(!in_window(at(9, 12), 9, 7));
    }

    #[test]
    fn window_at_day_end() {
        // 23:58 配置 + 5 分钟窗口会跨天，这里截到 60
        assert!(in_window(at(23, 59), 23, 58));
        assert!(!in_window(at(0, 1), 23, 58));
    }
}
