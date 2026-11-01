//! A store's configuration, `<store>/store.json` (SPEC §10.10): written
//! atomically, in the canonical form of SPEC §2, never in `index.db`. A
//! missing file or field means its default. Readers never consult it.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const STORE_JSON: &str = "store.json";

/// `store.json`. Fields are declared in sorted order: serialising writes the
/// canonical form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// The read cache's budget (SPEC §9.6), when it is not the default.
    /// Written only when set, so a store that never set it keeps the
    /// `store.json` it had.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfig>,
    pub compression: Compression,
}

/// `store.json`'s `cache` object: `store config --cache-mb <n>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConfig {
    /// The budget in MiB; 0 turns the cache off.
    pub mb: u64,
}

impl Config {
    /// The read cache's budget in bytes: `cache.mb`, or the default.
    pub fn cache_bytes(&self) -> u64 {
        self.cache.map_or(super::cache::DEFAULT_CACHE_BYTES, |c| c.mb.saturating_mul(1024 * 1024))
    }
}

/// `store.json`'s `compression` object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Compression {
    /// The default dictionary id for new blocks, or `None`.
    pub dictionary: Option<String>,
    /// New ledgers are created compressed.
    pub enabled: bool,
    /// The target block size of a migration (and a recompression).
    pub recompress_bytes: u64,
    /// The hot tail's size at which a writer seals.
    pub seal_bytes: u64,
}

impl Default for Compression {
    fn default() -> Self {
        Compression { dictionary: None, enabled: false, recompress_bytes: 1_048_576, seal_bytes: 262_144 }
    }
}

impl Config {
    /// The configuration of the store at `root`: its `store.json`, or the
    /// defaults when there is none.
    pub fn read(root: &Path) -> Result<Config> {
        let p = root.join(STORE_JSON);
        let text = match std::fs::read(&p) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
        };
        let c: Config = serde_json::from_slice(&text).with_context(|| format!("{} is not a store configuration", p.display()))?;
        c.check().with_context(|| p.display().to_string())?;
        Ok(c)
    }

    /// Write it to `<root>/store.json`, atomically, in canonical form.
    pub fn write(&self, root: &Path) -> Result<()> {
        self.check()?;
        let mut text = serde_json::to_vec(self)?;
        text.push(b'\n');
        crate::ledger::write_atomic(&root.join(STORE_JSON), &text)
    }

    fn check(&self) -> Result<()> {
        if self.compression.seal_bytes == 0 {
            bail!("seal_bytes must be at least 1");
        }
        if self.compression.recompress_bytes == 0 {
            bail!("recompress_bytes must be at least 1");
        }
        if let Some(d) = &self.compression.dictionary
            && !super::dicts::is_id(d)
        {
            bail!("dictionary {d:?} is not a dictionary id (64 lowercase hex digits)");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_spelled_as_the_spec_spells_it() {
        assert_eq!(
            serde_json::to_string(&Config::default()).unwrap(),
            r#"{"compression":{"dictionary":null,"enabled":false,"recompress_bytes":1048576,"seal_bytes":262144}}"#
        );
        let partial: Config = serde_json::from_str(r#"{"compression":{"enabled":true}}"#).unwrap();
        assert_eq!(partial.compression, Compression { enabled: true, ..Compression::default() });
        let empty: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Config::default());
        assert_eq!(empty.cache_bytes(), 256 * 1024 * 1024);
        let set = Config { cache: Some(CacheConfig { mb: 0 }), ..Config::default() };
        assert_eq!(
            serde_json::to_string(&set).unwrap(),
            r#"{"cache":{"mb":0},"compression":{"dictionary":null,"enabled":false,"recompress_bytes":1048576,"seal_bytes":262144}}"#
        );
        assert_eq!(serde_json::from_str::<Config>(&serde_json::to_string(&set).unwrap()).unwrap().cache_bytes(), 0);
    }
}
