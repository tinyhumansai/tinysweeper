//! Durable physical-call reservations for live corpus recording.
//!
//! Reservations are append-only by case. A process crash leaves its full
//! reservation charged, so rerunning a recorder cannot silently reset the
//! physical-attempt ceiling.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const AUDIT_FILE: &str = "recording-attempts.json";
const LOCK_FILE: &str = "recording-attempts.lock";

/// Unlocks the audit file even when its descriptor was inherited during a
/// concurrent fork before close-on-exec takes effect. Keep the lock inode in
/// place: unlinking it could let another process lock a different inode.
#[derive(Debug)]
struct AuditLock(File);

impl AuditLock {
    fn acquire(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory).map_err(|err| Error::path(directory, err))?;
        let lock_path = directory.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| Error::path(&lock_path, err))?;
        lock.try_lock()
            .map_err(|err| Error::path(&lock_path, err))?;
        Ok(Self(lock))
    }
}

impl Drop for AuditLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Audit {
    #[serde(default)]
    reservations: Vec<ReservationRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ReservationRecord {
    id: String,
    reserved_calls: usize,
    actual_calls: Option<usize>,
}

/// Holds the per-case audit lock until the review has finalized its call count.
#[derive(Debug)]
pub(crate) struct Reservation {
    directory: PathBuf,
    id: String,
    reserved_calls: usize,
    _lock: AuditLock,
}

impl Reservation {
    /// Reserve at most `requested_calls`, without exceeding the persistent
    /// per-case ceiling. The lock remains held across the review to prevent a
    /// second recorder from racing the same remaining allowance.
    pub(crate) fn begin(
        directory: &Path,
        requested_calls: usize,
        total_limit: usize,
    ) -> Result<Self> {
        let lock = AuditLock::acquire(directory)?;
        Self::reserve_locked(directory, lock, requested_calls, total_limit)
    }

    fn reserve_locked(
        directory: &Path,
        lock: AuditLock,
        requested_calls: usize,
        total_limit: usize,
    ) -> Result<Self> {
        let audit_path = directory.join(AUDIT_FILE);
        let mut audit = read_audit(&audit_path)?;
        let used = audit.reservations.iter().fold(0usize, |used, record| {
            used.saturating_add(record.actual_calls.unwrap_or(record.reserved_calls))
        });
        let available = total_limit.saturating_sub(used);
        let reserved_calls = requested_calls.min(available);
        if reserved_calls == 0 {
            return Err(Error::config(format!(
                "recording physical-call allowance is exhausted ({used}/{total_limit} calls reserved or used)"
            )));
        }

        let id = format!(
            "{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            std::process::id()
        );
        audit.reservations.push(ReservationRecord {
            id: id.clone(),
            reserved_calls,
            actual_calls: None,
        });
        write_audit(&audit_path, &audit, &id)?;
        Ok(Self {
            directory: directory.to_path_buf(),
            id,
            reserved_calls,
            _lock: lock,
        })
    }

    pub(crate) fn reserved_calls(&self) -> usize {
        self.reserved_calls
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// Record observed committed dispatches. An impossible overrun stays in
    /// the audit and is returned as a hard error instead of being clamped.
    pub(crate) fn finish(self, actual_calls: usize) -> Result<()> {
        let path = self.directory.join(AUDIT_FILE);
        let mut audit = read_audit(&path)?;
        let record = audit
            .reservations
            .iter_mut()
            .find(|record| record.id == self.id)
            .ok_or_else(|| Error::config("recording attempt reservation disappeared"))?;
        record.actual_calls = Some(actual_calls);
        write_audit(&path, &audit, &self.id)?;
        if actual_calls > self.reserved_calls {
            return Err(Error::config(format!(
                "physical call control overran its reservation: {actual_calls} calls used from {} reserved",
                self.reserved_calls
            )));
        }
        Ok(())
    }
}

fn read_audit(path: &Path) -> Result<Audit> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw).map_err(Error::from),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Audit::default()),
        Err(err) => Err(Error::path(path, err)),
    }
}

fn write_audit(path: &Path, audit: &Audit, suffix: &str) -> Result<()> {
    let temporary = path.with_extension(format!("json.{suffix}.tmp"));
    let bytes = serde_json::to_vec_pretty(audit)?;
    fs::write(&temporary, bytes).map_err(|err| Error::path(&temporary, err))?;
    fs::rename(&temporary, path).map_err(|err| Error::path(path, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[test]
    fn reservations_count_actual_calls_and_crashes_keep_the_full_reservation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let reservation = Reservation::begin(directory.path(), 8, 8).expect("first reserve");
        assert_eq!(reservation.reserved_calls(), 8);
        reservation.finish(3).expect("write actual count");

        let next = Reservation::begin(directory.path(), 8, 8).expect("remaining reserve");
        assert_eq!(next.reserved_calls(), 5);
        drop(next);

        let exhausted = Reservation::begin(directory.path(), 8, 8)
            .expect_err("dropped reservation remains charged");
        assert!(exhausted.to_string().contains("exhausted"));
    }

    #[test]
    fn an_exhausted_reservation_releases_its_lock_without_refunding_calls() {
        let directory = tempfile::tempdir().expect("tempdir");
        let initial = Reservation::begin(directory.path(), 8, 8).expect("initial reserve");
        initial.finish(8).expect("record all calls");

        let exhausted = Reservation::begin(directory.path(), 1, 8)
            .expect_err("the committed attempt used the full allowance");
        assert!(exhausted.to_string().contains("exhausted"));

        let still_exhausted = Reservation::begin(directory.path(), 1, 8)
            .expect_err("an exhausted request must not strand the audit lock");
        assert!(still_exhausted.to_string().contains("exhausted"));
    }

    #[test]
    fn an_error_path_unlocks_even_if_a_child_inherited_the_audit_handle() {
        let directory = tempfile::tempdir().expect("tempdir");
        Reservation::begin(directory.path(), 8, 8)
            .expect("initial reserve")
            .finish(8)
            .expect("record all calls");

        let lock = AuditLock::acquire(directory.path()).expect("acquire audit lock");
        let inherited_handle = lock.0.try_clone().expect("clone inherited handle");
        let exhausted = Reservation::reserve_locked(directory.path(), lock, 1, 8)
            .expect_err("committed calls exhaust the allowance");
        assert!(exhausted.to_string().contains("exhausted"));

        let lock_after_error = AuditLock::acquire(directory.path())
            .expect("error return explicitly unlocks inherited descriptor");
        drop(lock_after_error);
        drop(inherited_handle);
    }

    #[test]
    fn simultaneous_reservations_fail_closed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = Reservation::begin(directory.path(), 8, 8).expect("first reserve");
        assert!(Reservation::begin(directory.path(), 8, 8).is_err());
        drop(first);
    }

    #[test]
    fn dropping_a_reservation_unlocks_even_when_a_child_inherited_its_handle() {
        let directory = tempfile::tempdir().expect("tempdir");
        let reservation = Reservation::begin(directory.path(), 8, 16).expect("first reserve");
        let inherited_handle = reservation._lock.0.try_clone().expect("clone lock handle");

        drop(reservation);
        let exhausted = Reservation::begin(directory.path(), 1, 8)
            .expect_err("unfinished reservation remains fully charged");
        assert!(exhausted.to_string().contains("exhausted"));

        let next = Reservation::begin(directory.path(), 8, 16)
            .expect("drop releases the lock despite an inherited handle");
        assert_eq!(next.reserved_calls(), 8);
        drop(next);
        drop(inherited_handle);
    }

    #[test]
    fn a_larger_rerun_limit_only_admits_the_persistent_remainder() {
        let directory = tempfile::tempdir().expect("tempdir");
        let initial = Reservation::begin(directory.path(), 8, 56).expect("initial reserve");
        assert_eq!(initial.reserved_calls(), 8);
        initial.finish(8).expect("persist first attempt count");

        let rerun = Reservation::begin(directory.path(), 56, 56).expect("remaining reserve");
        assert_eq!(rerun.reserved_calls(), 48);
        rerun.finish(48).expect("persist rerun attempt count");

        assert!(Reservation::begin(directory.path(), 56, 56).is_err());
        let audit: Audit = read_audit(&directory.path().join(AUDIT_FILE)).expect("audit reads");
        assert_eq!(
            audit
                .reservations
                .iter()
                .map(|record| record.actual_calls.unwrap_or(record.reserved_calls))
                .sum::<usize>(),
            56
        );
    }

    #[test]
    fn terminated_process_releases_lock_but_keeps_its_full_reservation() {
        const CHILD_DIRECTORY: &str = "TINYSWEEPER_RECORDING_BUDGET_CHILD_DIRECTORY";

        let directory = tempfile::tempdir().expect("tempdir");
        let ready_path = directory.path().join("child-ready");
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "eval::recording_budget::tests::reservation_lock_holder_child",
                    "--nocapture",
                ])
                .env(CHILD_DIRECTORY, directory.path())
                .spawn()
                .expect("start lock holder"),
        );
        for _ in 0..500 {
            if ready_path.exists() {
                break;
            }
            if let Some(status) = child.0.try_wait().expect("poll lock holder") {
                panic!("lock holder exited before acquiring its lock ({status})");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready_path.exists(), "lock holder acquired its reservation");

        child.0.kill().expect("terminate lock holder");
        child.0.wait().expect("wait for terminated child");

        let remaining = Reservation::begin(directory.path(), 8, 12)
            .expect("OS releases the process lock after termination");
        assert_eq!(remaining.reserved_calls(), 4);
        remaining.finish(4).expect("record remaining usage");
        assert!(Reservation::begin(directory.path(), 1, 12).is_err());
    }

    #[test]
    fn reservation_lock_holder_child() {
        let Ok(directory) = std::env::var("TINYSWEEPER_RECORDING_BUDGET_CHILD_DIRECTORY") else {
            return;
        };
        let _reservation =
            Reservation::begin(Path::new(&directory), 8, 12).expect("child reservation");
        fs::write(Path::new(&directory).join("child-ready"), b"held")
            .expect("write lock-holder marker");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
