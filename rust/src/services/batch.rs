//! 后台批量任务：全量刷新 Token。
//!
//! ## 为什么必须是后台任务
//!
//! 995 个号串行刷新约 40 分钟（每个 2-3 秒，含限流排队）。同步跑会：
//! 1. HTTP 请求超时（浏览器 60s、Nginx 300s 都撑不住）
//! 2. 占死一个 worker
//!
//! 所以对齐 Python 版：**起后台任务 + 返回 task_id，前端轮询进度**。
//!
//! ## 任务状态为什么放内存
//!
//! 与 Python 版一致 —— 任务是一次性的运维操作，进程重启后"任务不存在"
//! 是可接受的（重启本来就中断了刷新）。放数据库要多一张表和清理逻辑，
//! 收益为零。
//!
//! ## 互斥
//!
//! 同一时刻只允许一个刷新任务在跑（对齐 Python：running 状态直接拒绝）。
//! 原因是并发刷新会对同一个 refresh_token 做轮换，微软那边会判定为
//! 重放攻击 → `invalid_grant` → 整批号废掉。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::auth::AppState;
use crate::error::{AppError, AppResult};
use crate::models::EmailAccount;

/// 单个账号的刷新结果。
#[derive(Debug, Clone)]
struct TaskResult {
    email: String,
    status: &'static str, // success / failed / skipped
    message: String,
}

/// 一个批量刷新任务。
#[derive(Debug, Clone)]
struct BatchTask {
    task_id: String,
    status: &'static str, // running / finished / failed
    total: i64,
    done: i64,
    success: i64,
    failed: i64,
    skipped: i64,
    current_email: String,
    results: Vec<TaskResult>,
    trigger: String,
    provider: String,
    started_at: String,
    finished_at: String,
    error: String,
}

/// 全局任务表（进程内）。
static TASKS: std::sync::OnceLock<Mutex<HashMap<String, BatchTask>>> =
    std::sync::OnceLock::new();

fn tasks() -> &'static Mutex<HashMap<String, BatchTask>> {
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 启动全量刷新任务。返回 `(task_id, total)`。
///
/// 已有 running 任务时直接拒绝 —— 这是刻意的：并发刷新会让微软判定
/// refresh_token 重放，整批号会变成 `invalid_grant`。
pub async fn start_refresh_all(
    state: Arc<AppState>,
    provider: &str,
    trigger: &str,
) -> AppResult<Value> {
    // 互斥检查
    {
        let map = tasks().lock().unwrap();
        if let Some(running) = map.values().find(|t| t.status == "running") {
            return Err(AppError::bad_request(format!(
                "已有刷新任务在运行（{}，进度 {}/{}），请等它完成",
                running.task_id, running.done, running.total
            )));
        }
    }

    // 先数一遍目标（只统计有 refresh_token 的号）
    let candidates = sqlx::query_as::<_, EmailAccount>(
        "SELECT * FROM email_accounts WHERE provider = ? AND health_status <> 'dead' \
         ORDER BY id ASC",
    )
    .bind(provider)
    .fetch_all(&state.pool)
    .await?;

    let total = candidates
        .iter()
        .filter(|a| {
            crate::providers::outlook::Credentials::from_json(&a.account_json)
                .map(|c| !c.refresh_token.trim().is_empty())
                .unwrap_or(false)
        })
        .count() as i64;

    let task_id = format!("bt{}", crate::util::utc_now().timestamp());
    let task = BatchTask {
        task_id: task_id.clone(),
        status: "running",
        total,
        done: 0,
        success: 0,
        failed: 0,
        skipped: 0,
        current_email: String::new(),
        results: Vec::new(),
        trigger: trigger.to_string(),
        provider: provider.to_string(),
        started_at: crate::util::utc_now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false),
        finished_at: String::new(),
        error: String::new(),
    };
    tasks().lock().unwrap().insert(task_id.clone(), task);

    // 后台跑
    let tid = task_id.clone();
    let prov = provider.to_string();
    tokio::spawn(async move {
        if let Err(e) = run_refresh_task(state, tid.clone(), prov).await {
            tracing::error!("[batch-refresh] 任务 {tid} 异常: {e}");
            let mut map = tasks().lock().unwrap();
            if let Some(t) = map.get_mut(&tid) {
                t.status = "failed";
                t.error = e.to_string();
                t.finished_at =
                    crate::util::utc_now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false);
            }
        }
    });

    Ok(json!({
        "task_id": task_id,
        "running": true,
        "total": total,
        "message": format!("全量刷新已启动，共 {total} 个账号"),
    }))
}

/// 后台任务主体：逐个刷新。
async fn run_refresh_task(
    state: Arc<AppState>,
    task_id: String,
    provider: String,
) -> AppResult<()> {
    let accounts = sqlx::query_as::<_, EmailAccount>(
        "SELECT * FROM email_accounts WHERE provider = ? AND health_status <> 'dead' \
         ORDER BY id ASC",
    )
    .bind(&provider)
    .fetch_all(&state.pool)
    .await?;

    for acc in accounts {
        // 标记当前处理到谁（前端显示"正在刷新 xxx@outlook.com"）
        {
            let mut map = tasks().lock().unwrap();
            if let Some(t) = map.get_mut(&task_id) {
                t.current_email = acc.email.clone();
            }
        }

        let mut cred = match crate::providers::outlook::Credentials::from_json(&acc.account_json) {
            Ok(c) => c,
            Err(_) => {
                mark(&task_id, &acc.email, "skipped", "凭据 JSON 损坏");
                continue;
            }
        };
        if cred.refresh_token.trim().is_empty() || cred.client_id.trim().is_empty() {
            mark(&task_id, &acc.email, "skipped", "缺少 refresh_token / client_id");
            continue;
        }
        cred.email = acc.email.clone();
        if !acc.proxy.trim().is_empty() {
            cred.proxy = acc.proxy.clone();
        }

        match crate::providers::outlook::refresh_token(
            &state.http,
            &mut cred,
            crate::providers::outlook::GRAPH_SCOPE,
        )
        .await
        {
            Ok(_) => {
                // 轮换出的新 rt 必须落库，否则下次刷新用旧 rt 会失效
                let _ = sqlx::query(
                    "UPDATE email_accounts SET account_json = ?, health_status = 'healthy', \
                     health_reason = '', last_check_at = ? WHERE id = ?",
                )
                .bind(cred.to_json())
                .bind(crate::util::now_naive())
                .bind(acc.id)
                .execute(&state.pool)
                .await;
                mark(&task_id, &acc.email, "success", "");
            }
            Err(e) => {
                let msg = e.to_string();
                // 凭据失效 → 直接标死号（刷新也救不回来）
                if crate::providers::outlook::is_credential_fatal(&msg) {
                    let _ = sqlx::query(
                        "UPDATE email_accounts SET health_status = 'dead', health_reason = ?, \
                         pool_status = 'retired' WHERE id = ?",
                    )
                    .bind(crate::util::truncate_chars(&msg, 120))
                    .bind(acc.id)
                    .execute(&state.pool)
                    .await;
                }
                mark(
                    &task_id,
                    &acc.email,
                    "failed",
                    &crate::util::truncate_chars(&msg, 120),
                );
            }
        }

        // 账号间间隔 3 秒：避免触发微软 client_id 级限流
        // （与 Python 版 BATCH_REFRESH_INTERVAL 一致）
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }

    let mut map = tasks().lock().unwrap();
    if let Some(t) = map.get_mut(&task_id) {
        t.status = "finished";
        t.current_email.clear();
        t.finished_at = crate::util::utc_now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false);
    }
    if let Some(t) = map.get(&task_id) {
        tracing::info!(
            "[batch-refresh] 完成 task={} 成功 {} / 失败 {} / 跳过 {}",
            task_id,
            t.success,
            t.failed,
            t.skipped
        );
    }
    Ok(())
}

/// 记一条结果并推进计数。
fn mark(task_id: &str, email: &str, status: &'static str, message: &str) {
    let mut map = tasks().lock().unwrap();
    let Some(t) = map.get_mut(task_id) else {
        return;
    };
    t.done += 1;
    match status {
        "success" => t.success += 1,
        "failed" => t.failed += 1,
        _ => t.skipped += 1,
    }
    // 结果列表上限 500 条：995 个号全存会让前端拉一次 100KB+，
    // 而页面只看最近失败的那几条
    if t.results.len() < 500 {
        t.results.push(TaskResult {
            email: email.to_string(),
            status,
            message: message.to_string(),
        });
    }
}

/// 查任务进度。任务不存在返回 `{"status":"not_found"}`（对齐 Python）。
pub fn get_status(task_id: &str) -> Value {
    let map = tasks().lock().unwrap();
    let Some(t) = map.get(task_id) else {
        return json!({"status": "not_found", "message": "任务不存在"});
    };
    json!({
        "task_id": t.task_id,
        "status": t.status,
        "total": t.total,
        "done": t.done,
        "success": t.success,
        "failed": t.failed,
        "skipped": t.skipped,
        "current_email": t.current_email,
        "results": t.results.iter().map(|r| json!({
            "email": r.email,
            "status": r.status,
            "message": r.message,
        })).collect::<Vec<_>>(),
        "trigger": t.trigger,
        "provider": t.provider,
        "started_at": t.started_at,
        "finished_at": t.finished_at,
        "error": t.error,
    })
}

/// 列出所有任务（按启动时间倒序，最多 20 个）。
pub fn list_tasks() -> Vec<Value> {
    let map = tasks().lock().unwrap();
    let mut all: Vec<&BatchTask> = map.values().collect();
    all.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    all.into_iter()
        .take(20)
        .map(|t| {
            json!({
                "task_id": t.task_id,
                "status": t.status,
                "total": t.total,
                "done": t.done,
                "success": t.success,
                "failed": t.failed,
                "skipped": t.skipped,
                "trigger": t.trigger,
                "started_at": t.started_at,
                "finished_at": t.finished_at,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_updates_counters() {
        let tid = "test-task-1";
        {
            let mut map = tasks().lock().unwrap();
            map.insert(
                tid.into(),
                BatchTask {
                    task_id: tid.into(),
                    status: "running",
                    total: 3,
                    done: 0,
                    success: 0,
                    failed: 0,
                    skipped: 0,
                    current_email: String::new(),
                    results: Vec::new(),
                    trigger: "manual".into(),
                    provider: "outlook".into(),
                    started_at: String::new(),
                    finished_at: String::new(),
                    error: String::new(),
                },
            );
        }
        mark(tid, "a@b.test", "success", "");
        mark(tid, "c@d.test", "failed", "boom");
        mark(tid, "e@f.test", "skipped", "no rt");

        let v = get_status(tid);
        assert_eq!(v["done"], 3);
        assert_eq!(v["success"], 1);
        assert_eq!(v["failed"], 1);
        assert_eq!(v["skipped"], 1);
        assert_eq!(v["results"].as_array().unwrap().len(), 3);

        tasks().lock().unwrap().remove(tid);
    }

    #[test]
    fn unknown_task_returns_not_found() {
        let v = get_status("no-such-task");
        assert_eq!(v["status"], "not_found");
    }

    #[test]
    fn results_capped_at_500() {
        let tid = "test-task-cap";
        {
            let mut map = tasks().lock().unwrap();
            map.insert(
                tid.into(),
                BatchTask {
                    task_id: tid.into(),
                    status: "running",
                    total: 1000,
                    done: 0,
                    success: 0,
                    failed: 0,
                    skipped: 0,
                    current_email: String::new(),
                    results: Vec::new(),
                    trigger: "manual".into(),
                    provider: "outlook".into(),
                    started_at: String::new(),
                    finished_at: String::new(),
                    error: String::new(),
                },
            );
        }
        for i in 0..600 {
            mark(tid, &format!("u{i}@t.test"), "success", "");
        }
        let v = get_status(tid);
        assert_eq!(v["done"], 600, "计数要准确");
        assert_eq!(
            v["results"].as_array().unwrap().len(),
            500,
            "结果列表要截断（含 995 条会让前端拉爆）"
        );
        tasks().lock().unwrap().remove(tid);
    }
}
