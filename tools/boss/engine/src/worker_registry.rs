//! Worker pid → run-id registry.
//!
//! When the engine spawns a worker (claude in a leased cube workspace),
//! it knows the worker process's pid and the run id it was spawned for.
//! The events socket (Phase 6c) accepts hook deliveries from the
//! `boss-event` shim, which runs as a *descendant* of the worker
//! process — so peer pid is the shim's pid, not the worker's.
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

#[derive(Clone, Default)]
pub struct WorkerRegistry {
    inner: Arc<Mutex<HashMap<libc::pid_t, String>>>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that pid is the worker process for run `run_id`. Called by
    /// the spawn flow once it has both the run id and the spawned pid.
    pub fn register(&self, pid: libc::pid_t, run_id: impl Into<String>) {
        self.inner.lock().expect("registry poisoned").insert(pid, run_id.into());
    }

    /// Drop the entry for `pid`. Called when a run terminates and the
    /// worker process exits.
    pub fn unregister(&self, pid: libc::pid_t) -> Option<String> {
        self.inner.lock().expect("registry poisoned").remove(&pid)
    }

    /// Direct pid lookup — no ancestor walk. Returns `None` if the pid
    /// is not a registered worker.
    pub fn lookup(&self, pid: libc::pid_t) -> Option<String> {
        self.inner.lock().expect("registry poisoned").get(&pid).cloned()
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
        self.inner.lock().expect("registry poisoned").len()
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
        return Err(io::Error::new(
            io::ErrorKind::Other,
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
    fn registry_clones_share_state() {
        let reg = WorkerRegistry::new();
        let clone = reg.clone();
        reg.register(7, "shared");
        assert_eq!(clone.lookup(7).as_deref(), Some("shared"));
        clone.unregister(7);
        assert!(reg.is_empty());
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
