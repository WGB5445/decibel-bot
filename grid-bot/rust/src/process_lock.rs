//! Advisory process lock per `(network, subaccount)` pair.

use std::path::PathBuf;

use anyhow::{Context, Result};

pub struct SubaccountRunLock {
    // Held for the lifetime of the struct; the lock is released when this field is dropped.
    #[allow(dead_code)]
    lock: fslock::LockFile,
}

impl SubaccountRunLock {
    /// Acquire one non-blocking process lock per network/subaccount. The lock is intentionally
    /// independent of market so two processes cannot race bulk sequence numbers or funding for
    /// different markets on the same subaccount.
    pub fn acquire(network: &str, subaccount: &str) -> Result<Self> {
        use sha3::{Digest, Sha3_256};

        let base = std::env::var_os("DECIBEL_GRID_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().or_else(dirs::data_dir))
            .ok_or_else(|| anyhow::anyhow!("could not determine the local data directory"))?;
        let lock_dir = base.join("decibel-grid").join("locks");
        std::fs::create_dir_all(&lock_dir)
            .with_context(|| format!("could not create lock directory {}", lock_dir.display()))?;
        let canonical_subaccount = subaccount
            .trim()
            .trim_start_matches("0x")
            .trim_start_matches('0')
            .to_ascii_lowercase();
        let key = format!(
            "{}:{}",
            network.trim().to_ascii_lowercase(),
            canonical_subaccount
        );
        let digest = hex::encode(Sha3_256::digest(key.as_bytes()));
        let path = lock_dir.join(format!("subaccount-{digest}.lock"));
        let mut lock = fslock::LockFile::open(&path)
            .with_context(|| format!("could not open subaccount lock {}", path.display()))?;
        if !lock
            .try_lock()
            .with_context(|| format!("could not acquire subaccount lock {}", path.display()))?
        {
            anyhow::bail!(
                "another grid process is already running for network {} and this subaccount; stop it before starting a second instance",
                network
            )
        }
        Ok(Self { lock })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_acquire_same_subaccount_fails() {
        let temp = std::env::temp_dir().join(format!(
            "decibel-grid-lock-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp).expect("create temp lock dir");
        let previous = std::env::var_os("DECIBEL_GRID_DATA_DIR");
        // SAFETY: test runs single-threaded and restores the prior value before returning.
        unsafe { std::env::set_var("DECIBEL_GRID_DATA_DIR", &temp) };

        let network = "testnet";
        let subaccount = "0xabc123";
        let first = SubaccountRunLock::acquire(network, subaccount).expect("first acquire");
        let second = SubaccountRunLock::acquire(network, subaccount);
        assert!(second.is_err(), "second acquire should fail while first is held");
        drop(first);

        match previous {
            Some(value) => unsafe { std::env::set_var("DECIBEL_GRID_DATA_DIR", value) },
            None => unsafe { std::env::remove_var("DECIBEL_GRID_DATA_DIR") },
        }
        let _ = std::fs::remove_dir_all(temp);
    }
}
