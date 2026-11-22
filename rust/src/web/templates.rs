//! 模板层：minijinja 原样加载现有 22 个 Jinja 模板。
//!
//! **设计原则：模板文件一行都不改。** 因此这里必须补齐 Python Jinja2
//! 与 minijinja 之间的全部语义差异，而不是反过来改模板。
//!
//! 需要处理的差异：
//!
//! | Jinja2 写法 | minijinja 处理 |
//! |---|---|
//! | `{{ asset_version() }}` | 注册全局函数 `asset_version` |
//! | `{{ r.balance / 1000 }}` | 整数除法差异：Jinja2 的 `/` 是真除（float），minijinja 同 |
//! | `{{ x \| tojson }}` | 内置（`json` feature） |
//! | `{{ '%.3f' \| format(x) }}` | minijinja **没有** `format` 过滤器 → 自己注册 |
//! | `{{ d.strftime('%Y-%m-%d') }}` | Python 方法调用 → 自定义测试/函数 |
//! | `{{ x \| default('') }}` | 内置 |
//! | `{% if not x %}` / `{% elif %}` | 完全兼容 |
//! | `{{ dict.key }}` | minijinja 支持 map 属性访问 |
//!
//! 引擎在启动时**预编译全部模板**：任何语法不兼容都会在启动阶段暴露，
//! 而不是等用户点到某个页面才 500。

use std::path::Path;
use std::sync::Arc;

use minijinja::{path_loader, Environment, Value};

use crate::error::{AppError, AppResult};

/// 模板引擎（`Arc` 包裹以便跨请求共享；minijinja 的 Environment 是 Send+Sync）。
pub struct Templates {
    env: Environment<'static>,
}

impl Templates {
    /// 加载模板目录并注册全部兼容层。
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let mut env = Environment::new();
        env.set_loader(path_loader(dir));

        // ── 关键兼容：未定义变量不报错 ──
        // Jinja2 的默认 Undefined 渲染成空串（`{{ missing }}` → ""），
        // 只有显式 `{{ x.attr }}` 才会告警。minijinja 默认**直接报错**，
        // 22 个模板里有大量「可能缺字段」的引用，必须放宽成 Jinja2 语义，
        // 否则页面会 500。用 Chainable 是因为模板里也有 `{% if x.y %}`。
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Chainable);

        // ── 全局变量 / 函数 ──
        // 模板里写作 `{{ asset_version() }}`，所以必须是可调用的函数
        env.add_function("asset_version", || asset_version());
        env.add_function("now_iso", || {
            crate::util::utc_now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        });

        // Python 的 `len` 在模板里偶尔会用
        env.add_function("len", |v: Value| v.len().unwrap_or(0) as u64);

        // ── 过滤器 ──
        // 1) `|format`：Jinja2 的 `'%.3f' | format(x)`。
        //    minijinja 内置没有，这里按 printf 风格实现常用子集。
        env.add_filter("format", format_filter);
        // 2) `|tojson`：minijinja 的 `json` feature 自带，但默认语义是
        //    输出带引号的 JSON 字符串；模板里用在 `<script>` 内需要 Raw。
        //    内置 `tojson` 已满足，无需覆盖。
        // 3) `|strftime`：给 datetime 用（Python 模板里有 `.strftime()` 方法调用，
        //    这里额外提供过滤器作为可选替代）
        env.add_filter("strftime", strftime_filter);
        // 4) `|money`：厘 → 元（Rust 侧统一转换，避免模板里做除法丢精度）
        env.add_filter("money", money_filter);
        // 5) `|default_if_none`
        env.add_filter("default_none", |v: Value, d: Value| {
            if v.is_none() || v.is_undefined() {
                d
            } else {
                v
            }
        });
        // 6) `|length`：Jinja2 对 undefined / none 返回 0，minijinja 直接报错。
        //    模板里 `{{ projects|length }}` 这类写法在缺上下文时会炸，必须兜住。
        env.add_filter("length", |v: Value| -> usize {
            if v.is_undefined() || v.is_none() {
                0
            } else {
                v.len().unwrap_or(0)
            }
        });
        // 7) `|get(key, default)`：dict 取值（用于 `p.config.get(...)` 的 pipe 写法）。
        //    注意 minijinja 的 map **没有** `.get()` 方法（实测报
        //    `map has no method named get`），settings.html 那 4 处必须改成
        //    `p.config[key]|default(d)`；渲染前 `web::pages` 会把 config 预展开。
        env.add_filter("get", |v: Value, key: Value, default: Value| -> Value {
            match v.get_item(&key) {
                Ok(found) if !found.is_undefined() && !found.is_none() => found,
                _ => default,
            }
        });
        // 8) `|urlencode`：orders.html:58 用它拼查询串。
        //    minijinja 的 `urlencode` 需要 `urlencode` feature；这里直接注册，
        //    语义对齐 Python `urllib.parse.quote`（保留 unreserved 字符）。
        env.add_filter("urlencode", |v: Value| -> String {
            let s = match v.as_str() {
                Some(s) => s.to_string(),
                None if v.is_none() || v.is_undefined() => String::new(),
                None => v.to_string(),
            };
            urlencode(&s)
        });

        // ── 测试 ──
        env.add_test("defined", |v: Value| !v.is_undefined());

        // ── 启动期预编译：语法不兼容当场失败 ──
        // `env.templates()` 只列出**已加载**的模板；用 loader 时必须先主动加载一遍。
        let names: Vec<String> = collect_template_names(dir);
        let mut ok = 0usize;
        let mut problems: Vec<String> = Vec::new();
        for name in &names {
            match env.get_template(name) {
                Ok(_) => ok += 1,
                Err(e) => problems.push(format!("{name}: {e}")),
            }
        }
        tracing::info!("模板预编译: {ok}/{} 通过", names.len());
        if !problems.is_empty() {
            for p in &problems {
                tracing::error!("模板不兼容 → {p}");
            }
            anyhow::bail!(
                "有 {} 个模板无法在 minijinja 下编译：\n{}",
                problems.len(),
                problems.join("\n")
            );
        }

        Ok(Self { env })
    }

    /// 渲染模板。
    pub fn render(&self, name: &str, ctx: Value) -> AppResult<String> {
        let tmpl = self
            .env
            .get_template(name)
            .map_err(|e| AppError::internal(format!("模板 {name} 不存在: {e}")))?;
        tmpl.render(ctx)
            .map_err(|e| AppError::internal(format!("模板 {name} 渲染失败: {e}")))
    }

    /// 渲染模板（上下文为 `serde_json::Value`，会自动把时间字段包成
    /// [`DateTimeValue`]，让模板里的 `.strftime(...)` 可用）。
    ///
    /// 这是所有页面 handler 应该用的入口 —— 直接 `render()` 传原始 JSON 的话，
    /// 时间字段是字符串，`.strftime()` 会报 `string has no method named strftime`。
    pub fn render_json(&self, name: &str, ctx: &serde_json::Value) -> AppResult<String> {
        let wrapped = build_value(ctx);
        let tmpl = self
            .env
            .get_template(name)
            .map_err(|e| AppError::internal(format!("模板 {name} 不存在: {e}")))?;
        tmpl.render(wrapped)
            .map_err(|e| AppError::internal(format!("模板 {name} 渲染失败: {e}")))
    }
}

/// 递归把 JSON 转成 minijinja 的 `Value`，并给时间字段换成 [`DateTimeValue`]。
///
/// 不走 `Value::from_serialize` 是因为那条路会把 [`DateTimeValue`] 序列化成
/// 字符串（`Serialize` impl 的锅），`.strftime` 方法就丢了。必须手动建 map，
/// 让时间字段以 **object** 身份进去。
fn build_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Object(map) => {
            let pairs: Vec<(Value, Value)> = map
                .iter()
                .map(|(k, val)| (Value::from(k.as_str()), build_field_value(k, val)))
                .collect();
            Value::from_iter(pairs)
        }
        serde_json::Value::Array(items) => {
            Value::from_iter(items.iter().map(build_value))
        }
        serde_json::Value::Null => Value::from(()),
        serde_json::Value::Bool(b) => Value::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::from(i)
            } else if let Some(f) = n.as_f64() {
                Value::from(f)
            } else {
                Value::from(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::from(s.as_str()),
    }
}

/// 单个字段：时间字段换成 object，其余递归。
fn build_field_value(key: &str, v: &serde_json::Value) -> Value {
    // `last_scan_at` 是**字符串**字段（health_summary 返回 ISO8601 串），
    // pool.html 对它做 `[:19]|replace('T',' ')`，包成 Object 反而不能切片。
    // 其它 `*_at` 字段模板都调 `.strftime()`，必须包成 Object。
    if key == "last_scan_at" {
        return build_value(v);
    }
    if let serde_json::Value::String(s) = v {
        let looks_time = key.ends_with("_at") || key.ends_with("_time");
        if looks_time {
            if let Some(dt) = crate::util::normalize_utc(s) {
                return DateTimeValue::from_naive(Some(dt.naive_utc())).to_value();
            }
            // `_at` 字段但值为空串 → 视为无时间（让 `{% if %}` 判假）
            if s.is_empty() {
                return DateTimeValue::from_naive(None).to_value();
            }
        }
    }
    build_value(v)
}

/// 递归列出模板目录下所有 `.html`（相对路径，正斜杠分隔）。
fn collect_template_names(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, cur: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(cur) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("html") {
                if let Ok(rel) = p.strip_prefix(base) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// 静态资源版本号：用 style.css 的 mtime（内容不变则 URL 不变，可正常缓存）。
pub fn asset_version() -> String {
    let css = crate::config::Settings::get()
        .static_dir
        .join("css")
        .join("style.css");
    match std::fs::metadata(&css).and_then(|m| m.modified()) {
        Ok(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "1".to_string()),
        Err(_) => "1".to_string(),
    }
}

// ────────────────────────────────────────────────────────────────
//  自定义过滤器
// ────────────────────────────────────────────────────────────────

/// `'%.3f' | format(value)`：printf 风格的最小实现。
///
/// 支持 `%d` `%s` `%.Nf` `%f` `%x`。模板里只用到 `%.3f` 和 `%s`。
fn format_filter(spec: String, value: Value) -> String {
    let spec = spec.trim();
    if let Some(rest) = spec.strip_prefix("%.") {
        if let Some(prec_str) = rest.strip_suffix('f') {
            let prec: usize = prec_str.parse().unwrap_or(3);
            let n = as_f64(&value);
            return format!("{n:.prec$}");
        }
    }
    match spec {
        "%d" | "%i" => {
            let n = value
                .as_i64()
                .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(0);
            n.to_string()
        }
        "%f" => format!("{:.6}", as_f64(&value)),
        "%x" => value
            .as_i64()
            .map(|n| format!("{n:x}"))
            .unwrap_or_default(),
        "%s" => render_value(&value),
        _ => render_value(&value),
    }
}

fn as_f64(v: &Value) -> f64 {
    f64::try_from(v.clone()).unwrap_or(0.0)
}

fn render_value(v: &Value) -> String {
    if v.is_none() || v.is_undefined() {
        String::new()
    } else {
        v.to_string()
    }
}

/// `|strftime('%Y-%m-%d %H:%M')`：给 ISO8601 字符串用。
fn strftime_filter(value: Value, fmt: String) -> String {
    let Some(s) = value.as_str() else {
        return String::new();
    };
    let Some(dt) = crate::util::normalize_utc(s) else {
        return String::new();
    };
    // chrono 的格式化串与 Python strftime 基本一致（%Y %m %d %H %M %S）
    dt.format(&fmt).to_string()
}

/// `|money`：厘（整数）→ 元（3 位小数）。
fn money_filter(value: Value) -> String {
    let milli = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0);
    crate::money::Money::from_milli(milli).to_yuan()
}

/// 对齐 Python `urllib.parse.quote(s, safe='')` 的百分号编码。
///
/// unreserved 字符（`A-Z a-z 0-9 - _ . ~`）原样保留，其余按 UTF-8 字节转 `%XX`。
/// 与 minijinja 内置 `urlencode` 的差别：内置版会把空格编码成 `%20`，
/// 且保留 `/ . - _`；这里统一按 Python 语义（更保守，兼容性更好）。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ────────────────────────────────────────────────────────────────
//  时间值：支持模板里的 `.strftime('%Y-%m-%d')`
// ────────────────────────────────────────────────────────────────
//
// 模板里有 8 处 `{{ r.created_at.strftime('%Y-%m-%d %H:%M:%S') }}`。
// minijinja 不会给内置字符串自动加方法，所以这里包一个 `DateTimeValue`：
// - 序列化成 ISO8601 字符串（`{{ r.created_at }}` 直接输出也正常）
// - 暴露 `strftime(fmt)` 方法
//
// 这样**模板文件一行都不用改**。

/// 带 `strftime` 方法的日期时间值。
#[derive(Debug, Clone)]
pub struct DateTimeValue(pub Option<chrono::NaiveDateTime>);

impl DateTimeValue {
    pub fn from_naive(v: Option<chrono::NaiveDateTime>) -> Self {
        Self(v)
    }

    /// 包成 minijinja 的 **object** 值。
    ///
    /// 注意：不能直接用 `Value::from(dt_value)` 或 `context!` 宏 ——
    /// 那会走 `Serialize` 变成字符串，`strftime` 方法就丢了。
    /// 必须显式 `Value::from_object`，让 minijinja 保留 `Object` 的 `call_method`。
    pub fn to_value(&self) -> Value {
        Value::from_object(self.clone())
    }
}

impl std::fmt::Display for DateTimeValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(dt) => f.write_str(&crate::util::iso_from_naive(Some(dt))),
            None => Ok(()),
        }
    }
}

impl serde::Serialize for DateTimeValue {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Some(dt) => s.serialize_str(&crate::util::iso_from_naive(Some(dt))),
            None => s.serialize_none(),
        }
    }
}

impl minijinja::value::Object for DateTimeValue {
    fn repr(self: &std::sync::Arc<Self>) -> minijinja::value::ObjectRepr {
        minijinja::value::ObjectRepr::Plain
    }

    /// 直接输出时的渲染（`{{ created_at }}` → ISO8601）。
    ///
    /// 不实现这个的话 minijinja 会走 `Debug`，输出 `DateTimeValue(Some(...))`
    /// 这种调试串，页面就露馅了。
    fn render(
        self: &std::sync::Arc<Self>,
        out: &mut std::fmt::Formatter<'_>,
    ) -> std::fmt::Result {
        match self.0 {
            Some(dt) => write!(out, "{}", crate::util::iso_from_naive(Some(dt))),
            None => Ok(()),
        }
    }

    /// **关键**：让 `{% if r.created_at %}` 对空值判为假。
    ///
    /// 模板里大量写 `{{ r.created_at.strftime(...) if r.created_at else '—' }}`。
    /// 如果 Object 的 `is_true` 默认返回 true，空时间也会进 strftime 分支，
    /// 输出空串而不是 `—` —— 页面就显示出空白而不是破折号。
    fn is_true(self: &std::sync::Arc<Self>) -> bool {
        self.0.is_some()
    }

    /// 空值的「长度」为 0，也让 `|length` 有合理表现。
    fn enumerate(self: &std::sync::Arc<Self>) -> minijinja::value::Enumerator {
        minijinja::value::Enumerator::NonEnumerable
    }

    fn call_method(
        self: &std::sync::Arc<Self>,
        _state: &minijinja::State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, minijinja::Error> {
        match method {
            "strftime" => {
                let fmt = args
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("%Y-%m-%d %H:%M:%S");
                Ok(Value::from(match self.0 {
                    // 数据库存的是 naive UTC，展示时按原值格式化（与 Python 行为一致：
                    // Python 侧 datetime 也是 naive，strftime 直接作用于原值）
                    Some(dt) => dt.format(fmt).to_string(),
                    None => String::new(),
                }))
            }
            "isoformat" => Ok(Value::from(crate::util::iso_from_naive(self.0))),
            _ => Err(minijinja::Error::new(
                minijinja::ErrorKind::UnknownMethod,
                format!("datetime 没有方法 {method}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn all_templates_render_with_full_context() {
        // 门禁：21 个页面模板必须能渲染出非空 HTML。
        // 上下文是「超集」——包含所有模板可能引用的字段。
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("app")
            .join("web")
            .join("templates");
        if !dir.exists() {
            eprintln!("模板目录不存在，跳过");
            return;
        }
        let t = Templates::load(&dir).expect("模板必须全部可编译");

        let ctx = serde_json::json!({
            "admin_ok": true, "user_logged_in": false, "has_password": true,
            "billing_on": true, "active": "dashboard", "is_admin": true,
            "counts": {"total":995,"available":969,"using":12,"dead":14,"retired":0},
            "accounts": [{"id":1,"email":"a@b.test","provider":"outlook","status":"available",
                          "pool_status":"available","health_status":"healthy","health_reason":"",
                          "fail_count":0,"tags":"","proxy":"","remark":"","check_result":"",
                          "last_check_at":"2026-11-08T10:00:00Z","created_at":"2026-11-08T10:00:00Z"}],
            "health": {"total":995,"healthy":969,"unhealthy":5,"unknown":7,"dead":14,
                       "last_scan_at":"2026-11-22T09:00:00.123456Z","available":969,"using":12,"retired":0},
            "cid_stats": [{"client_id":"abc12345","total":995,"healthy":969,"unhealthy":5,"dead":14,
                           "unknown":7,"available":969,"using":12,"healthy_rate":97.4}],
            "page_offset": 0, "page_limit": 200, "page": 1, "total": 1, "page_size": 200,
            "pages": 1, "limit": 200, "total_pages": 1,
            "projects": [{"id":1,"code":"chatgpt001","name":"ChatGPT","provider":"outlook",
                          "enabled":1,"available":969,"is_pooled":true,
                          "logo_src":"/static/img/projects/chatgpt.png",
                          "card_price":"0.018","card_original_price":"","card_query":"",
                          "card_keywords":"","card_duration":120}],
            "rows": [{"id":1,"email":"a@b.test","balance":18000,"frozen_balance":0,
                      "available_balance":18000,"total_recharged":18000,"total_charged":0,
                      "status":1,"username":"user1","created_at":"2026-11-08T10:00:00Z",
                      "first_used_at":"2026-11-08T10:00:00Z","last_received_at":"2026-11-08T10:00:00Z",
                      "received_at":"2026-11-08T10:00:00Z","code":"123456","sender":"x@y.test",
                      "subject":"hi","project_code":"p1","project_name":"P1","billing_status":"charged",
                      "price":"0.018","charged_amount":"0.018","order_id":1,"user_id":1,"mail_count":1,
                      "account_status":"available","pooled":true,"log_type":"charge","amount":-18,
                      "money":18000,"trade_no":"T1","pay_type":"alipay","remark":"","card_key":"abc",
                      "used_by":0,"batch_id":"b1","completed_at":"2026-11-08T10:00:00Z"}],
            "user": {"id":1,"username":"user1","balance":18000,"frozen_balance":0,
                     "total_recharged":18000,"total_charged":0,"email":"u@t.test","status":1},
            "snapshot": {"balance":"18.000","available_balance":"18.000","frozen_balance":"0.000",
                         "scan_recharge":"18.000","card_recharge":"0.000","award_amount":"0.000"},
            "user_keys": [], "api_key": "ak_x", "api_keys": [], "recent": [], "mail_records": [],
            "providers": [{"id":"outlook","name":"Outlook","pooled":true,"ephemeral":false,
                           "config_fields":[{"key":"proxy","label":"代理","type":"text","required":false,
                                             "placeholder":"","help":"","default":""}],
                           "help_text":"","config":{"proxy":""}}],
            "billing_config": {"site_url":"","min_topup":"1.00","max_topup":"1000.00",
                               "card_default_amount":"10.00","epay_url":"","epay_pid":"","epay_key":"",
                               "hold_timeout":"1200","default_price":"0.018","award_percent":"0",
                               "enabled":"true"},
            "health_check_cfg": {"health_check_enabled":"true","health_check_hour":"8","health_check_minute":"0"},
            "health_check_last": {}, "heartbeat_cfg": {"heartbeat_enabled":"false",
                                                       "heartbeat_mode":"silent","heartbeat_hour":"9",
                                                       "heartbeat_minute":"0"},
            "heartbeat_last": {}, "refresh_cfg": {"refresh_enabled":"false","refresh_hour":"3",
                                                   "refresh_minute":"0"},
            "refresh_last": {}, "projects_total": 53, "projects_enabled": 53,
            "error": "", "username": "", "message": "", "ok": "", "next": "", "prev": "",
            "query": "", "keyword": "", "tag": "", "status_filter": "", "project_code": "",
            "email": "", "selected_email": "", "summary": {}, "operations": {}, "logos": [],
            "filter_project": "", "log_type": "", "channels": [],
            "doc_active": "api", "doc_title": "API 文档",
        });

        let names = ["dashboard.html","overview.html","accounts.html","account.html","mailbox.html",
                     "projects.html","pool.html","buy_email.html","orders.html","balances.html",
                     "recharges.html","billing_users.html","billing_cards.html","settings.html",
                     "login.html","user_login.html","docs_base.html",
                     "docs/api-overview.html","docs/quick-start.html","docs/temporary-email.html",
                     "docs/errors.html"];
        let mut failures = Vec::new();
        for n in names {
            match t.render_json(n, &ctx) {
                Ok(html) => {
                    assert!(html.len() > 200, "{n} 渲染结果过短: {} 字节", html.len());
                    assert!(html.contains("<!DOCTYPE html>") || html.contains("<html"),
                            "{n} 不是完整 HTML");
                }
                Err(e) => failures.push(format!("{n}: {e}")),
            }
        }
        assert!(failures.is_empty(), "以下模板渲染失败:\n{}", failures.join("\n"));
    }

    #[test]
    fn strftime_works_through_render_json() {
        // 端到端验证：JSON 里的 `*_at` 字段经 render_json 后能调 .strftime()
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap().join("app").join("web").join("templates");
        if !dir.exists() { return; }

        let mut env = Environment::new();
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Chainable);
        env.add_template("t", "{{ r.created_at.strftime('%Y-%m-%d') }}").unwrap();
        let ctx = serde_json::json!({"r": {"created_at": "2026-11-08T10:23:45Z"}});
        let v = build_value(&ctx);
        let out = env.get_template("t").unwrap().render(v).unwrap();
        assert_eq!(out, "2026-11-08");
    }

    #[test]
    fn urlencode_matches_python_quote() {
        // 对齐 urllib.parse.quote(s, safe='')
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a@b.test"), "a%40b.test");
        assert_eq!(urlencode("x/y"), "x%2Fy");
        assert_eq!(urlencode("abc-_.~"), "abc-_.~");
        // 中文按 UTF-8 字节
        assert_eq!(urlencode("中"), "%E4%B8%AD");
    }

    #[test]
    fn all_templates_compile() {
        // 这是整个 Rust 重写的**门禁测试**：22 个 Jinja 模板必须全部
        // 能在 minijinja 下原样编译，否则「复用模板」的方案就不成立。
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("app")
            .join("web")
            .join("templates");
        if !dir.exists() {
            eprintln!("模板目录不存在，跳过: {}", dir.display());
            return;
        }
        let names = collect_template_names(&dir);
        assert!(names.len() >= 20, "模板数量异常: {}", names.len());
        Templates::load(&dir).expect("全部模板必须能在 minijinja 下编译");
    }

    #[test]
    fn datetime_value_strftime() {
        use chrono::NaiveDate;
        let dt = NaiveDate::from_ymd_opt(2026, 11, 8)
            .unwrap()
            .and_hms_opt(10, 23, 45)
            .unwrap();
        let v = DateTimeValue::from_naive(Some(dt)).to_value();
        // 通过模板实际调用，验证方法真的可达
        let mut env = Environment::new();
        env.add_template("t", "{{ d.strftime('%Y-%m-%d %H:%M:%S') }}")
            .unwrap();
        let mut ctx = std::collections::BTreeMap::new();
        ctx.insert("d", v);
        let out = env.get_template("t").unwrap().render(&ctx).unwrap();
        assert_eq!(out, "2026-11-08 10:23:45");
    }

    #[test]
    fn datetime_none_renders_empty() {
        let v = DateTimeValue::from_naive(None).to_value();
        let mut env = Environment::new();
        env.add_template("t", "[{{ d }}]{% if d %}set{% else %}unset{% endif %}")
            .unwrap();
        let mut ctx = std::collections::BTreeMap::new();
        ctx.insert("d", v);
        let out = env.get_template("t").unwrap().render(&ctx).unwrap();
        // 空值既渲染为空串，也在 if 里判为假（模板大量依赖 `if r.created_at`）
        assert_eq!(out, "[]unset");
    }
    use super::*;

    #[test]
    fn format_filter_three_decimals() {
        assert_eq!(format_filter("%.3f".into(), Value::from(0.5)), "0.500");
        assert_eq!(format_filter("%.3f".into(), Value::from(18)), "18.000");
        assert_eq!(format_filter("%.2f".into(), Value::from(1.234)), "1.23");
    }

    #[test]
    fn format_filter_ints_and_strings() {
        assert_eq!(format_filter("%d".into(), Value::from(42)), "42");
        assert_eq!(format_filter("%s".into(), Value::from("hi")), "hi");
    }

    #[test]
    fn money_filter_converts() {
        assert_eq!(money_filter(Value::from(18)), "0.018");
        assert_eq!(money_filter(Value::from(1000)), "1.000");
    }

    #[test]
    fn strftime_on_iso() {
        assert_eq!(
            strftime_filter(Value::from("2026-11-08T10:23:00Z"), "%Y-%m-%d".into()),
            "2026-11-08"
        );
    }
}
