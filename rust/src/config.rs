//! 全局配置：路径 + 环境变量。
//!
//! 与 Python 版 `app/config.py` 对齐：
//! - `MAILPOOL_DATA`   数据目录（默认 `<repo>/data`）
//! - `MAILPOOL_HOST` / `MAILPOOL_PORT`
//! - `MAILPOOL_ADMIN_PASSWORD`
//! - `MAILPOOL_SESSION_SECRET`
//! - `MAILPOOL_TIMEOUT`

use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::money::Money;

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// 全局配置（进程内单例）。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 数据目录（SQLite 库、会话密钥、日志）
    pub data_dir: PathBuf,
    /// SQLite 数据库文件
    pub db_path: PathBuf,
    /// 模板目录
    pub template_dir: PathBuf,
    /// 静态资源目录
    pub static_dir: PathBuf,
    /// 监听地址
    pub host: String,
    /// 监听端口
    pub port: u16,
    /// 管理后台口令（为空=仅回环地址免密）
    pub admin_password: String,
    /// 会话签名密钥
    pub session_secret: String,
    /// 默认收码超时（秒）
    pub default_timeout: u64,
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

/// 仓库根目录：二进制在 `target/{debug,release}/` 下时向上找。
fn repo_root() -> PathBuf {
    // 1) 显式覆盖：部署时把静态资源挂在固定路径
    if let Ok(v) = env::var("MAILPOOL_ROOT") {
        if !v.trim().is_empty() {
            return PathBuf::from(v);
        }
    }
    // 2) 从可执行文件位置推断（target/debug/mailpool → 上两级）
    if let Ok(exe) = env::current_exe() {
        let mut cur = exe.as_path();
        for _ in 0..4 {
            if let Some(parent) = cur.parent() {
                if parent.join("app").join("web").join("templates").is_dir() {
                    return parent.to_path_buf();
                }
                cur = parent;
            }
        }
    }
    // 3) 兜底：当前工作目录
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 会话签名密钥：环境变量 → `{DATA_DIR}/.session_secret` → 随机生成并持久化。
fn load_session_secret(data_dir: &Path) -> String {
    if let Ok(v) = env::var("MAILPOOL_SESSION_SECRET") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    let file = data_dir.join(".session_secret");
    if let Ok(content) = std::fs::read_to_string(&file) {
        let content = content.trim().to_string();
        if content.len() >= 32 {
            return content;
        }
    }
    let secret = crate::util::random_token(48);
    let _ = std::fs::create_dir_all(data_dir);
    if std::fs::write(&file, &secret).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!("已生成新的会话签名密钥: {}", file.display());
    } else {
        tracing::warn!("无法持久化会话密钥，降级为进程内随机密钥（重启后需重新登录）");
    }
    secret
}

impl Settings {
    fn from_env() -> Self {
        let root = repo_root();
        let data_dir = env::var("MAILPOOL_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.join("data"));
        let _ = std::fs::create_dir_all(&data_dir);

        let session_secret = load_session_secret(&data_dir);

        Self {
            db_path: data_dir.join("mailpool.db"),
            template_dir: root.join("app").join("web").join("templates"),
            static_dir: root.join("app").join("web").join("static"),
            host: env_or("MAILPOOL_HOST", "0.0.0.0"),
            port: env_or("MAILPOOL_PORT", "18080")
                .parse()
                .unwrap_or(18080),
            admin_password: env_or("MAILPOOL_ADMIN_PASSWORD", ""),
            session_secret,
            default_timeout: env_or("MAILPOOL_TIMEOUT", "120").parse().unwrap_or(120),
            data_dir,
        }
    }

    /// 进程内单例。
    pub fn get() -> &'static Settings {
        SETTINGS.get_or_init(Settings::from_env)
    }

    /// 是否配置了管理口令（未配置 = 仅回环地址免密）。
    pub fn has_admin_password(&self) -> bool {
        !self.admin_password.is_empty()
    }
}

// ────────────────────────────────────────────────────────────────
//  内置项目 Logo 库（对齐 Python 版 PROJECT_LOGOS）
// ────────────────────────────────────────────────────────────────

/// 内置 logo 列表：`(id, 显示名)`
pub const PROJECT_LOGOS: &[(&str, &str)] = &[
    ("kick", "Kick"),
    ("opensea", "OpenSea"),
    ("z_ai", "Z.AI"),
    ("xai", "xAI"),
    ("microsoft", "Microsoft 系通用"),
    ("google", "Google 系通用"),
    ("apple", "Apple 系通用"),
    ("claude", "Claude"),
    ("chatgpt", "ChatGPT"),
    ("cursor", "Cursor"),
    ("ollama", "Ollama"),
    ("huggingface", "HuggingFace"),
    ("rubii", "Rubii"),
    ("mulerun", "MuleRun"),
    ("klingai", "KlingAI"),
    ("miraimind", "MiraiMind"),
    ("warp", "Warp"),
    ("aihubmix", "AIHubMix"),
    ("windsurf", "Devin / Windsurf"),
    ("dreamina", "Dreamina"),
    ("manus", "Manus"),
    ("pixmax", "PixMax"),
    ("stagewise", "StageWise"),
    ("studentai", "StudentAI"),
    ("traq", "Traq"),
    ("weryai", "WeryAI"),
    ("zopia", "Zopia"),
    ("linkedin", "LinkedIn 领英"),
    ("teamviewer", "TeamViewer"),
    ("zoom", "Zoom"),
    ("x", "X 推特"),
    ("discord", "Discord"),
    ("facebook", "Facebook"),
    ("reddit", "Reddit"),
    ("netflix", "Netflix"),
    ("amazon", "Amazon"),
    ("shopify", "Shopify"),
    ("temu", "Temu"),
    ("shein", "SHEIN"),
    ("github", "GitHub"),
    ("gitlab", "GitLab"),
    ("cloudflare", "Cloudflare"),
    ("nvidia", "NVIDIA"),
    ("resend", "Resend"),
    ("aws", "AWS Builder ID / Kiro"),
    ("jetbrains", "JetBrains"),
    ("binance", "币安"),
    ("dune", "Dune"),
    ("bitpay", "BitPay"),
    ("ic", "IC"),
    ("fileyourtaxes", "FileYourTaxes"),
];

/// 项目 config 里的 logo 字段 → 图片地址。
///
/// - 内置 id（如 chatgpt）→ `/static/img/projects/chatgpt.png`（找不到回退 .svg）
/// - 以 http 开头 → 视为自定义图片 URL 原样返回
/// - 空 → 返回空串（前端用首字母占位）
pub fn project_logo_src(logo: &str) -> String {
    let logo = logo.trim();
    if logo.is_empty() {
        return String::new();
    }
    if logo.starts_with("http://") || logo.starts_with("https://") {
        return logo.to_string();
    }
    let base = logo.rsplit('/').next().unwrap_or(logo);
    let base = match base.rsplit_once('.') {
        Some((stem, ext))
            if matches!(ext.to_ascii_lowercase().as_str(), "png" | "svg" | "jpg" | "jpeg") =>
        {
            stem
        }
        _ => base,
    };
    let dir = Settings::get().static_dir.join("img").join("projects");
    for ext in ["png", "svg"] {
        if dir.join(format!("{base}.{ext}")).exists() {
            return format!("/static/img/projects/{base}.{ext}");
        }
    }
    format!("/static/img/projects/{base}.png")
}

/// 金额：从元（字符串/数字）转厘。见 [`Money`]。
pub fn yuan_to_money(yuan: &str) -> Option<Money> {
    Money::parse_yuan(yuan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_http_passthrough() {
        assert_eq!(project_logo_src("https://x.test/a.png"), "https://x.test/a.png");
    }

    #[test]
    fn logo_empty() {
        assert_eq!(project_logo_src(""), "");
    }
}
