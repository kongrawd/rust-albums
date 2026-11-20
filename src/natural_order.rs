//! The order albums and items are listed in: as people count, so `IMG_9`
//! comes before `IMG_10` and `lake` before `Trip`.
//!
//! Written here on `str` alone: natord and lexical-sort are unmaintained,
//! alphanumeric-sort minds case, and ICU collation (icu_collator) brings far
//! more data than file names need.

use std::cmp::Ordering;

/// Compares `a` and `b` naturally: runs of ASCII digits by their value
/// (leading zeros ignored), everything else by character, ignoring case.
/// Names equal that way fall back to plain string order, so the order is
/// total and the same on every scan.
pub fn compare(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a, b);
    loop {
        let ordering = match (x.chars().next(), y.chars().next()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let (m, rest_x) = leading_number(x);
                let (n, rest_y) = leading_number(y);
                (x, y) = (rest_x, rest_y);
                // Without leading zeros, a longer number is larger.
                m.len().cmp(&n.len()).then_with(|| m.cmp(n))
            }
            (Some(c), Some(d)) => {
                (x, y) = (&x[c.len_utf8()..], &y[d.len_utf8()..]);
                c.to_lowercase().cmp(d.to_lowercase())
            }
        };
        if ordering.is_ne() {
            return ordering;
        }
    }
}

/// The run of ASCII digits `s` starts with, without its leading zeros, and
/// the rest of `s`.
fn leading_number(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, rest) = s.split_at(end);
    (digits.trim_start_matches('0'), rest)
}
