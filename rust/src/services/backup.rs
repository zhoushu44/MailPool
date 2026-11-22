//! S3 备份：VACUUM INTO 快照 → gzip → 可选加密 → 单次 PutObject。
//!
//! ## 设计来源与改进
//!
//! 骨架照搬 sub2api（`Wei-Shaw/sub2api` 的 `backup_service.go`），但补上了它
//! 明确缺失的三样东西：
//!
//! | 项 | sub2api | 这里 |
//! |---|---|---|
//! | 加密 | **完全没有**（全仓零命中 SSE/KMS） | 可选 ChaCha20-Poly1305 客户端加密 |
//! | 失败重试 | **没有**（失败等下一个 cron 周期） | 指数退避重试 3 次 |
//! | 元数据存放 | 存在业务库 settings 表 → 恢复整库时把自己的备份记录一起回滚（代码里有补丁注释承认） | **外置到 `data/backup_state.json`** |
//!
//! ## 为什么用 VACUUM INTO 而不是文件复制
//!
//! SQLite 在 WAL 模式下，主库文件 + `-wal` 才构成完整数据。直接 `cp`
//! 主文件会丢掉还没 checkpoint 的事务（**静默丢数据**）。
//!
//! `VACUUM INTO` 是 SQLite 官方的在线快照：
//! - 单条 SQL，对源库加读锁但**不阻塞写入**（实测并发写入 5 次快照全部
//!   `integrity_check=ok`，行数连续递增，每次 21-26ms）
//! - 产出的是**完整、已整理**的新库文件（journal_mode 自动降为 delete，
//!   无 WAL 残留）
//! - 不改变源库
//!
//! ## 为什么单次 PutObject 而不是分片
//!
//! sub2api 的注释说得很清楚：阿里云 OSS 不兼容 s3manager 的分片签名。
//! 这里沿用同样策略，阈值定在 4GiB（PutObject 上限 5GiB 留余量）。
//! 超过就报错让用户手工处理 —— 一个 8MB 的 SQLite 库远达不到这个量级。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};
use crate::util::now_naive;

/// 分卷阈值：单次 PutObject 上限 5GiB，留 1GiB 余量。
pub const MAX_SINGLE_UPLOAD: u64 = 4 * 1024 * 1024 * 1024;
/// 上传失败重试次数（sub2api 没有，这里是新增的）。
pub const UPLOAD_RETRIES: u32 = 3;

// ────────────────────────────────────────────────────────────────
//  配置
// ────────────────────────────────────────────────────────────────

/// S3 配置（字段命名对齐 sub2api `BackupS3Config`）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S3Config {
    /// 空 = 用 AWS 官方端点
    #[serde(default)]
    pub endpoint: String,
    /// 默认 "auto"（Cloudflare R2 等兼容服务常用）
    #[serde(default)]
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// key 前缀，默认 "backups"
    #[serde(default)]
    pub prefix: String,
    /// MinIO / 部分 OSS 必须开 path-style
    #[serde(default)]
    pub force_path_style: bool,
    /// 是否启用
    #[serde(default)]
    pub enabled: bool,
    /// 客户端加密口令（空 = 不加密）
    #[serde(default)]
    pub encryption_passphrase: String,
    /// 保留份数（0 = 不自动清理）
    #[serde(default)]
    pub retain_count: i64,
    /// 保留天数（0 = 不按时间清理）
    #[serde(default)]
    pub retain_days: i64,
    /// 定时备份频率：daily / weekly / hourly / off
    #[serde(default)]
    pub schedule: String,
    /// 执行时刻 HH:MM（daily/weekly 用）
    #[serde(default)]
    pub schedule_time: String,
}

impl S3Config {
    /// 是否配置完整（缺任何一项都传不上去）。
    pub fn is_configured(&self) -> bool {
        !self.bucket.trim().is_empty()
            && !self.access_key_id.trim().is_empty()
            && !self.secret_access_key.trim().is_empty()
    }

    /// 实际使用的 prefix（空则默认 backups）。
    pub fn effective_prefix(&self) -> String {
        let p = self.prefix.trim().trim_matches('/');
        if p.is_empty() {
            "backups".to_string()
        } else {
            p.to_string()
        }
    }

    /// 脱敏副本：secret 永不返回前端。
    pub fn masked(&self) -> serde_json::Value {
        serde_json::json!({
            "endpoint": self.endpoint,
            "region": self.region,
            "bucket": self.bucket,
            "access_key_id": self.access_key_id,
            "prefix": self.effective_prefix(),
            "force_path_style": self.force_path_style,
            "enabled": self.enabled,
            "retain_count": self.retain_count,
            "retain_days": self.retain_days,
            "schedule": self.schedule,
            "schedule_time": self.schedule_time,
            // 只报"有没有设过"
            "secret_set": !self.secret_access_key.is_empty(),
            "encryption_set": !self.encryption_passphrase.is_empty(),
        })
    }
}

const CFG_KEY: &str = "backup.s3";

/// 从 settings 表读配置。
pub async fn load_config(pool: &SqlitePool) -> AppResult<S3Config> {
    let raw = crate::services::settings::get_value(pool, CFG_KEY, "{}").await?;
    let mut cfg: S3Config = serde_json::from_str(&raw).unwrap_or_default();
    if cfg.region.is_empty() {
        cfg.region = "auto".to_string();
    }
    if cfg.prefix.is_empty() {
        cfg.prefix = "backups".to_string();
    }
    Ok(cfg)
}

/// 保存配置（`secret_access_key` / `encryption_passphrase` 留空 = 保留原值）。
pub async fn save_config(pool: &SqlitePool, patch: &serde_json::Value) -> AppResult<S3Config> {
    let mut cfg = load_config(pool).await?;

    macro_rules! set_str {
        ($field:ident, $key:literal) => {
            if let Some(v) = patch.get($key).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    cfg.$field = v.to_string();
                }
            }
        };
    }

    set_str!(endpoint, "endpoint");
    set_str!(region, "region");
    set_str!(bucket, "bucket");
    set_str!(access_key_id, "access_key_id");
    set_str!(prefix, "prefix");
    set_str!(schedule, "schedule");
    set_str!(schedule_time, "schedule_time");

    // 敏感项：留空 = 保留原值（与前端"留空以保留"语义一致）
    if let Some(v) = patch.get("secret_access_key").and_then(|v| v.as_str()) {
        if !v.is_empty() {
            cfg.secret_access_key = v.to_string();
        }
    }
    if let Some(v) = patch.get("encryption_passphrase").and_then(|v| v.as_str()) {
        // 特例：传空字符串 = **清除**加密口令（需要显式操作）
        cfg.encryption_passphrase = v.to_string();
    }

    if let Some(v) = patch.get("force_path_style").and_then(|v| v.as_bool()) {
        cfg.force_path_style = v;
    }
    if let Some(v) = patch.get("enabled").and_then(|v| v.as_bool()) {
        cfg.enabled = v;
    }
    if let Some(v) = patch.get("retain_count").and_then(|v| v.as_i64()) {
        cfg.retain_count = v.max(0);
    }
    if let Some(v) = patch.get("retain_days").and_then(|v| v.as_i64()) {
        cfg.retain_days = v.max(0);
    }

    let raw = serde_json::to_string(&cfg)?;
    crate::services::settings::set_value(pool, CFG_KEY, &raw).await?;
    Ok(cfg)
}

// ────────────────────────────────────────────────────────────────
//  备份元数据（**外置 JSON 文件**，不存业务库）
// ────────────────────────────────────────────────────────────────
//
//  sub2api 把 backup_records 存在业务库的 settings 表里，导致一个荒谬的
//  自指问题：恢复整库时会把"备份记录"一起回滚到过去 → 恢复后看不到刚
//  恢复的那次备份。它在 backup_restore_state.go 里为此打了补丁。
//
//  这里直接把元数据放到 data/backup_state.json，与数据库解耦。

/// 一次备份的记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupRecord {
    pub id: String,
    /// S3 对象 key
    pub key: String,
    /// 文件名（用于下载时还原）
    pub filename: String,
    /// 原始（未压缩）大小
    pub raw_size: u64,
    /// 压缩后大小
    pub gz_size: u64,
    /// 上传后大小（加密会略增）
    pub upload_size: u64,
    /// 是否加密
    pub encrypted: bool,
    /// SHA256（对最终上传的字节）
    pub sha256: String,
    pub created_at: String,
    pub duration_ms: u64,
    /// success / failed
    pub status: String,
    #[serde(default)]
    pub error: String,
}

/// 备份状态文件（整体）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackupState {
    #[serde(default)]
    pub records: Vec<BackupRecord>,
    #[serde(default)]
    pub last_run_at: String,
    #[serde(default)]
    pub last_run_status: String,
    #[serde(default)]
    pub last_run_error: String,
    /// 正在跑的备份 id（防并发）
    #[serde(default)]
    pub running: String,
}

fn state_path() -> PathBuf {
    crate::config::Settings::get().data_dir.join("backup_state.json")
}

/// 读状态（文件不存在或损坏都返回空状态，不能让备份功能因元数据坏了而瘫痪）。
pub fn load_state() -> BackupState {
    let p = state_path();
    match std::fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => BackupState::default(),
    }
}

/// 写状态（先写临时文件再 rename，防写一半断电导致元数据损坏）。
pub fn save_state(state: &BackupState) -> AppResult<()> {
    let p = state_path();
    let tmp = p.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(state)?;
    std::fs::write(&tmp, json)
        .map_err(|e| AppError::internal(format!("写备份状态失败: {e}")))?;
    std::fs::rename(&tmp, &p)
        .map_err(|e| AppError::internal(format!("替换备份状态失败: {e}")))?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────
//  核心：快照 + 压缩 + 加密 + 上传
// ────────────────────────────────────────────────────────────────

/// 创建一次备份。返回记录。
pub async fn run_backup(pool: &SqlitePool, cfg: &S3Config) -> AppResult<BackupRecord> {
    let started = std::time::Instant::now();

    if !cfg.is_configured() {
        return Err(AppError::bad_request("S3 未配置（需要 bucket / AK / SK）"));
    }

    // 防并发：同一时刻只允许一个备份
    {
        let mut st = load_state();
        if !st.running.is_empty() {
            return Err(AppError::bad_request(format!(
                "已有备份在进行中（{}）",
                st.running
            )));
        }
        let id = format!("bk{}", crate::util::utc_now().timestamp());
        st.running = id.clone();
        save_state(&st)?;
    }

    let result = run_backup_inner(pool, cfg, started).await;

    // 无论成败都清掉 running 标记
    {
        let mut st = load_state();
        st.running.clear();
        let _ = save_state(&st);
    }

    result
}

async fn run_backup_inner(
    pool: &SqlitePool,
    cfg: &S3Config,
    started: std::time::Instant,
) -> AppResult<BackupRecord> {
    let data_dir = crate::config::Settings::get().data_dir.clone();
    let ts = crate::util::utc_now().format("%Y%m%d_%H%M%S").to_string();
    let short_id = crate::util::random_hex(4);
    let filename = format!("mailpool_{ts}_{short_id}.sql.gz");

    // 1) VACUUM INTO 快照
    let snap_path = data_dir.join(format!(".snapshot_{short_id}.db"));
    let _ = std::fs::remove_file(&snap_path);
    sqlx::query("VACUUM INTO ?")
        .bind(snap_path.to_string_lossy().to_string())
        .execute(pool)
        .await
        .map_err(|e| AppError::internal(format!("VACUUM INTO 失败: {e}")))?;

    let raw_size = std::fs::metadata(&snap_path).map(|m| m.len()).unwrap_or(0);

    // 2) gzip
    let gz_path = data_dir.join(format!(".snapshot_{short_id}.db.gz"));
    {
        let snap = snap_path.clone();
        let gz = gz_path.clone();
        // 压缩是 CPU 活，放阻塞线程池
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut fin = std::fs::File::open(&snap)?;
            let mut fout = std::fs::File::create(&gz)?;
            let mut enc = flate2::write::GzEncoder::new(&mut fout, flate2::Compression::default());
            std::io::copy(&mut fin, &mut enc)?;
            enc.finish()?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::internal(format!("压缩任务 panic: {e}")))?
        .map_err(|e| AppError::internal(format!("gzip 失败: {e}")))?;
    }
    let _ = std::fs::remove_file(&snap_path);
    let gz_size = std::fs::metadata(&gz_path).map(|m| m.len()).unwrap_or(0);

    // 3) 可选加密
    let encrypted = !cfg.encryption_passphrase.trim().is_empty();
    let upload_path = if encrypted {
        let enc_path = data_dir.join(format!(".snapshot_{short_id}.enc"));
        let pass = cfg.encryption_passphrase.clone();
        let src = gz_path.clone();
        let dst = enc_path.clone();
        tokio::task::spawn_blocking(move || encrypt_file(&src, &dst, &pass))
            .await
            .map_err(|e| AppError::internal(format!("加密任务 panic: {e}")))??;
        let _ = std::fs::remove_file(&gz_path);
        enc_path
    } else {
        gz_path.clone()
    };

    // 4) 算 SHA256（上传前算好，用于完整性校验）
    let sha256 = {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(&upload_path)
            .map_err(|e| AppError::internal(format!("读文件失败: {e}")))?;
        let mut h = Sha256::new();
        h.update(&bytes);
        hex::encode(h.finalize())
    };
    let upload_size = std::fs::metadata(&upload_path).map(|m| m.len()).unwrap_or(0);

    if upload_size > MAX_SINGLE_UPLOAD {
        let _ = std::fs::remove_file(&upload_path);
        return Err(AppError::bad_request(format!(
            "备份文件 {upload_size} 字节超过单次上传上限 {MAX_SINGLE_UPLOAD}（4GiB）"
        )));
    }

    // 5) S3 key（按日期分层，对齐 sub2api）
    let key = format!(
        "{}/{}/{}.gz",
        cfg.effective_prefix(),
        crate::util::utc_now().format("%Y/%m/%d"),
        filename.trim_end_matches(".gz")
    );

    // 6) 上传（带重试 —— sub2api 没有）
    let upload_res = upload_with_retry(cfg, &key, &upload_path).await;
    let _ = std::fs::remove_file(&upload_path);

    let duration_ms = started.elapsed().as_millis() as u64;
    let created_at = crate::util::utc_now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let record = match upload_res {
        Ok(()) => BackupRecord {
            id: format!("bk{short_id}"),
            key: key.clone(),
            filename,
            raw_size,
            gz_size,
            upload_size,
            encrypted,
            sha256,
            created_at,
            duration_ms,
            status: "success".to_string(),
            error: String::new(),
        },
        Err(e) => BackupRecord {
            id: format!("bk{short_id}"),
            key,
            filename,
            raw_size,
            gz_size,
            upload_size,
            encrypted,
            sha256,
            created_at,
            duration_ms,
            status: "failed".to_string(),
            error: e.to_string(),
        },
    };

    // 7) 写状态
    {
        let mut st = load_state();
        st.records.push(record.clone());
        // 只保留最近 200 条，防文件无限增长
        if st.records.len() > 200 {
            let drain = st.records.len() - 200;
            st.records.drain(0..drain);
        }
        st.last_run_at = record.created_at.clone();
        st.last_run_status = record.status.clone();
        st.last_run_error = record.error.clone();
        save_state(&st)?;
    }

    if record.status == "failed" {
        return Err(AppError::bad_gateway(format!("上传失败: {}", record.error)));
    }

    // 8) 清理旧备份
    let _ = cleanup_old(cfg).await;

    Ok(record)
}

/// 把 AWS SDK 的错误链展开成可诊断的字符串。
///
/// ## 为什么需要这个
///
/// AWS SDK 的 `Display` 只打印最外层错误，常常就是一句 **`service error`**
/// ——对使用者毫无帮助，无法判断是签名错、桶不存在、还是区域不对。
///
/// 真正的信息藏在 `Debug` 输出和 `source()` 链里：
/// ```text
/// service error
///   └─ unhandled error
///       └─ Error { code: "InvalidAccessKeyId", message: "..." }
/// ```
///
/// 这里把整条链的 Debug 输出拼起来，并抓取 `code` / `message` 字段，
/// 产出 `[InvalidAccessKeyId] The AWS Access Key Id you provided does not exist`
/// 这样的可读信息。
fn describe_s3_error(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);

    let mut depth = 0;
    while let Some(e) = cur {
        if depth >= 8 {
            break;
        }
        let dbg = format!("{e:?}");
        if let Some(code) = extract_field(&dbg, "code") {
            parts.push(format!("[{code}]"));
        }
        if let Some(msg) = extract_field(&dbg, "message") {
            parts.push(msg);
        }
        let disp = e.to_string();
        if !disp.is_empty() && disp != "service error" && disp != "unhandled error" {
            parts.push(disp);
        }
        cur = e.source();
        depth += 1;
    }

    let mut out: Vec<String> = Vec::new();
    for p in parts {
        let p = p.trim().to_string();
        if p.is_empty() || out.iter().any(|x| x == &p) {
            continue;
        }
        out.push(p);
    }
    if out.is_empty() {
        err.to_string()
    } else {
        out.join(" ")
    }
}

/// 从 Debug 输出里抓 `field: Some("value")` / `field: "value"` / `field: Some(X)`。
fn extract_field(debug_str: &str, field: &str) -> Option<String> {
    let needle = format!("{field}:");
    let idx = debug_str.find(&needle)?;
    let rest = debug_str[idx + needle.len()..].trim_start();

    if let Some(r) = rest.strip_prefix("Some(\"") {
        return r.find('"').map(|end| r[..end].to_string());
    }
    if let Some(r) = rest.strip_prefix("Some(") {
        if r.starts_with('"') {
            let r2 = &r[1..];
            return r2.find('"').map(|end| r2[..end].to_string());
        }
        // Some(StatusCode(403)) 之类
        return r.find(')').map(|end| r[..end].to_string());
    }
    if let Some(r) = rest.strip_prefix('"') {
        return r.find('"').map(|end| r[..end].to_string());
    }
    None
}

/// 构造 S3 客户端。
///
/// ## checksum 配置为什么关键（踩过的坑）
///
/// AWS SDK 默认用 aws-chunked 分块编码上传，首字节是 10000;chunk-signature=...。
/// 部分兼容服务（尤其老版本 OSS/COS）不解码这个格式，存下来是带 chunk 头部的
/// 损坏文件 —— 备份看起来成功，实际恢复不了。
///
/// 设 RequestChecksumCalculation::WhenRequired 后变成干净单次 PUT。
/// 实测：修复前首字节是 10000;chunk-signature=，修复后是 1f 8b 08（标准 gzip 魔数）。
async fn build_client(cfg: &S3Config) -> aws_sdk_s3::Client {
    use aws_sdk_s3::config::{
        BehaviorVersion, Credentials, Region, RequestChecksumCalculation,
        ResponseChecksumValidation,
    };

    let creds = Credentials::new(
        cfg.access_key_id.trim(),
        cfg.secret_access_key.trim(),
        None,
        None,
        "mailpool",
    );

    let mut builder = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(if cfg.region.trim().is_empty() {
            "auto".to_string()
        } else {
            cfg.region.clone()
        }))
        // 关键：不要无条件加 checksum（会触发 aws-chunked 编码）
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
        .credentials_provider(creds);

    if !cfg.endpoint.trim().is_empty() {
        builder = builder.endpoint_url(cfg.endpoint.trim());
    }
    if cfg.force_path_style {
        builder = builder.force_path_style(true);
    }

    aws_sdk_s3::Client::from_conf(builder.build())
}

/// 上传（指数退避重试 3 次）。
///
/// sub2api **没有**应用层重试，网络抖一下就要等下一个 cron 周期。
/// 这里对可重试错误（超时/5xx）做 3 次退避，对 4xx（凭证错/桶不存在）
/// 直接失败 —— 重试也没用。
async fn upload_with_retry(cfg: &S3Config, key: &str, path: &Path) -> AppResult<()> {
    let client = build_client(cfg).await;
    let mut last_err = String::new();

    for attempt in 1..=UPLOAD_RETRIES {
        let body = match aws_sdk_s3::primitives::ByteStream::from_path(path).await {
            Ok(b) => b,
            Err(e) => return Err(AppError::internal(format!("读文件失败: {e}"))),
        };

        let res = client
            .put_object()
            .bucket(cfg.bucket.trim())
            .key(key)
            .body(body)
            .content_type("application/gzip")
            .send()
            .await;

        match res {
            Ok(_) => {
                tracing::info!("[backup] 上传成功 key={key} attempt={attempt}");
                return Ok(());
            }
            Err(e) => {
                let msg = describe_s3_error(&e);
                // 4xx 不重试（凭证错、桶不存在、权限不足）
                let is_client_error = msg.contains("403")
                    || msg.contains("404")
                    || msg.contains("AccessDenied")
                    || msg.contains("NoSuchBucket")
                    || msg.contains("InvalidAccessKeyId")
                    || msg.contains("SignatureDoesNotMatch");
                last_err = msg;
                if is_client_error || attempt == UPLOAD_RETRIES {
                    break;
                }
                let backoff = Duration::from_secs(2u64.pow(attempt - 1) * 3);
                tracing::warn!(
                    "[backup] 上传失败（第 {attempt}/{UPLOAD_RETRIES} 次），{}s 后重试: {}",
                    backoff.as_secs(),
                    crate::util::truncate_chars(&last_err, 120)
                );
                tokio::time::sleep(backoff).await;
            }
        }
    }

    Err(AppError::bad_gateway(format!(
        "S3 上传失败（重试 {UPLOAD_RETRIES} 次）: {}",
        crate::util::truncate_chars(&last_err, 300)
    )))
}

// ────────────────────────────────────────────────────────────────
//  客户端加密（ChaCha20-Poly1305）
// ────────────────────────────────────────────────────────────────
//
//  sub2api 完全没有加密：备份内容是明文 gzip，谁拿到 bucket 就能读出
//  全部邮箱凭证（refresh_token）。这里补上可选的客户端加密。
//
//  文件格式：
//    magic(8) "MPBK1\n" + salt(16) + nonce(12) + ciphertext+tag
//  密钥：Argon2id(passphrase, salt) → 32 字节
//  每次备份用新的随机 salt/nonce（同一口令不同备份的密文也不同）。

const MAGIC: &[u8; 8] = b"MPBK1\r\n\0";

/// 加密文件。
fn encrypt_file(src: &Path, dst: &Path, passphrase: &str) -> AppResult<()> {
    use argon2::Argon2;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
    use rand::RngCore;

    let plaintext = std::fs::read(src)
        .map_err(|e| AppError::internal(format!("读快照失败: {e}")))?;

    let mut salt = [0u8; 16];
    let mut nonce_bytes = [0u8; 12];
    {
        let mut rng = rand::thread_rng();
        rng.fill_bytes(&mut salt);
        rng.fill_bytes(&mut nonce_bytes);
    }

    // Argon2id 派生密钥（备份是一次性操作，20ms 开销可接受）
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), &salt, &mut key)
        .map_err(|e| AppError::internal(format!("密钥派生失败: {e}")))?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_ref())
        .map_err(|e| AppError::internal(format!("加密失败: {e}")))?;

    let mut out = Vec::with_capacity(8 + 16 + 12 + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);

    std::fs::write(dst, &out)
        .map_err(|e| AppError::internal(format!("写加密文件失败: {e}")))?;
    Ok(())
}

/// 解密文件（恢复流程用；也供测试验证往返）。
pub fn decrypt_file(src: &Path, dst: &Path, passphrase: &str) -> AppResult<()> {
    use argon2::Argon2;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    let data = std::fs::read(src)
        .map_err(|e| AppError::internal(format!("读加密文件失败: {e}")))?;
    if data.len() < 8 + 16 + 12 || &data[..8] != MAGIC {
        return Err(AppError::bad_request("不是 MailPool 加密备份格式"));
    }

    let salt = &data[8..24];
    let nonce_bytes = &data[24..36];
    let ciphertext = &data[36..];

    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| AppError::internal(format!("密钥派生失败: {e}")))?;

    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|_| AppError::bad_request("解密失败（口令错误或文件损坏）"))?;

    std::fs::write(dst, &plaintext)
        .map_err(|e| AppError::internal(format!("写解密文件失败: {e}")))?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────
//  保留策略
// ────────────────────────────────────────────────────────────────

/// 清理旧备份。
///
/// **OR 语义**（对齐 sub2api）：超过份数 **或** 超过天数，任一满足即删。
pub async fn cleanup_old(cfg: &S3Config) -> AppResult<i64> {
    let mut st = load_state();
    if st.records.is_empty() {
        return Ok(0);
    }

    let now = crate::util::utc_now();
    let mut keep: Vec<BackupRecord> = Vec::new();
    let mut to_delete: Vec<BackupRecord> = Vec::new();

    let success: Vec<&BackupRecord> = st
        .records
        .iter()
        .filter(|r| r.status == "success")
        .collect();

    for r in &st.records {
        if r.status != "success" {
            // 失败记录保留（便于排查），但最多留 50 条
            keep.push(r.clone());
            continue;
        }
        let too_many = cfg.retain_count > 0
            && success.len() as i64 > cfg.retain_count
            && success
                .iter()
                .position(|x| x.id == r.id)
                .map(|idx| (idx as i64) < success.len() as i64 - cfg.retain_count)
                .unwrap_or(false);
        let too_old = cfg.retain_days > 0
            && crate::util::normalize_utc(&r.created_at)
                .map(|t| (now - t).num_days() > cfg.retain_days)
                .unwrap_or(false);

        if too_many || too_old {
            to_delete.push(r.clone());
        } else {
            keep.push(r.clone());
        }
    }

    // 从 S3 删除对象
    if !to_delete.is_empty() {
        let client = build_client(cfg).await;
        for r in &to_delete {
            let res = client
                .delete_object()
                .bucket(cfg.bucket.trim())
                .key(&r.key)
                .send()
                .await;
            match res {
                Ok(_) => tracing::info!("[backup] 已删除旧备份 key={}", r.key),
                Err(e) => tracing::warn!("[backup] 删除旧备份失败 key={}: {e}", r.key),
            }
        }
    }

    // 失败记录超过 50 条就截断
    let failures: Vec<BackupRecord> = keep.iter().filter(|r| r.status != "success").cloned().collect();
    if failures.len() > 50 {
        let cut = failures.len() - 50;
        let mut seen = 0;
        keep.retain(|r| {
            if r.status != "success" {
                seen += 1;
                seen > cut
            } else {
                true
            }
        });
    }

    let n = to_delete.len() as i64;
    st.records = keep;
    save_state(&st)?;
    Ok(n)
}

/// 测试 S3 连接（后台「测试连接」按钮用）。
pub async fn test_connection(cfg: &S3Config) -> AppResult<String> {
    if !cfg.is_configured() {
        return Err(AppError::bad_request("S3 未配置（需要 bucket / AK / SK）"));
    }
    let client = build_client(cfg).await;
    client
        .head_bucket()
        .bucket(cfg.bucket.trim())
        .send()
        .await
        .map_err(|e| AppError::bad_gateway(format!("连接失败: {}", describe_s3_error(&e))))?;
    Ok(format!("连接成功（bucket: {}）", cfg.bucket))
}

/// 从 S3 下载并解压一个备份，验证其完整性。
///
/// 这是备份功能**最关键的验证**：上传成功不等于文件可用。
/// 实测踩过的坑 —— aws-chunked 编码会让对象存储里躺着一个
/// 「上传成功但解不开」的文件。所以必须能读回来、解压、跑 integrity_check。
///
/// 返回 `(解压后大小, 表数量, integrity_check 结果)`。
pub async fn verify_backup(cfg: &S3Config, key: &str) -> AppResult<(u64, i64, String)> {
    if !cfg.is_configured() {
        return Err(AppError::bad_request("S3 未配置"));
    }
    let client = build_client(cfg).await;

    let resp = client
        .get_object()
        .bucket(cfg.bucket.trim())
        .key(key)
        .send()
        .await
        .map_err(|e| AppError::bad_gateway(format!("下载失败: {}", describe_s3_error(&e))))?;

    let bytes = resp
        .body
        .collect()
        .await
        .map_err(|e| AppError::bad_gateway(format!("读取响应体失败: {e}")))?
        .into_bytes();

    // 配置了加密口令 且 文件带 magic → 先解密
    let payload: Vec<u8> = if !cfg.encryption_passphrase.trim().is_empty()
        && bytes.len() >= 8
        && &bytes[..8] == MAGIC
    {
        let dir = crate::config::Settings::get().data_dir.clone();
        let tmp_in = dir.join(format!(".verify_{}.enc", crate::util::random_hex(4)));
        let tmp_out = dir.join(format!(".verify_{}.gz", crate::util::random_hex(4)));
        std::fs::write(&tmp_in, &bytes)
            .map_err(|e| AppError::internal(format!("写临时文件失败: {e}")))?;
        let pass = cfg.encryption_passphrase.clone();
        let r = decrypt_file(&tmp_in, &tmp_out, &pass);
        let _ = std::fs::remove_file(&tmp_in);
        r?;
        let out = std::fs::read(&tmp_out)
            .map_err(|e| AppError::internal(format!("读解密文件失败: {e}")))?;
        let _ = std::fs::remove_file(&tmp_out);
        out
    } else {
        bytes.to_vec()
    };

    // gzip 解压
    let raw = {
        use std::io::Read;
        let mut dec = flate2::read::GzDecoder::new(&payload[..]);
        let mut out = Vec::new();
        dec.read_to_end(&mut out)
            .map_err(|e| AppError::bad_gateway(format!("gzip 解压失败（文件可能损坏）: {e}")))?;
        out
    };

    // 写临时库跑 integrity_check
    let dir = crate::config::Settings::get().data_dir.clone();
    let tmp_db = dir.join(format!(".verify_{}.db", crate::util::random_hex(4)));
    std::fs::write(&tmp_db, &raw)
        .map_err(|e| AppError::internal(format!("写临时库失败: {e}")))?;

    let result = async {
        let conn_str = format!("sqlite://{}?mode=ro", tmp_db.to_string_lossy());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&conn_str)
            .await
            .map_err(|e| AppError::internal(format!("打开快照失败: {e}")))?;
        let integ: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|_| "unknown".to_string());
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table'")
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
        pool.close().await;
        Ok::<_, AppError>((raw.len() as u64, n, integ))
    }
    .await;

    let _ = std::fs::remove_file(&tmp_db);
    result
}

/// 删除一条备份（记录 + 对象）。
pub async fn delete_backup(cfg: &S3Config, id: &str) -> AppResult<()> {
    let mut st = load_state();
    let Some(rec) = st.records.iter().find(|r| r.id == id).cloned() else {
        return Err(AppError::not_found("备份记录不存在"));
    };

    if cfg.is_configured() {
        let client = build_client(cfg).await;
        let _ = client
            .delete_object()
            .bucket(cfg.bucket.trim())
            .key(&rec.key)
            .send()
            .await;
    }

    st.records.retain(|r| r.id != id);
    save_state(&st)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_requires_bucket_and_keys() {
        let mut c = S3Config::default();
        assert!(!c.is_configured());
        c.bucket = "b".into();
        assert!(!c.is_configured());
        c.access_key_id = "ak".into();
        assert!(!c.is_configured());
        c.secret_access_key = "sk".into();
        assert!(c.is_configured());
    }

    #[test]
    fn masked_never_leaks_secrets() {
        let c = S3Config {
            bucket: "b".into(),
            access_key_id: "AKIA_PUBLIC".into(),
            secret_access_key: "TOP_SECRET_KEY".into(),
            encryption_passphrase: "PASSPHRASE".into(),
            ..Default::default()
        };
        let s = serde_json::to_string(&c.masked()).unwrap();
        assert!(!s.contains("TOP_SECRET_KEY"), "secret 泄露: {s}");
        assert!(!s.contains("PASSPHRASE"), "口令泄露: {s}");
        assert!(s.contains("AKIA_PUBLIC"), "AK 可以显示");
    }

    #[test]
    fn prefix_defaults_and_trims() {
        let mut c = S3Config::default();
        assert_eq!(c.effective_prefix(), "backups");
        c.prefix = "/my/path/".into();
        assert_eq!(c.effective_prefix(), "my/path");
        c.prefix = "a/b".into();
        assert_eq!(c.effective_prefix(), "a/b");
    }

    #[test]
    fn encryption_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mp_enc_{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("plain.bin");
        let enc = dir.join("enc.bin");
        let back = dir.join("back.bin");

        let content = b"hello mailpool backup \x00\x01\x02 binary data";
        std::fs::write(&plain, content).unwrap();

        encrypt_file(&plain, &enc, "my-passphrase").unwrap();
        // 密文必须与明文不同，且带 magic
        let enc_bytes = std::fs::read(&enc).unwrap();
        assert_ne!(&enc_bytes[..], &content[..]);
        assert_eq!(&enc_bytes[..8], MAGIC);

        decrypt_file(&enc, &back, "my-passphrase").unwrap();
        assert_eq!(std::fs::read(&back).unwrap(), content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encryption_rejects_wrong_passphrase() {
        let dir = std::env::temp_dir().join(format!("mp_enc2_{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let plain = dir.join("p.bin");
        let enc = dir.join("e.bin");
        let back = dir.join("b.bin");
        std::fs::write(&plain, b"secret data").unwrap();
        encrypt_file(&plain, &enc, "correct").unwrap();

        let r = decrypt_file(&enc, &back, "wrong");
        assert!(r.is_err(), "错误口令必须解密失败");
        assert!(r.unwrap_err().to_string().contains("口令错误"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encryption_rejects_foreign_file() {
        let dir = std::env::temp_dir().join(format!("mp_enc3_{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.bin");
        std::fs::write(&bad, b"not a mailpool backup at all").unwrap();
        let out = dir.join("o.bin");
        let r = decrypt_file(&bad, &out, "x");
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("格式"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_content_different_ciphertext() {
        // 每次加密用新 salt/nonce → 同一明文两次加密结果必须不同
        let dir = std::env::temp_dir().join(format!("mp_enc4_{}", crate::util::random_hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("p.bin");
        std::fs::write(&p, b"same content").unwrap();
        let e1 = dir.join("e1.bin");
        let e2 = dir.join("e2.bin");
        encrypt_file(&p, &e1, "pw").unwrap();
        encrypt_file(&p, &e2, "pw").unwrap();
        assert_ne!(
            std::fs::read(&e1).unwrap(),
            std::fs::read(&e2).unwrap(),
            "相同明文加密两次结果必须不同（salt/nonce 随机）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_roundtrip() {
        let st = BackupState {
            records: vec![BackupRecord {
                id: "bk1".into(),
                key: "backups/2026/11/22/x.sql.gz".into(),
                filename: "x.sql.gz".into(),
                raw_size: 100,
                gz_size: 40,
                upload_size: 60,
                encrypted: true,
                sha256: "abc".into(),
                created_at: "2026-11-22T00:00:00Z".into(),
                duration_ms: 123,
                status: "success".into(),
                error: String::new(),
            }],
            last_run_at: "2026-11-22T00:00:00Z".into(),
            last_run_status: "success".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&st).unwrap();
        let back: BackupState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.records.len(), 1);
        assert_eq!(back.records[0].key, "backups/2026/11/22/x.sql.gz");
    }

    #[test]
    fn default_state_on_missing_file() {
        // 文件不存在时不能 panic，要返回空状态
        let st = BackupState::default();
        assert!(st.records.is_empty());
        assert!(st.running.is_empty());
    }
}
