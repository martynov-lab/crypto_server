//! Hysteresis + dedup for signals (spec §8.4). Prevents a single opportunity
//! from spamming events while still re-alerting when the spread widens
//! meaningfully, and resets cleanly so the *next* widening fires again.

use domain::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Emit a signal and record the new peak.
    Emit,
    /// Suppress — either below threshold, or not enough new widening.
    Suppress,
}

/// Per-(client, instrument) hysteresis state: the last emitted net-spread peak.
#[derive(Debug, Clone, Default)]
pub struct PeakState {
    pub peak: Option<Decimal>,
}

impl PeakState {
    /// Decide whether to emit given the current `net` spread.
    ///
    /// - below/at threshold      → reset peak, suppress
    /// - above, no prior peak    → emit, set peak
    /// - above, widened by >step → emit, raise peak
    /// - above, but not widened  → suppress (avoid ratchet spam)
    pub fn decide(&mut self, net: Decimal, threshold: Decimal, step: Decimal) -> Decision {
        if net <= threshold {
            self.peak = None;
            return Decision::Suppress;
        }
        match self.peak {
            None => {
                self.peak = Some(net);
                Decision::Emit
            }
            Some(peak) if peak + step < net => {
                self.peak = Some(net);
                Decision::Emit
            }
            Some(_) => Decision::Suppress,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn t() -> Decimal {
        dec!(0.02)
    }
    fn step() -> Decimal {
        dec!(0.005)
    }

    #[test]
    fn first_cross_emits() {
        let mut s = PeakState::default();
        assert_eq!(s.decide(dec!(0.03), t(), step()), Decision::Emit);
        assert_eq!(s.peak, Some(dec!(0.03)));
    }

    #[test]
    fn no_widening_suppresses() {
        let mut s = PeakState::default();
        s.decide(dec!(0.03), t(), step());
        // 0.03 + tiny, less than peak+step (0.035) => suppress.
        assert_eq!(s.decide(dec!(0.032), t(), step()), Decision::Suppress);
        assert_eq!(s.peak, Some(dec!(0.03)));
    }

    #[test]
    fn sufficient_widening_reemits() {
        let mut s = PeakState::default();
        s.decide(dec!(0.03), t(), step());
        assert_eq!(s.decide(dec!(0.04), t(), step()), Decision::Emit);
        assert_eq!(s.peak, Some(dec!(0.04)));
    }

    #[test]
    fn drop_below_threshold_resets_then_reemits() {
        let mut s = PeakState::default();
        s.decide(dec!(0.03), t(), step());
        // Falls below threshold: reset.
        assert_eq!(s.decide(dec!(0.01), t(), step()), Decision::Suppress);
        assert_eq!(s.peak, None);
        // Next crossing emits again (no ratchet lock-up).
        assert_eq!(s.decide(dec!(0.031), t(), step()), Decision::Emit);
    }
}
