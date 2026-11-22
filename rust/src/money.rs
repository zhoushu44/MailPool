//! 金额类型：整数厘（1 元 = 1000 厘）。
//!
//! 为什么不用浮点：`0.018 * 1000 = 17.999999999999996`，直接截断会少 1 厘；
//! float 累加会产生 `0.018000000000000002` 这类误差，余额对不上账。
//! 整数厘加法零误差，且 3 位小数精度足够（MailNest 的 0.018/0.038 正好 3 位）。
//!
//! 对齐 Python `app/services/billing_service.py` 的 `to_milli` / `fmt_milli`：
//! - `to_milli`: 元 → 厘，ROUND_HALF_UP（必须，不能四舍六入五成双）
//! - `fmt_milli`: 厘 → 元，固定 3 位小数

use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 1 元 = 1000 厘
pub const MONEY_SCALE: i64 = 1000;

/// 金额，单位为「厘」。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, Hash,
)]
#[serde(transparent)]
pub struct Money(pub i64);

impl Money {
    pub const ZERO: Money = Money(0);

    pub const fn from_milli(v: i64) -> Self {
        Money(v)
    }

    pub const fn milli(self) -> i64 {
        self.0
    }

    /// 元 → 厘，ROUND_HALF_UP。
    ///
    /// 实现上不走浮点：把十进制字符串拆成「整数部分 + 小数部分」手工换算，
    /// 只有小数位超过 3 位时才需要按第 4 位四舍五入。
    pub fn parse_yuan(value: &str) -> Option<Self> {
        let s = value.trim();
        if s.is_empty() {
            return None;
        }
        let (neg, s) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        // 允许 1_000 这样的下划线？Python Decimal 不接受，这里也不接受。
        let s = s.trim();
        if s.is_empty() {
            return None;
        }

        let (int_part, frac_part) = match s.split_once('.') {
            Some((i, f)) => (i, f),
            None => (s, ""),
        };
        // 只允许数字（空整数部分按 0 处理，如 ".5"）
        if !int_part.chars().all(|c| c.is_ascii_digit())
            || !frac_part.chars().all(|c| c.is_ascii_digit())
        {
            return None;
        }
        if int_part.is_empty() && frac_part.is_empty() {
            return None;
        }

        let int_val: i64 = if int_part.is_empty() {
            0
        } else {
            int_part.parse().ok()?
        };

        // 小数部分补/截到 3 位
        let mut frac = [0u8; 3];
        let fbytes = frac_part.as_bytes();
        for i in 0..3 {
            frac[i] = fbytes.get(i).map(|b| b - b'0').unwrap_or(0);
        }
        // 第 4 位决定进位（ROUND_HALF_UP）
        let round_up = fbytes.get(3).map(|b| *b >= b'5').unwrap_or(false);

        let mut milli = int_val
            .checked_mul(MONEY_SCALE)?
            .checked_add(frac[0] as i64 * 100 + frac[1] as i64 * 10 + frac[2] as i64)?;
        if round_up {
            milli = milli.checked_add(1)?;
        }
        Some(Money(if neg { -milli } else { milli }))
    }

    /// 厘 → 元字符串，固定 3 位小数（对齐 Python `fmt_milli`）。
    pub fn to_yuan(self) -> String {
        let v = self.0;
        let sign = if v < 0 { "-" } else { "" };
        let a = v.unsigned_abs();
        format!(
            "{}{}.{:03}",
            sign,
            a / MONEY_SCALE as u64,
            a % MONEY_SCALE as u64
        )
    }

    /// 乘以数量（负数数量返回 None，防溢出）。
    pub fn checked_mul_qty(self, qty: u32) -> Option<Self> {
        self.0.checked_mul(qty as i64).map(Money)
    }

    pub fn is_positive(self) -> bool {
        self.0 > 0
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl FromStr for Money {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Money::parse_yuan(s).ok_or(())
    }
}

impl From<i64> for Money {
    fn from(v: i64) -> Self {
        Money(v)
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_yuan())
    }
}

impl Add for Money {
    type Output = Money;
    fn add(self, rhs: Money) -> Money {
        Money(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Money {
    type Output = Money;
    fn sub(self, rhs: Money) -> Money {
        Money(self.0.saturating_sub(rhs.0))
    }
}

impl Neg for Money {
    type Output = Money;
    fn neg(self) -> Money {
        Money(-self.0)
    }
}

impl AddAssign for Money {
    fn add_assign(&mut self, rhs: Money) {
        self.0 = self.0.saturating_add(rhs.0);
    }
}

impl SubAssign for Money {
    fn sub_assign(&mut self, rhs: Money) {
        self.0 = self.0.saturating_sub(rhs.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_three_decimals() {
        assert_eq!(Money::parse_yuan("0.018").unwrap().milli(), 18);
        assert_eq!(Money::parse_yuan("1.00").unwrap().milli(), 1000);
        assert_eq!(Money::parse_yuan("0.038").unwrap().milli(), 38);
        assert_eq!(Money::parse_yuan("10").unwrap().milli(), 10_000);
        assert_eq!(Money::parse_yuan("-.5").unwrap().milli(), -500);
        assert_eq!(Money::parse_yuan(".5").unwrap().milli(), 500);
    }

    #[test]
    fn round_half_up_not_bankers() {
        // 关键回归：0.0005 元 = 0.5 厘，ROUND_HALF_UP → 1 厘（银行家舍入会得 0）
        assert_eq!(Money::parse_yuan("0.0005").unwrap().milli(), 1);
        assert_eq!(Money::parse_yuan("0.0004").unwrap().milli(), 0);
        // 浮点会错的经典值
        assert_eq!(Money::parse_yuan("0.018").unwrap().milli(), 18);
    }

    #[test]
    fn fmt_three_decimals() {
        assert_eq!(Money::from_milli(18).to_yuan(), "0.018");
        assert_eq!(Money::from_milli(1000).to_yuan(), "1.000");
        assert_eq!(Money::from_milli(0).to_yuan(), "0.000");
        assert_eq!(Money::from_milli(-1500).to_yuan(), "-1.500");
        assert_eq!(Money::from_milli(5).to_yuan(), "0.005");
        assert_eq!(Money::from_milli(1_000_000).to_yuan(), "1000.000");
    }

    #[test]
    fn reject_garbage() {
        assert!(Money::parse_yuan("").is_none());
        assert!(Money::parse_yuan("abc").is_none());
        assert!(Money::parse_yuan("1.2.3").is_none());
        assert!(Money::parse_yuan("1e5").is_none());
    }

    #[test]
    fn add_is_exact() {
        let a = Money::from_milli(18);
        let mut acc = Money::ZERO;
        for _ in 0..1000 {
            acc += a;
        }
        assert_eq!(acc.milli(), 18_000); // 浮点这里会是 17999.999999999996
    }
}
