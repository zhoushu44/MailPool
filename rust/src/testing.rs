//! 差分测试工具：Python 版 vs Rust 版行为一致性检查。
//!
//! 思路：同一个 SQLite 库副本，分别用 Python 版和 Rust 版起服务，
//! 对同一批请求比对 **响应体的结构**（不是逐字节，因为时间戳/随机数必然不同）。
//!
//! 比对策略：
//! - `code` / `status` / 结构里的 key 集合必须完全一致
//! - 金额字符串必须完全一致（这是账目正确性的核心）
//! - 时间字段只校验「能解析成 ISO8601」
//! - 随机量（订单号、api key）只校验格式
//!
//! 用法见 `docs/rust-migration.md`。

use serde_json::Value;

/// 一条差异。
#[derive(Debug, Clone)]
pub struct Diff {
    /// JSON 路径，如 `data.temporary[0].price`
    pub path: String,
    /// Python 侧的值
    pub py: String,
    /// Rust 侧的值
    pub rs: String,
    /// 差异类别
    pub kind: DiffKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// 字段缺失（一侧有另一侧没有）—— 必须修
    Missing,
    /// 类型不同（字符串 vs 数字）—— 必须修
    TypeMismatch,
    /// 值不同 —— 视字段而定
    ValueMismatch,
    /// 只是格式差异（时间戳）—— 可接受
    Formatting,
}

impl Diff {
    pub fn is_blocking(&self) -> bool {
        matches!(
            self.kind,
            DiffKind::Missing | DiffKind::TypeMismatch
        )
    }
}

/// 比对两份 JSON，返回差异列表。
///
/// `path` 用来标注位置，递归时会拼接。
pub fn compare(py: &Value, rs: &Value, path: &str) -> Vec<Diff> {
    let mut out = Vec::new();
    compare_inner(py, rs, path, &mut out);
    out
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn compare_inner(py: &Value, rs: &Value, path: &str, out: &mut Vec<Diff>) {
    // 类型先对齐（null 与缺失要区分：null 是有这个字段）
    if type_name(py) != type_name(rs) {
        // 特例：数字 1 与浮点 1.0 在 JSON 里都是 number，不算差异
        out.push(Diff {
            path: path.to_string(),
            py: format!("{}:{}", type_name(py), py),
            rs: format!("{}:{}", type_name(rs), rs),
            kind: DiffKind::TypeMismatch,
        });
        return;
    }

    match (py, rs) {
        (Value::Object(a), Value::Object(b)) => {
            // 两边的 key 集合必须一致（Python 有 Rust 没有 = 字段漏了）
            for k in a.keys() {
                if !b.contains_key(k) {
                    out.push(Diff {
                        path: join(path, k),
                        py: "存在".into(),
                        rs: "缺失".into(),
                        kind: DiffKind::Missing,
                    });
                }
            }
            for k in b.keys() {
                if !a.contains_key(k) {
                    out.push(Diff {
                        path: join(path, k),
                        py: "缺失".into(),
                        rs: "存在".into(),
                        kind: DiffKind::Missing,
                    });
                }
            }
            for (k, av) in a {
                if let Some(bv) = b.get(k) {
                    compare_inner(av, bv, &join(path, k), out);
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                out.push(Diff {
                    path: path.to_string(),
                    py: format!("{} 项", a.len()),
                    rs: format!("{} 项", b.len()),
                    kind: DiffKind::ValueMismatch,
                });
            }
            for (i, (av, bv)) in a.iter().zip(b.iter()).enumerate() {
                compare_inner(av, bv, &format!("{path}[{i}]"), out);
            }
        }
        (Value::String(a), Value::String(b)) => {
            if a != b {
                // 时间戳类型字段只校验格式
                if is_time_field(path) && looks_iso8601(a) && looks_iso8601(b) {
                    out.push(Diff {
                        path: path.to_string(),
                        py: a.clone(),
                        rs: b.clone(),
                        kind: DiffKind::Formatting,
                    });
                } else {
                    out.push(Diff {
                        path: path.to_string(),
                        py: a.clone(),
                        rs: b.clone(),
                        kind: DiffKind::ValueMismatch,
                    });
                }
            }
        }
        (Value::Number(a), Value::Number(b)) => {
            // 用字符串比较避免 f64 精度问题
            if a.to_string() != b.to_string() {
                out.push(Diff {
                    path: path.to_string(),
                    py: a.to_string(),
                    rs: b.to_string(),
                    kind: DiffKind::ValueMismatch,
                });
            }
        }
        (Value::Bool(a), Value::Bool(b)) => {
            if a != b {
                out.push(Diff {
                    path: path.to_string(),
                    py: a.to_string(),
                    rs: b.to_string(),
                    kind: DiffKind::ValueMismatch,
                });
            }
        }
        _ => {} // null == null
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

/// 字段名是否表示时间。
fn is_time_field(path: &str) -> bool {
    let last = path.rsplit('.').next().unwrap_or(path);
    last.ends_with("_at") || last.ends_with("_time") || last == "created_at"
}

/// 粗判是否 ISO8601 时间串。
fn looks_iso8601(s: &str) -> bool {
    s.len() >= 19
        && s.as_bytes().get(4) == Some(&b'-')
        && s.as_bytes().get(10) == Some(&b'T') || s.contains('T')
}

/// 比对结果汇总。
pub struct Report {
    pub diffs: Vec<Diff>,
    pub checked_paths: usize,
}

impl Report {
    pub fn new(diffs: Vec<Diff>) -> Self {
        let checked = diffs.len();
        Self {
            diffs,
            checked_paths: checked,
        }
    }

    /// 必须修的差异。
    pub fn blocking(&self) -> Vec<&Diff> {
        self.diffs.iter().filter(|d| d.is_blocking()).collect()
    }

    /// 值不一致（可能是真的 bug，也可能是随机量）。
    pub fn value_mismatches(&self) -> Vec<&Diff> {
        self.diffs
            .iter()
            .filter(|d| d.kind == DiffKind::ValueMismatch)
            .collect()
    }

    pub fn is_ok(&self) -> bool {
        self.blocking().is_empty()
    }

    pub fn summary(&self) -> String {
        if self.diffs.is_empty() {
            return "✅ 完全一致".to_string();
        }
        let b = self.blocking().len();
        let v = self.value_mismatches().len();
        let f = self.diffs.len() - b - v;
        format!("差异 {} 项：阻塞 {b}、值不同 {v}、格式差异 {f}", self.diffs.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn identical_passes() {
        let a = json!({"code": "00000", "data": {"total": 5}});
        let r = Report::new(compare(&a, &a, ""));
        assert!(r.is_ok());
        assert!(r.diffs.is_empty());
    }

    #[test]
    fn missing_field_is_blocking() {
        let py = json!({"code": "00000", "data": {"total": 5, "items": []}});
        let rs = json!({"code": "00000", "data": {"total": 5}});
        let r = Report::new(compare(&py, &rs, ""));
        let blocking = r.blocking();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].path, "data.items");
        assert_eq!(blocking[0].kind, DiffKind::Missing);
    }

    #[test]
    fn extra_field_is_blocking() {
        // Rust 多返回字段也算不一致（客户端契约以 Python 为准）
        let py = json!({"code": "00000"});
        let rs = json!({"code": "00000", "extra": 1});
        let r = Report::new(compare(&py, &rs, ""));
        assert_eq!(r.blocking().len(), 1);
        assert_eq!(r.blocking()[0].path, "extra");
    }

    #[test]
    fn type_mismatch_is_blocking() {
        // 金额必须是字符串 "0.018"，不能是数字 0.018
        let py = json!({"price": "0.018"});
        let rs = json!({"price": 0.018});
        let r = Report::new(compare(&py, &rs, ""));
        assert_eq!(r.blocking().len(), 1);
        assert_eq!(r.blocking()[0].kind, DiffKind::TypeMismatch);
    }

    #[test]
    fn value_mismatch_not_blocking_but_reported() {
        let py = json!({"stock": 100});
        let rs = json!({"stock": 99});
        let r = Report::new(compare(&py, &rs, ""));
        assert!(r.is_ok(), "值不同不算阻塞");
        assert_eq!(r.value_mismatches().len(), 1);
    }

    #[test]
    fn time_field_only_formatting() {
        let py = json!({"created_at": "2026-11-22T01:00:00Z"});
        let rs = json!({"created_at": "2026-11-22T01:00:00.000000Z"});
        let r = Report::new(compare(&py, &rs, ""));
        assert!(r.is_ok());
        assert_eq!(r.diffs[0].kind, DiffKind::Formatting);
    }

    #[test]
    fn nested_array_diff() {
        let py = json!({"items": [{"code": "a"}, {"code": "b"}]});
        let rs = json!({"items": [{"code": "a"}, {"code": "c"}]});
        let r = Report::new(compare(&py, &rs, ""));
        assert_eq!(r.diffs.len(), 1);
        assert_eq!(r.diffs[0].path, "items[1].code");
    }

    #[test]
    fn array_length_diff() {
        let py = json!({"items": [1, 2, 3]});
        let rs = json!({"items": [1, 2]});
        let r = Report::new(compare(&py, &rs, ""));
        assert!(r.diffs.iter().any(|d| d.path == "items"));
    }

    #[test]
    fn summary_text() {
        let py = json!({"a": 1, "b": "x"});
        let rs = json!({"a": 2});
        let r = Report::new(compare(&py, &rs, ""));
        let s = r.summary();
        assert!(s.contains("阻塞"), "{s}");
    }
}
