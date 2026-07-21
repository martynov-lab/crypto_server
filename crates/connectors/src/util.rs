//! Small parsing helpers shared by connector implementations.

use domain::{BookLevel, Decimal};
use serde_json::Value;
use std::str::FromStr;

/// Parse a `Decimal` from a JSON value that may be a string ("123.45") or a
/// number (123.45). Returns `None` for anything else or unparsable input.
pub fn dec_from_json(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => Decimal::from_str(s).ok(),
        Value::Number(n) => Decimal::from_str(&n.to_string()).ok(),
        _ => None,
    }
}

/// Parse a `Decimal` from a plain string, tolerant of surrounding whitespace.
pub fn dec_from_str(s: &str) -> Option<Decimal> {
    Decimal::from_str(s.trim()).ok()
}

/// Build a `[price, qty]` level from a JSON array element like `["100.5","2.0"]`.
pub fn level_from_pair(pair: &Value) -> Option<BookLevel> {
    let arr = pair.as_array()?;
    let price = dec_from_json(arr.first()?)?;
    let qty = dec_from_json(arr.get(1)?)?;
    Some(BookLevel::new(price, qty))
}

/// Sort/trim helper: keep at most `n` levels. `bids` should already be built
/// best-first (desc price); `asks` best-first (asc price). This defensively
/// re-sorts in case the venue delivers unsorted partial books.
pub fn finalize_sides(mut bids: Vec<BookLevel>, mut asks: Vec<BookLevel>, n: usize) -> (Vec<BookLevel>, Vec<BookLevel>) {
    bids.sort_by_key(|b| std::cmp::Reverse(b.price));
    asks.sort_by_key(|a| a.price);
    bids.truncate(n);
    asks.truncate(n);
    (bids, asks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    #[test]
    fn parses_string_and_number() {
        assert_eq!(dec_from_json(&json!("1.5")), Some(dec!(1.5)));
        assert_eq!(dec_from_json(&json!(2)), Some(dec!(2)));
        assert_eq!(dec_from_json(&json!(true)), None);
    }

    #[test]
    fn levels_sorted_and_trimmed() {
        let bids = vec![
            BookLevel::new(dec!(99), dec!(1)),
            BookLevel::new(dec!(101), dec!(1)),
            BookLevel::new(dec!(100), dec!(1)),
        ];
        let asks = vec![
            BookLevel::new(dec!(103), dec!(1)),
            BookLevel::new(dec!(102), dec!(1)),
        ];
        let (b, a) = finalize_sides(bids, asks, 2);
        assert_eq!(b[0].price, dec!(101));
        assert_eq!(b.len(), 2);
        assert_eq!(a[0].price, dec!(102));
    }
}
