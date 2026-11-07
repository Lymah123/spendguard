use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// An amount of SOL in lamports. All arithmetic is checked: the only way to
/// combine two amounts is `checked_add` / `checked_sub`, so overflow can never
/// silently wrap past a spending limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Lamports(u64);

impl Lamports {
    pub const ZERO: Lamports = Lamports(0);

    pub const fn new(lamports: u64) -> Self {
        Self(lamports)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }

    /// Parses decimal SOL amounts such as `"0.1"`, `"0.1 SOL"`, `" 2 sol "`,
    /// `".5"`. Exact decimal parsing, no floats. Rejects negatives, exponents,
    /// separators, more than 9 decimal places, and anything that overflows u64.
    pub fn parse_sol(input: &str) -> Result<Self> {
        let bad = || Error::InvalidAmount(input.to_string());

        let mut s = input.trim();
        if s.len() >= 3
            && s.is_char_boundary(s.len() - 3)
            && s[s.len() - 3..].eq_ignore_ascii_case("sol")
        {
            s = s[..s.len() - 3].trim_end();
        }
        if s.is_empty() {
            return Err(bad());
        }

        let (whole, frac) = match s.split_once('.') {
            Some((_, "")) => return Err(bad()), // "1." is rejected
            Some((w, f)) => (w, f),
            None => (s, ""),
        };

        let all_digits = |x: &str| x.bytes().all(|b| b.is_ascii_digit());
        if !all_digits(whole) || !all_digits(frac) || frac.len() > 9 {
            return Err(bad());
        }

        let whole_n: u64 = if whole.is_empty() {
            0
        } else {
            whole.parse().map_err(|_| bad())?
        };
        let frac_n: u64 = if frac.is_empty() {
            0
        } else {
            format!("{frac:0<9}").parse().map_err(|_| bad())?
        };

        whole_n
            .checked_mul(LAMPORTS_PER_SOL)
            .and_then(|w| w.checked_add(frac_n))
            .map(Self)
            .ok_or_else(bad)
    }
}

impl fmt::Display for Lamports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.0 / LAMPORTS_PER_SOL;
        let frac = self.0 % LAMPORTS_PER_SOL;
        if frac == 0 {
            write!(f, "{whole} SOL")
        } else {
            let frac = format!("{frac:09}");
            write!(f, "{whole}.{} SOL", frac.trim_end_matches('0'))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(
            Lamports::parse_sol("0.1 SOL").unwrap().as_u64(),
            100_000_000
        );
        assert_eq!(Lamports::parse_sol("1").unwrap().as_u64(), 1_000_000_000);
        assert_eq!(
            Lamports::parse_sol(" 2.5 sol ").unwrap().as_u64(),
            2_500_000_000
        );
        assert_eq!(Lamports::parse_sol(".5").unwrap().as_u64(), 500_000_000);
        assert_eq!(Lamports::parse_sol("0.000000001").unwrap().as_u64(), 1);
        assert_eq!(Lamports::parse_sol("0").unwrap(), Lamports::ZERO);
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            " ",
            "sol",
            "-1",
            "+1",
            "1e3",
            "1,000",
            "0x10",
            "abc",
            "1.",
            ".",
            "1.2.3",
            "1.0000000001", // 10 decimals
            "1 SOLS",
            "SOL 1",
            "１", // fullwidth digit
        ] {
            assert!(Lamports::parse_sol(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn rejects_overflow() {
        assert!(Lamports::parse_sol("18446744073709551615 SOL").is_err());
        // largest whole-SOL value that still fits in u64 lamports
        assert!(Lamports::parse_sol("18446744073 SOL").is_ok());
        assert!(Lamports::parse_sol("18446744074 SOL").is_err());
    }

    #[test]
    fn checked_math_does_not_wrap() {
        let max = Lamports::new(u64::MAX);
        assert_eq!(max.checked_add(Lamports::new(1)), None);
        assert_eq!(Lamports::ZERO.checked_sub(Lamports::new(1)), None);
        assert_eq!(
            Lamports::new(5).checked_add(Lamports::new(7)),
            Some(Lamports::new(12))
        );
    }

    #[test]
    fn display_round_trips() {
        for s in ["0.1 SOL", "1 SOL", "2.5 SOL", "0.000000001 SOL"] {
            let l = Lamports::parse_sol(s).unwrap();
            assert_eq!(l.to_string(), s);
        }
    }
}
