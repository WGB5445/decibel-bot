//! Advisory process locks per `(network, subaccount)` pair.
//!
//! Two independent locks prevent different race scenarios:
//!
//! - [`SubaccountStartupLock`]: held by `start` from preflight through the engine's first Ping,
//!   so a second `start` cannot race the spawn. Released when `start` exits.
//! - [`SubaccountRunLock`]: held by the engine, shadow mode, and TUI for the process lifetime,
//!   preventing two live traders from bulk-submitting for the same subaccount.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

fn lock_dir() -> Result<PathBuf> {
    let base = std::env::var_os("DECIBEL_GRID_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_local_dir().or_else(dirs::data_dir))
        .ok_or_else(|| anyhow::anyhow!("could not determine the local data directory"))?;
    let lock_dir = base.join("decibel-grid").join("locks");
    std::fs::create_dir_all(&lock_dir)
        .with_context(|| format!("could not create lock directory {}", lock_dir.display()))?;
    Ok(lock_dir)
}

fn subaccount_lock_path(network: &str, subaccount: &str, suffix: &str) -> Result<PathBuf> {
    use sha3::{Digest, Sha3_256};

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
    Ok(lock_dir()?.join(format!("subaccount-{digest}{suffix}")))
}

fn acquire_lock(path: &Path, held_message: &str) -> Result<fslock::LockFile> {
    let mut lock = fslock::LockFile::open(path)
        .with_context(|| format!("could not open subaccount lock {}", path.display()))?;
    if !lock
        .try_lock()
        .with_context(|| format!("could not acquire subaccount lock {}", path.display()))?
    {
        anyhow::bail!("{held_message}");
    }
    Ok(lock)
}

pub struct SubaccountStartupLock {
    // Held for the lifetime of the struct; the lock is released when this field is dropped.
    #[allow(dead_code)]
    lock: fslock::LockFile,
}

impl SubaccountStartupLock {
    /// Acquire the short-lived startup lock used by `start` to serialize spawn races.
    pub fn acquire(network: &str, subaccount: &str) -> Result<Self> {
        let path = subaccount_lock_path(network, subaccount, ".startup.lock")?;
        let lock = acquire_lock(
            &path,
            &format!(
                "another grid start is already in progress for network {} and this subaccount; wait for it to finish or stop the other start",
                network
            ),
        )?;
        Ok(Self { lock })
    }
}

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
        let path = subaccount_lock_path(network, subaccount, ".lock")?;
        let lock = acquire_lock(
            &path,
            &format!(
                "another grid process is already running for network {} and this subaccount; stop it before starting a second instance",
                network
            ),
        )?;
        Ok(Self { lock })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, atomic::{AtomicU64, Ordering}};

    use super::*;

    static LOCK_TEST_MUTEX: Mutex<()> = Mutex::new(());
    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn with_temp_lock_dir<F: FnOnce()>(test: F) {
        let _guard = LOCK_TEST_MUTEX.lock().unwrap_or_else(|err| err.into_inner());
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = std::env::temp_dir().join(format!(
            "decibel-grid-lock-test-{}-{}",
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&temp).expect("create temp lock dir");
        let previous = std::env::var_os("DECIBEL_GRID_DATA_DIR");
        // SAFETY: test runs single-threaded and restores the prior value before returning.
        unsafe { std::env::set_var("DECIBEL_GRID_DATA_DIR", &temp) };

        test();

        match previous {
            Some(value) => unsafe { std::env::set_var("DECIBEL_GRID_DATA_DIR", value) },
            None => unsafe { std::env::remove_var("DECIBEL_GRID_DATA_DIR") },
        }
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn double_acquire_same_subaccount_fails() {
        with_temp_lock_dir(|| {
            let network = "testnet";
            let subaccount = "0xabc123";
            let first = SubaccountRunLock::acquire(network, subaccount).expect("first acquire");
            let second = SubaccountRunLock::acquire(network, subaccount);
            assert!(second.is_err(), "second acquire should fail while first is held");
            drop(first);
        });
    }

    #[test]
    fn double_acquire_startup_lock_fails() {
        with_temp_lock_dir(|| {
            let network = "testnet";
            let subaccount = "0xabc123";
            let first =
                SubaccountStartupLock::acquire(network, subaccount).expect("first acquire");
            let second = SubaccountStartupLock::acquire(network, subaccount);
            assert!(second.is_err(), "second acquire should fail while first is held");
            drop(first);
        });
    }

    #[test]
    fn startup_and_run_locks_are_independent() {
        with_temp_lock_dir(|| {
            let network = "testnet";
            let subaccount = "0xabc123";
            let startup =
                SubaccountStartupLock::acquire(network, subaccount).expect("startup acquire");
            let run = SubaccountRunLock::acquire(network, subaccount).expect("run acquire");
            drop(startup);
            drop(run);
        });
    }
}
