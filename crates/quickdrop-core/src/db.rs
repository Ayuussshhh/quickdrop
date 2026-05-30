//! Embedded persistence (sled).
//!
//! Filled in Step 3 (TrustStore) and Step 8 (transfer state).
//! Holding the placeholder here so module wiring stays stable.

use std::path::Path;

use crate::Result;

#[derive(Debug, Clone)]
pub struct Db {
    pub inner: sled::Db,
}

impl Db {
    pub fn open(dir: &Path) -> Result<Self> {
        let inner = sled::open(dir)?;
        Ok(Self { inner })
    }
}
