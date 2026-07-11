//! Optional CSV sink for emitted signals — feeds the lifetime/analysis question
//! from spec §8.5 ("does a self-hosted SaaS actually have time to execute?").
//! Disabled unless a path is configured.

use screener::ScreenerEvent;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use tracing::warn;

pub struct CsvSink {
    writer: Mutex<BufWriter<File>>,
}

impl CsvSink {
    /// Open (creating + writing a header if new) a CSV file for appending.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let is_new = !path.exists();
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut w = BufWriter::new(file);
        if is_new {
            writeln!(
                w,
                "ts_ms,instrument,buy_exchange,sell_exchange,gross_pct,net_pct,executable_notional,capped_by_depth,funding_diff_apr"
            )?;
            w.flush()?;
        }
        Ok(CsvSink {
            writer: Mutex::new(w),
        })
    }

    /// Append one event. Best-effort: logs and continues on write failure.
    pub fn record(&self, ev: &ScreenerEvent) {
        let funding = ev
            .funding
            .as_ref()
            .map(|f| f.diff_apr.to_string())
            .unwrap_or_default();
        let s = &ev.spread;
        let line = format!(
            "{},{}/{},{},{},{},{},{},{},{}",
            ev.ts_ms,
            s.instrument.base,
            s.instrument.quote,
            s.buy_exchange,
            s.sell_exchange,
            s.gross_pct,
            s.net_pct,
            s.executable_notional,
            s.capped_by_depth,
            funding,
        );
        if let Ok(mut w) = self.writer.lock() {
            if let Err(e) = writeln!(w, "{line}").and_then(|_| w.flush()) {
                warn!(error = %e, "failed to write signal to CSV");
            }
        }
    }
}
