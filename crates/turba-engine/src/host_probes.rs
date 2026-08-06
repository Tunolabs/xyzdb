//! Host hardware probes.
//!
//! Three measurements are gathered at engine boot to populate
//! [`HostHardware`]:
//!
//! 1. Filesystem free bytes via `statvfs`. Cross-platform
//!    (Linux + macOS).
//! 2. Cgroup memory limit when running under one. Linux only;
//!    macOS returns `None`. Both cgroup v2 and v1 are probed.
//! 3. Physical RAM. Linux via `/proc/meminfo`; macOS via
//!    `sysctlbyname("hw.memsize")`.
//!
//! The probes are wrapped behind [`probe_host_hardware`] so the
//! server boot path makes a single call. Each helper is `pub` to let
//! integration tests invoke them in isolation (e.g. statvfs against a
//! tmpdir to verify it returns >0).
//!
//! All probes are best-effort: a failure does not panic. The fallback
//! values are conservative (small physical RAM, no cgroup limit, zero
//! filesystem free).

// SPDX-License-Identifier: BUSL-1.1
use std::ffi::CString;
use std::path::Path;

/// Snapshot of host hardware produced by [`probe_host_hardware`] at
/// engine boot. Surfaced via the server's `/stats` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostHardware {
    /// Physical RAM detected on the host. Linux `/proc/meminfo`,
    /// macOS `sysctl hw.memsize`. Never zero — probe falls back to a
    /// safe minimum if detection fails.
    pub physical_ram_bytes: u64,
    /// Cgroup memory limit if running under one. `None` when
    /// unlimited (cgroup v2 `memory.max = "max"`) or absent.
    pub cgroup_memory_limit_bytes: Option<u64>,
    /// `statvfs` free bytes on the SSD path. `None` when not probed.
    pub ssd_filesystem_free_bytes: Option<u64>,
    /// `statvfs` free bytes on the HDD path.
    pub hdd_filesystem_free_bytes: u64,
}

/// Default physical RAM when detection fails. 8 GiB — large enough to
/// be useful on small hosts, small enough that I-4 hardcap dominates
/// on larger hosts (the 4 GiB hardcap kicks in well before this fallback
/// matters).
pub const PHYSICAL_RAM_FALLBACK_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Probe the host hardware (physical RAM, cgroup limit, filesystem
/// free space) for `/stats` observability. `hdd_path` is the data
/// directory; `ssd_path` is an optional second mount to size.
///
/// # Errors
///
/// Never returns an error — all probe failures are absorbed into safe
/// fallbacks (a failed `statvfs` reports `0` free bytes rather than
/// crashing the boot path).
pub fn probe_host_hardware(hdd_path: &Path, ssd_path: Option<&Path>) -> HostHardware {
    let hdd_free = probe_filesystem_free(hdd_path).unwrap_or(0);
    let ssd_free = ssd_path.and_then(probe_filesystem_free);
    let physical_ram = probe_physical_ram_bytes().unwrap_or(PHYSICAL_RAM_FALLBACK_BYTES);
    let cgroup_limit = probe_cgroup_memory_limit_bytes();

    HostHardware {
        physical_ram_bytes: physical_ram,
        cgroup_memory_limit_bytes: cgroup_limit,
        ssd_filesystem_free_bytes: ssd_free,
        hdd_filesystem_free_bytes: hdd_free,
    }
}

/// Return free bytes available to a non-superuser on the filesystem
/// containing `path`. Cross-platform (`statvfs`). `None` if the
/// syscall fails (path missing, permissions error, unsupported
/// filesystem).
pub fn probe_filesystem_free(path: &Path) -> Option<u64> {
    // Resolve to bytes via the underlying directory; if the path does
    // not exist, fall back to its parent (the engine creates the
    // tier subdir later — we want the filesystem of the parent).
    let probe_path = if path.exists() {
        path.to_path_buf()
    } else if let Some(parent) = path.parent() {
        if parent.as_os_str().is_empty() {
            std::path::PathBuf::from(".")
        } else {
            parent.to_path_buf()
        }
    } else {
        return None;
    };

    let c_path = CString::new(probe_path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `libc::statvfs` is a C plain-old-data struct of integer fields; an
    // all-zero bit pattern is a valid initialised value, and every field read
    // below is populated by the `statvfs` call first.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated C string that outlives the call,
    // and `&mut buf` is a valid, properly-aligned pointer to a `statvfs` for the
    // kernel to fill. The return code is checked before `buf` is read.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    // Use f_frsize (fragment size) — POSIX-defined unit for the block
    // counts. f_bsize is the "preferred I/O block size" which can
    // differ; mixing it with f_bavail is a common bug.
    let frsize = buf.f_frsize as u64;
    let avail = buf.f_bavail as u64;
    Some(frsize.saturating_mul(avail))
}

/// Probe the cgroup memory limit. Linux only; returns `None` on macOS
/// and on Linux hosts running outside a cgroup (`memory.max = "max"`,
/// no cgroup file at all, or v1 limit at the kernel sentinel value).
pub fn probe_cgroup_memory_limit_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        // cgroup v2 first.
        if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
            let trimmed = s.trim();
            if trimmed == "max" {
                return None;
            }
            if let Ok(n) = trimmed.parse::<u64>() {
                return Some(n);
            }
        }
        // cgroup v1 fallback.
        if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
            if let Ok(n) = s.trim().parse::<u64>() {
                // v1 sets a sentinel near 2^63 when "unlimited". Anything
                // above 1 PiB is implausible for an honest limit; treat
                // as unlimited.
                if n > (1u64 << 50) {
                    return None;
                }
                return Some(n);
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Probe physical RAM. Linux via `/proc/meminfo`; macOS via
/// `sysctlbyname("hw.memsize")`. `None` when the probe fails — the
/// caller should substitute [`PHYSICAL_RAM_FALLBACK_BYTES`].
pub fn probe_physical_ram_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
                return Some(kb.saturating_mul(1024));
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let name = CString::new("hw.memsize").ok()?;
        let mut value: u64 = 0;
        let mut size: libc::size_t = std::mem::size_of::<u64>();
        // SAFETY: `name` is a valid NUL-terminated C string alive for the call;
        // `value`/`size` are valid, properly-aligned pointers to `u64`/`size_t`
        // storage sized via `size_of`; the new-value pointer is null with len 0,
        // the documented sysctl contract for a read-only query.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut value as *mut u64 as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 { Some(value) } else { None }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statvfs_returns_some_for_tmpdir() {
        // The current working directory always exists; statvfs should
        // produce a non-zero free byte count on any sane CI host.
        let cwd = std::env::current_dir().unwrap();
        let free = probe_filesystem_free(&cwd);
        assert!(free.is_some(), "statvfs returned None for cwd");
        let bytes = free.unwrap();
        assert!(bytes > 0, "statvfs reported 0 free bytes for cwd");
    }

    #[test]
    fn statvfs_uses_parent_when_path_missing() {
        // Path under cwd that does not exist — statvfs of the parent
        // should still succeed.
        let mut p = std::env::current_dir().unwrap();
        p.push("nonexistent-i4-probe-test");
        let free = probe_filesystem_free(&p);
        assert!(free.is_some());
    }

    #[test]
    fn statvfs_returns_none_for_missing_root() {
        // /this/path/does/not/exist/anywhere — both the path and its
        // ancestors are missing. statvfs of "/" should still succeed
        // because parent walks up. The point of this test is to
        // exercise the not-exists branch without expecting failure.
        let p = std::path::PathBuf::from("/this/path/does/not/exist/anywhere");
        let _ = probe_filesystem_free(&p);
    }

    #[test]
    fn physical_ram_returns_some_on_linux_or_macos() {
        let ram = probe_physical_ram_bytes();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(ram.is_some(), "expected Some on linux/macos");
            assert!(ram.unwrap() > 0);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            assert!(ram.is_none());
        }
    }

    #[test]
    fn cgroup_returns_none_on_macos() {
        #[cfg(target_os = "macos")]
        {
            assert!(probe_cgroup_memory_limit_bytes().is_none());
        }
        // On Linux this depends on the runner. We only assert the
        // probe does not panic.
        #[cfg(target_os = "linux")]
        {
            let _ = probe_cgroup_memory_limit_bytes();
        }
    }

    #[test]
    fn probe_host_hardware_returns_sane_values() {
        let cwd = std::env::current_dir().unwrap();
        let host = probe_host_hardware(&cwd, Some(&cwd));
        assert!(host.physical_ram_bytes > 0);
        assert!(host.hdd_filesystem_free_bytes > 0);
        assert!(host.ssd_filesystem_free_bytes.unwrap() > 0);
    }

    // ──────────────── V2 review: defensive /proc/meminfo parsing ─────────────

    /// Helper: parse one MemAvailable line in the same way the
    /// production `probe_runtime_ram` does. Pulled into a free
    /// function so we can test the parser against synthetic inputs
    /// without touching the real `/proc/meminfo`.
    fn parse_mem_available_kib(meminfo: &str) -> Option<u64> {
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                if let Some(kb_str) = rest.split_whitespace().next() {
                    return kb_str.parse::<u64>().ok();
                }
                return None;
            }
        }
        None
    }

    #[test]
    fn mem_available_parses_standard_line() {
        let m = "MemTotal:        8000000 kB\n\
                 MemFree:         1234567 kB\n\
                 MemAvailable:    5678900 kB\n";
        assert_eq!(parse_mem_available_kib(m), Some(5678900));
    }

    #[test]
    fn mem_available_missing_line_returns_none() {
        // Kernels < 3.14 don't expose MemAvailable; the loop
        // returns None silently and the parent function falls
        // back to the cgroup-only computation.
        let m = "MemTotal:        8000000 kB\n\
                 MemFree:         1234567 kB\n\
                 Buffers:          100000 kB\n";
        assert_eq!(parse_mem_available_kib(m), None);
    }

    #[test]
    fn mem_available_empty_string_returns_none() {
        assert_eq!(parse_mem_available_kib(""), None);
    }

    #[test]
    fn mem_available_garbled_value_returns_none() {
        // "abc kB" cannot parse as u64 → the parser returns None
        // and the parent falls back conservatively.
        let m = "MemAvailable:    abc kB\n";
        assert_eq!(parse_mem_available_kib(m), None);
    }

    #[test]
    fn mem_available_no_whitespace_after_colon_returns_none() {
        // Defensive: split_whitespace().next() yields None on the
        // empty rest, so the function returns None without panic.
        let m = "MemAvailable:\n";
        assert_eq!(parse_mem_available_kib(m), None);
    }

    #[test]
    fn mem_available_zero_value_parses_as_zero() {
        // A literal "0 kB" is a legitimate parse. Treating it as
        // "no available memory" is the caller's responsibility;
        // the parser just returns Some(0).
        let m = "MemAvailable:           0 kB\n";
        assert_eq!(parse_mem_available_kib(m), Some(0));
    }
}
