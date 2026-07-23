//! Persisted client screening config — the single source of truth for what the
//! server screens with.
//!
//! Phase 1 has exactly one client, so there is one stored config: the client's
//! `subscribe` overwrites it, it survives restarts on disk, and everything
//! server-side (spread sampler, terminal signal logger, REST summary, WS
//! sessions that subscribe without a config) reads from here rather than from
//! the static `default_client` in the TOML. The TOML value remains only the
//! bootstrap default for a fresh install.
//!
//! When multi-user auth lands, this becomes a per-user row in a database; the
//! interface (get/set + generation) is deliberately shaped so call sites won't
//! change.

use screener::ClientConfig;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tracing::{info, warn};

pub struct ConfigStore {
    path: PathBuf,
    /// Generation increments on every successful `set`, so long-lived loops
    /// (the terminal signal logger's engine) can cheaply detect a change and
    /// rebuild their derived state.
    inner: RwLock<(u64, Arc<ClientConfig>)>,
}

impl ConfigStore {
    /// Open the store: use the config persisted at `path` when present and
    /// valid, otherwise fall back to `bootstrap` (the TOML default). A corrupt
    /// or invalid file is logged and ignored, never fatal.
    pub fn load_or(path: PathBuf, bootstrap: ClientConfig) -> Self {
        let cfg = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<ClientConfig>(&text) {
                Ok(cfg) => match cfg.validate() {
                    Ok(()) => {
                        info!(path = %path.display(), "loaded persisted client config");
                        cfg
                    }
                    Err(e) => {
                        warn!(path = %path.display(), error = %e, "persisted config invalid; using bootstrap default");
                        bootstrap
                    }
                },
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "persisted config unreadable; using bootstrap default");
                    bootstrap
                }
            },
            Err(_) => bootstrap, // first run: no file yet
        };
        ConfigStore {
            path,
            inner: RwLock::new((0, Arc::new(cfg))),
        }
    }

    /// The current config.
    pub fn get(&self) -> Arc<ClientConfig> {
        self.inner.read().expect("config lock").1.clone()
    }

    /// Current generation and config in one consistent read.
    pub fn snapshot(&self) -> (u64, Arc<ClientConfig>) {
        let g = self.inner.read().expect("config lock");
        (g.0, g.1.clone())
    }

    /// Validate, adopt, and persist a client-supplied config. On success the
    /// whole server screens with it from the next tick. Returns the adopted
    /// config (what `subscribed` echoes back).
    pub fn set(&self, cfg: ClientConfig) -> Result<Arc<ClientConfig>, String> {
        cfg.validate()?;
        let cfg = Arc::new(cfg);
        {
            let mut g = self.inner.write().expect("config lock");
            g.0 += 1;
            g.1 = cfg.clone();
        }
        // Persistence is best-effort: a full disk must not block screening.
        // The config is still live in memory; only restart durability is lost.
        if let Err(e) = self.persist(&cfg) {
            warn!(path = %self.path.display(), error = %e, "failed to persist client config");
        }
        Ok(cfg)
    }

    fn persist(&self, cfg: &ClientConfig) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        // Write-then-rename so a crash mid-write can't leave a torn file.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(cfg)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("arb_cfg_store_{name}_{}.json", std::process::id()))
    }

    #[test]
    fn set_persists_and_reload_restores() {
        let path = tmp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let store = ConfigStore::load_or(path.clone(), ClientConfig::default());
        let mut cfg = ClientConfig::default();
        cfg.min_net_spread_pct = dec!(0.042);
        store.set(cfg).expect("valid config");

        // A fresh store (server restart) must come back with the client's
        // config, not the bootstrap default.
        let reloaded = ConfigStore::load_or(path.clone(), ClientConfig::default());
        assert_eq!(reloaded.get().min_net_spread_pct, dec!(0.042));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_set_is_rejected_and_not_stored() {
        let path = tmp_path("invalid");
        let _ = std::fs::remove_file(&path);

        let store = ConfigStore::load_or(path.clone(), ClientConfig::default());
        let (gen0, _) = store.snapshot();
        let mut bad = ClientConfig::default();
        bad.exchanges.clear();
        assert!(store.set(bad).is_err());
        assert_eq!(store.snapshot().0, gen0, "generation must not advance");
        assert!(!path.exists(), "invalid config must not be persisted");
    }

    #[test]
    fn corrupt_file_falls_back_to_bootstrap() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        let store = ConfigStore::load_or(path.clone(), ClientConfig::default());
        assert_eq!(store.get().quote, "USDT");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn generation_advances_on_each_set() {
        let path = tmp_path("gen");
        let _ = std::fs::remove_file(&path);
        let store = ConfigStore::load_or(path.clone(), ClientConfig::default());
        let g0 = store.snapshot().0;
        store.set(ClientConfig::default()).unwrap();
        store.set(ClientConfig::default()).unwrap();
        assert_eq!(store.snapshot().0, g0 + 2);
        let _ = std::fs::remove_file(&path);
    }
}
