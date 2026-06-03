//! Worker pid → run-id registry.
//!
//! When the engine spawns a worker (claude in a leased cube workspace),
//! it knows the worker process's pid and the run id it was spawned for.
//! The events socket accepts hook deliveries from the `boss-event`
//! shim, which runs as a *descendant* of the worker process — so peer
//! pid is the shim's pid, not the worker's.
//!
//! This registry stores the (worker_pid, run_id) pairs the spawn flow
//! creates, and looks them up via an ancestor walk: given the shim's
//! peer pid, walk up the process tree until we hit a registered
//! ancestor. That's the run the hook event belongs to.
//!
//! macOS-only walk via `proc_pidinfo` for reliability — `getppid()`
//! is per-process and would require IPC.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
use std::os::raw::c_void;

const ANCESTOR_WALK_DEPTH: usize = 8;

/// First slot id reserved for remote workers' virtual slots.
///
/// A remote worker holds no libghostty pane, so the spawn flow never
/// allocates a real pool slot for it. But the live-status surface is
/// slot-keyed, so the dispatcher assigns each live remote run a synthetic
/// slot from a high range that cannot collide with the local worker pool
/// (`1..=MAX_WORKER_POOL_SIZE`, currently 8) or the automation pool
/// (`MAX_WORKER_POOL_SIZE+1..`, currently 9..=11). 200 leaves generous
/// headroom below for pool growth and 56 ids above for concurrent remote
/// runs — far more than any real deployment. See
/// [`WorkerRegistry::get_or_allocate_remote_slot`].
pub const REMOTE_SLOT_BASE: u8 = 200;

#[derive(Clone, Default)]
pub struct WorkerRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

#[derive(Default)]
struct RegistryInner {
    pid_to_run: HashMap<libc::pid_t, String>,
    run_to_slot: HashMap<String, u8>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that pid is the worker process for run `run_id`. Called by
    /// the spawn flow once it has both the run id and the spawned pid.
    pub fn register(&self, pid: libc::pid_t, run_id: impl Into<String>) {
        self.inner
            .lock()
            .expect("registry poisoned")
            .pid_to_run
            .insert(pid, run_id.into());
    }

    /// Record the libghostty slot id the engine asked the app to host
    /// `run_id` in. The engine uses this to route follow-up
    /// `SendToPane` requests by run id.
    pub fn register_run_slot(&self, run_id: impl Into<String>, slot_id: u8) {
        self.inner
            .lock()
            .expect("registry poisoned")
            .run_to_slot
            .insert(run_id.into(), slot_id);
    }

    /// Look up the slot id for `run_id`. Returns `None` if the run
    /// has not been registered (or has been released).
    pub fn slot_for_run(&self, run_id: &str) -> Option<u8> {
        self.inner
            .lock()
            .expect("registry poisoned")
            .run_to_slot
            .get(run_id)
            .copied()
    }

    /// Get the slot mapped to `run_id`, allocating a virtual remote slot
    /// if none exists.
    ///
    /// Remote workers carry no libghostty pane, so the spawn flow never
    /// calls [`Self::register_run_slot`] for them and [`Self::slot_for_run`]
    /// returns `None` — which makes `dispatch_live_worker_state` drop their
    /// hook fan-out and leaves the live-status surface blank for remote
    /// runs. The dispatcher calls this for a slotless run it has identified
    /// as a live remote worker: it returns the existing mapping when one is
    /// present, otherwise allocates the lowest free slot in
    /// `[REMOTE_SLOT_BASE, u8::MAX]` and records it under the same
    /// `run_to_slot` map the local path uses, so every downstream lookup
    /// (`slot_for_run`, `take_slot_for_run`) works identically.
    ///
    /// Returns `(slot_id, freshly_allocated)`. `freshly_allocated` is
    /// `true` only on the first call for a run, so the caller stamps the
    /// initial `LiveWorkerState` exactly once. Returns `None` only when
    /// every slot in the remote range is already mapped — the caller then
    /// drops the event as it would have before.
    pub fn get_or_allocate_remote_slot(&self, run_id: &str) -> Option<(u8, bool)> {
        let mut inner = self.inner.lock().expect("registry poisoned");
        if let Some(slot) = inner.run_to_slot.get(run_id) {
            return Some((*slot, false));
        }
        let slot = {
            let used = &inner.run_to_slot;
            (REMOTE_SLOT_BASE..=u8::MAX).find(|s| !used.values().any(|v| v == s))
        }?;
        inner.run_to_slot.insert(run_id.to_owned(), slot);
        Some((slot, true))
    }

    /// Atomically remove and return the slot id for `run_id`. The
    /// release-pane flow uses this for idempotency — duplicate release
    /// attempts (e.g. completion-detection plus a manual stop firing
    /// for the same run) see `None` on the second call and skip.
    pub fn take_slot_for_run(&self, run_id: &str) -> Option<u8> {
        self.inner
            .lock()
            .expect("registry poisoned")
            .run_to_slot
            .remove(run_id)
    }

    /// Drop the entry for `pid`. Called when a run terminates and the
    /// worker process exits.
    pub fn unregister(&self, pid: libc::pid_t) -> Option<String> {
        let mut inner = self.inner.lock().expect("registry poisoned");
        let run_id = inner.pid_to_run.remove(&pid);
        if let Some(rid) = run_id.as_deref() {
            inner.run_to_slot.remove(rid);
        }
        run_id
    }

    /// Direct pid lookup — no ancestor walk. Returns `None` if the pid
    /// is not a registered worker.
    pub fn lookup(&self, pid: libc::pid_t) -> Option<String> {
        self.inner
            .lock()
            .expect("registry poisoned")
            .pid_to_run
            .get(&pid)
            .cloned()
    }

    /// Walk up the process tree from `pid` until we find a registered
    /// ancestor or hit the depth limit. The shim runs as a descendant
    /// of the worker (claude → shell? → boss-event), so this is how
    /// hook events are correlated to runs.
    pub fn lookup_with_ancestor_walk(&self, mut pid: libc::pid_t) -> Option<String> {
        for _ in 0..ANCESTOR_WALK_DEPTH {
            if let Some(run_id) = self.lookup(pid) {
                return Some(run_id);
            }
            pid = match parent_pid(pid) {
                Ok(Some(parent)) => parent,
                Ok(None) | Err(_) => return None,
            };
        }
        None
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("registry poisoned")
            .pid_to_run
            .len()
    }

    /// Snapshot of every registered worker shell pid. Used by the
    /// engine's RPC auth to decide "is this peer a worker?" — peers
    /// whose process tree descends from one of these pids are workers,
    /// not the Boss session.
    pub fn registered_pids(&self) -> Vec<libc::pid_t> {
        self.inner
            .lock()
            .expect("registry poisoned")
            .pid_to_run
            .keys()
            .copied()
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Look up the parent pid of `pid` via macOS `proc_pidinfo`. Returns
/// `Ok(None)` when `pid` is the root of the process tree (parent is
/// 0 / launchd).
#[cfg(target_os = "macos")]
pub fn parent_pid(pid: libc::pid_t) -> io::Result<Option<libc::pid_t>> {
    // proc_bsdinfo struct layout per <sys/proc_info.h> on Darwin.
    // We read PROC_PIDTBSDINFO (flavor 3) and pull pbi_ppid from it.
    const PROC_PIDTBSDINFO: libc::c_int = 3;

    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [u8; 16],
        pbi_name: [u8; 32],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    unsafe extern "C" {
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    let mut info = ProcBsdInfo::default();
    let info_size = std::mem::size_of::<ProcBsdInfo>() as libc::c_int;
    // SAFETY: passing a valid &mut to proc_pidinfo, sized correctly.
    let n = unsafe {
        proc_pidinfo(
            pid as libc::c_int,
            PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            info_size,
        )
    };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    if (n as usize) < std::mem::size_of::<ProcBsdInfo>() {
        return Err(io::Error::other(
            format!("proc_pidinfo returned {n} bytes; expected {info_size}"),
        ));
    }

    let ppid = info.pbi_ppid as libc::pid_t;
    if ppid == 0 {
        Ok(None)
    } else {
        Ok(Some(ppid))
    }
}

#[cfg(not(target_os = "macos"))]
pub fn parent_pid(_pid: libc::pid_t) -> io::Result<Option<libc::pid_t>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "parent_pid is only implemented on macOS",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_lookup() {
        let reg = WorkerRegistry::new();
        reg.register(1234, "run-abc");
        assert_eq!(reg.lookup(1234).as_deref(), Some("run-abc"));
        assert_eq!(reg.lookup(9999), None);
    }

    #[test]
    fn unregister_removes_and_returns_old_value() {
        let reg = WorkerRegistry::new();
        reg.register(42, "run-1");
        assert_eq!(reg.unregister(42).as_deref(), Some("run-1"));
        assert_eq!(reg.lookup(42), None);
    }

    #[test]
    fn unregister_unknown_pid_returns_none() {
        let reg = WorkerRegistry::new();
        assert_eq!(reg.unregister(123), None);
    }

    #[test]
    fn len_and_is_empty_track_state() {
        let reg = WorkerRegistry::new();
        assert!(reg.is_empty());
        reg.register(1, "a");
        reg.register(2, "b");
        assert_eq!(reg.len(), 2);
        reg.unregister(1);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn take_slot_for_run_is_idempotent() {
        let reg = WorkerRegistry::new();
        reg.register_run_slot("run-x", 4);
        assert_eq!(reg.take_slot_for_run("run-x"), Some(4));
        // Second call returns None — that's the idempotency guard the
        // release path leans on.
        assert_eq!(reg.take_slot_for_run("run-x"), None);
        // Unregistered run is also None.
        assert_eq!(reg.take_slot_for_run("never-registered"), None);
    }

    #[test]
    fn registry_clones_share_state() {
        let reg = WorkerRegistry::new();
        let clone = reg.clone();
        reg.register(7, "shared");
        assert_eq!(clone.lookup(7).as_deref(), Some("shared"));
        clone.unregister(7);
        assert!(reg.is_empty());
    }

    #[test]
    fn remote_slot_allocation_is_idempotent_per_run() {
        let reg = WorkerRegistry::new();
        let (slot_a, fresh_a) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
        assert!(fresh_a, "first allocation must report freshly_allocated");
        assert!(slot_a >= REMOTE_SLOT_BASE, "remote slot must come from the reserved range");
        // Second call for the same run returns the same slot, not fresh.
        let (slot_again, fresh_again) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
        assert_eq!(slot_again, slot_a);
        assert!(!fresh_again);
        // slot_for_run now resolves it like any other registered run.
        assert_eq!(reg.slot_for_run("exec-a"), Some(slot_a));
    }

    #[test]
    fn remote_slots_are_distinct_and_avoid_local_pool_range() {
        let reg = WorkerRegistry::new();
        // Occupy a couple of local-pool slots first.
        reg.register_run_slot("local-1", 1);
        reg.register_run_slot("local-9", 9);
        let (a, _) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
        let (b, _) = reg.get_or_allocate_remote_slot("exec-b").unwrap();
        assert_ne!(a, b, "concurrent remote runs must get distinct slots");
        for s in [a, b] {
            assert!(s >= REMOTE_SLOT_BASE, "remote slot {s} collided with the pool range");
        }
    }

    #[test]
    fn remote_slot_reuses_lowest_free_after_release() {
        let reg = WorkerRegistry::new();
        let (a, _) = reg.get_or_allocate_remote_slot("exec-a").unwrap();
        assert_eq!(a, REMOTE_SLOT_BASE);
        let (b, _) = reg.get_or_allocate_remote_slot("exec-b").unwrap();
        assert_eq!(b, REMOTE_SLOT_BASE + 1);
        // Release the first run's slot; the next allocation reuses it.
        assert_eq!(reg.take_slot_for_run("exec-a"), Some(REMOTE_SLOT_BASE));
        let (c, fresh) = reg.get_or_allocate_remote_slot("exec-c").unwrap();
        assert_eq!(c, REMOTE_SLOT_BASE);
        assert!(fresh);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parent_pid_of_self_is_some() {
        let self_pid = std::process::id() as libc::pid_t;
        let parent = parent_pid(self_pid).unwrap();
        assert!(parent.is_some(), "expected a parent pid for the test process");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ancestor_walk_finds_self_pid_when_registered() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        reg.register(self_pid, "self-run");
        // Walking up from self should hit self immediately.
        assert_eq!(
            reg.lookup_with_ancestor_walk(self_pid).as_deref(),
            Some("self-run")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ancestor_walk_finds_parent_when_only_parent_registered() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        let Some(parent) = parent_pid(self_pid).unwrap() else {
            // No parent? Skip.
            return;
        };
        reg.register(parent, "parent-run");
        // Walking up from self should reach parent.
        assert_eq!(
            reg.lookup_with_ancestor_walk(self_pid).as_deref(),
            Some("parent-run")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ancestor_walk_returns_none_when_no_ancestor_registered() {
        let reg = WorkerRegistry::new();
        let self_pid = std::process::id() as libc::pid_t;
        assert_eq!(reg.lookup_with_ancestor_walk(self_pid), None);
    }
}
