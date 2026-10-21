//! Logical data types, scalar values and date helpers.

use std::cmp::Ordering;
use std::fmt;

/// The SQL types supported by the engine.
///
/// `Null` is the type of an untyped `NULL` literal and only exists during
/// binding; `Interval` only appears as a literal operand of date arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Null,
    Boolean,
    Int64,
    Float64,
    Utf8,
    Date,
    Interval,
}

impl DataType {
    pub fn is_numeric(self) -> bool {
        matches!(self, DataType::Int64 | DataType::Float64)
    }

    /// Short single-letter code used by sqllogictest-style `query` headers.
    pub fn slt_code(self) -> char {
        match self {
            DataType::Int64 | DataType::Boolean => 'I',
            DataType::Float64 => 'R',
            _ => 'T',
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DataType::Null => "NULL",
            DataType::Boolean => "BOOLEAN",
            DataType::Int64 => "BIGINT",
            DataType::Float64 => "DOUBLE",
            DataType::Utf8 => "VARCHAR",
            DataType::Date => "DATE",
            DataType::Interval => "INTERVAL",
        };
        f.write_str(s)
    }
}

/// A single typed value. `Null` carries the type it was coerced to.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Null(DataType),
    Boolean(bool),
    Int64(i64),
    Float64(f64),
    Utf8(String),
    /// Days since 1970-01-01.
    Date(i32),
    Interval {
        months: i32,
        days: i32,
    },
}

impl ScalarValue {
    pub fn data_type(&self) -> DataType {
        match self {
            ScalarValue::Null(t) => *t,
            ScalarValue::Boolean(_) => DataType::Boolean,
            ScalarValue::Int64(_) => DataType::Int64,
            ScalarValue::Float64(_) => DataType::Float64,
            ScalarValue::Utf8(_) => DataType::Utf8,
            ScalarValue::Date(_) => DataType::Date,
            ScalarValue::Interval { .. } => DataType::Interval,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, ScalarValue::Null(_))
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            ScalarValue::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            ScalarValue::Int64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            ScalarValue::Int64(v) => Some(*v as f64),
            ScalarValue::Float64(v) => Some(*v),
            _ => None,
        }
    }

    /// Total order used for sorting/min/max of scalars (NULLs compare first).
    pub fn total_cmp(&self, other: &ScalarValue) -> Ordering {
        use ScalarValue::*;
        match (self, other) {
            (Null(_), Null(_)) => Ordering::Equal,
            (Null(_), _) => Ordering::Less,
            (_, Null(_)) => Ordering::Greater,
            (Boolean(a), Boolean(b)) => a.cmp(b),
            (Int64(a), Int64(b)) => a.cmp(b),
            (Float64(a), Float64(b)) => a.total_cmp(b),
            (Int64(a), Float64(b)) => (*a as f64).total_cmp(b),
            (Float64(a), Int64(b)) => a.total_cmp(&(*b as f64)),
            (Utf8(a), Utf8(b)) => a.cmp(b),
            (Date(a), Date(b)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }
}

impl fmt::Display for ScalarValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScalarValue::Null(_) => f.write_str("NULL"),
            ScalarValue::Boolean(b) => write!(f, "{b}"),
            ScalarValue::Int64(v) => write!(f, "{v}"),
            ScalarValue::Float64(v) => f.write_str(&format_float(*v)),
            ScalarValue::Utf8(s) => f.write_str(s),
            ScalarValue::Date(d) => f.write_str(&format_date(*d)),
            ScalarValue::Interval { months, days } => {
                write!(f, "INTERVAL '{months} months {days} days'")
            }
        }
    }
}

/// Human friendly float formatting: at most 4 decimals, trailing zeros trimmed,
/// always at least one decimal digit (so a DOUBLE never looks like an integer).
pub fn format_float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if v != 0.0 && (v.abs() >= 1e17 || v.abs() < 1e-4) {
        return format!("{v:e}");
    }
    let mut s = format!("{v:.4}");
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.push('0');
    }
    if s == "-0.0" {
        s = "0.0".into();
    }
    s
}

// ---------------------------------------------------------------------------
// Date helpers (proleptic Gregorian calendar, days since Unix epoch).
// Algorithms from Howard Hinnant's "chrono-Compatible Low-Level Date Algorithms".
// ---------------------------------------------------------------------------

pub fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = m as i64;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) as i32
}

pub fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = z as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((if m <= 2 { y + 1 } else { y }) as i32, m, d)
}

fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Parse `YYYY-MM-DD` into days since epoch.
pub fn parse_date(s: &str) -> Option<i32> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Add a number of months to a date, clamping the day to the end of the month
/// (so 2024-01-31 + 1 month = 2024-02-29), matching PostgreSQL/DuckDB.
pub fn add_months(days: i32, months: i32) -> i32 {
    let (y, m, d) = civil_from_days(days);
    let total = y as i64 * 12 + (m as i64 - 1) + months as i64;
    let ny = total.div_euclid(12) as i32;
    let nm = (total.rem_euclid(12) + 1) as u32;
    days_from_civil(ny, nm, d.min(days_in_month(ny, nm)))
}

pub fn add_interval(days: i32, months: i32, interval_days: i32) -> i32 {
    add_months(days, months) + interval_days
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_roundtrip() {
        for s in ["1970-01-01", "1992-02-29", "1998-12-01", "2000-03-01", "1969-12-31"] {
            assert_eq!(format_date(parse_date(s).unwrap()), s);
        }
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("1993-02-29"), None);
        assert_eq!(parse_date("hello"), None);
    }

    #[test]
    fn month_arithmetic_clamps() {
        let d = parse_date("2024-01-31").unwrap();
        assert_eq!(format_date(add_months(d, 1)), "2024-02-29");
        assert_eq!(format_date(add_months(d, -2)), "2023-11-30");
        assert_eq!(format_date(add_months(d, 12)), "2025-01-31");
    }

    #[test]
    fn float_formatting() {
        assert_eq!(format_float(3.0), "3.0");
        assert_eq!(format_float(0.1 + 0.2), "0.3");
        assert_eq!(format_float(-2.5), "-2.5");
        assert_eq!(format_float(1e20), "1e20");
    }
}
