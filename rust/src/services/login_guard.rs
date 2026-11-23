//! 登录失败限流（防暴力破解）。
//!
//! ## 为什么必须有
//!
//! `/login`（后台口令）和 `/app/login`（客户账号）原来都没有任何频率限制，
//! 可以无限次尝试。实测环境里 `zhoushu`/`zs`/后台口令用的都是同一个弱密码，
//! 一旦对外就是现成的爆破目标。
//!
//! ## 限流口径
//!
//! | 入口 | 键 | 窗口 | 上限 |
//! |---|---|---|---|
//! | `/login`（后台口令） | 客户端 IP | 10 分钟 | 10 次 |
//! | `/app/login`（客户账号） | IP + 用户名 | 15 分钟 | 5 次 |
//!
//! **只统计失败**：成功登录会立即清空该键的计数，正常用户不会因为
//! 偶尔打错几次而被锁。
//!
//! `/app/login` 用 IP+用户名 而不是只按 IP：
//! - 只按 IP → 同一 NAT 后的多个用户互相拖累
//! - 只按用户名 → 换用户名就能绕过
//!
//! 两者都记才挡得住「同一 IP 撞同一账号」和「同一账号换 IP 撞」。
//!
//! ## 为什么放内存
//!
//! 单实例部署，进程内 `HashMap` 足够；重启清零是可接受的（攻击者
//! 拿不到重启时机）。落库要多一张表 + 清理任务，收益不匹配。
//! 将来多实例时改 Redis 即可，接口不变。

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 一次限流判定结果。
#[derive(Debug, Clone)]
pub enum Verdict {
    /// 放行
    Allow,
    /// 拒绝，附带还需等待的秒数
    Deny { retry_after: u64 },
}

struct Bucket {
    /// 失败时间点（滑动窗口）
    fails: VecDeque<Instant>,
}

impl Bucket {
    fn new() -> Self {
        Self {
            fails: VecDeque::new(),
        }
    }

    /// 丢掉窗口外的记录。
    fn prune(&mut self, window: Duration) {
        let now = Instant::now();
        while let Some(t) = self.fails.front() {
            if now.duration_since(*t) > window {
                self.fails.pop_front();
            } else {
                break;
            }
        }
    }

    /// 还要等多久才能重试（窗口内最早那条记录过期时）。
    fn retry_after(&self, window: Duration) -> u64 {
        let now = Instant::now();
        match self.fails.front() {
            Some(t) => {
                let elapsed = now.duration_since(*t);
                window.saturating_sub(elapsed).as_secs().max(1)
            }
            None => 0,
        }
    }
}

static LIMITER: OnceLock<Mutex<HashMap<String, Bucket>>> = OnceLock::new();

fn map() -> &'static Mutex<HashMap<String, Bucket>> {
    LIMITER.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 检查是否被限流（不消耗配额）。
///
/// 在**验证密码之前**调用：已锁定的直接拒绝，不再浪费一次密码校验。
pub fn check(key: &str, window: Duration, max_fails: usize) -> Verdict {
    let mut m = map().lock().unwrap();
    let b = m.entry(key.to_string()).or_insert_with(Bucket::new);
    b.prune(window);
    if b.fails.len() >= max_fails {
        Verdict::Deny {
            retry_after: b.retry_after(window),
        }
    } else {
        Verdict::Allow
    }
}

/// 记一次失败。
pub fn fail(key: &str) {
    let mut m = map().lock().unwrap();
    let b = m.entry(key.to_string()).or_insert_with(Bucket::new);
    b.fails.push_back(Instant::now());
}

/// 成功登录后清空计数。
pub fn clear(key: &str) {
    let mut m = map().lock().unwrap();
    m.remove(key);
}

/// 定期清理空桶，防止长期运行内存缓慢增长。
///
/// 由调度器每分钟调一次；没有调用也不影响正确性（只是少释放一点内存）。
pub fn gc() {
    let mut m = map().lock().unwrap();
    let long_window = Duration::from_secs(3600);
    m.retain(|_, b| {
        b.prune(long_window);
        !b.fails.is_empty()
    });
}

// ── 各入口的限流参数 ───────────────────────────────────────────

/// 后台口令：同一 IP 10 分钟内最多失败 10 次。
pub const ADMIN_WINDOW: Duration = Duration::from_secs(600);
pub const ADMIN_MAX: usize = 10;

/// 客户账号：同一 IP + 用户名 15 分钟内最多失败 5 次。
pub const USER_WINDOW: Duration = Duration::from_secs(900);
pub const USER_MAX: usize = 5;

/// 客户登录的限流键（IP + 用户名）。
pub fn user_key(ip: &str, username: &str) -> String {
    format!("u:{}:{}", ip, username.trim().to_ascii_lowercase())
}

/// 后台口令的限流键（IP）。
pub fn admin_key(ip: &str) -> String {
    format!("a:{ip}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 这些测试共享全局 map，用一把锁串行化
    static T: Mutex<()> = Mutex::new(());
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        T.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn allows_until_limit_then_denies() {
        let _g = lock();
        let k = "test:allow";
        clear(k);
        for _ in 0..USER_MAX {
            assert!(matches!(check(k, USER_WINDOW, USER_MAX), Verdict::Allow));
            fail(k);
        }
        match check(k, USER_WINDOW, USER_MAX) {
            Verdict::Deny { retry_after } => assert!(retry_after > 0),
            Verdict::Allow => panic!("达到上限后必须拒绝"),
        }
        clear(k);
    }

    #[test]
    fn success_clears_counter() {
        let _g = lock();
        let k = "test:clear";
        clear(k);
        for _ in 0..USER_MAX {
            fail(k);
        }
        assert!(matches!(check(k, USER_WINDOW, USER_MAX), Verdict::Deny { .. }));
        clear(k);
        assert!(
            matches!(check(k, USER_WINDOW, USER_MAX), Verdict::Allow),
            "成功登录后应恢复"
        );
    }

    #[test]
    fn keys_are_isolated() {
        let _g = lock();
        let k1 = user_key("1.1.1.1", "alice");
        let k2 = user_key("1.1.1.1", "bob");
        let k3 = user_key("2.2.2.2", "alice");
        for k in [&k1, &k2, &k3] {
            clear(k);
        }
        for _ in 0..USER_MAX {
            fail(&k1);
        }
        assert!(matches!(check(&k1, USER_WINDOW, USER_MAX), Verdict::Deny { .. }));
        assert!(
            matches!(check(&k2, USER_WINDOW, USER_MAX), Verdict::Allow),
            "换用户名不该被牵连"
        );
        assert!(
            matches!(check(&k3, USER_WINDOW, USER_MAX), Verdict::Allow),
            "换 IP 不该被牵连"
        );
        for k in [&k1, &k2, &k3] {
            clear(k);
        }
    }

    #[test]
    fn user_key_normalizes_case_and_space() {
        assert_eq!(user_key("1.2.3.4", "  Alice "), user_key("1.2.3.4", "alice"));
    }

    #[test]
    fn window_expiry_allows_again() {
        let _g = lock();
        let k = "test:expire";
        clear(k);
        // 用一个极短窗口验证过期逻辑
        let w = Duration::from_millis(50);
        for _ in 0..3 {
            fail(k);
        }
        assert!(matches!(check(k, w, 3), Verdict::Deny { .. }));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            matches!(check(k, w, 3), Verdict::Allow),
            "窗口过期后应放行"
        );
        clear(k);
    }
}
