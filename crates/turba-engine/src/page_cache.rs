//! OS page-cache residency measurement via `mincore(2)` + `mmap`.
//!
//! Used by the v0.3 cycle Spike 0 ampliado (Day 2-3) to attribute Q1
//! latency to one of three layers — block-cache hit, block-cache miss +
//! page-cache hit, or block-cache miss + page-cache miss (true seek). The
//! third class is what the I/O scheduler cannot accelerate; the second is reachable
//! through warmer block-cache sizing or bloom prefetch (D8). Distinguishing
//! them requires direct page-cache visibility, which `mincore` provides.
//!
//! Mechanism: `mmap` the file read-only (no I/O — mmap only sets up the
//! virtual mapping). `mincore` reports a per-page residency bitmap of the
//! mapping. Pages reported as resident are those already populated by
//! prior `pread` calls in the engine's read path. Sum the bitmap to derive
//! `resident_pages / total_pages`. `munmap` on the way out.
//!
//! Linux is the primary target. macOS exposes the same syscall with the
//! same semantics for our purposes; we keep the implementation
//! Linux-gated to match the existing `posix_fadvise` path in
//! `compaction/worker.rs` and to avoid divergence in operator-facing
//! reports — page-cache numbers from the macOS Docker host are mediated
//! by OrbStack's VM and would mislead. macOS callers always observe
//! `Default::default()`; the bench (Linux container) yields real numbers.

// SPDX-License-Identifier: BUSL-1.1
use std::io;
use std::path::Path;

/// Aggregate residency for a path or set of paths. `resident_pages` and
/// `total_pages` are page counts (pages, not bytes). `file_size_bytes` is
/// the underlying file size; `total_pages * page_size` may exceed
/// `file_size_bytes` by up to `page_size - 1` because the last page is
/// partial.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageCacheResidency {
    pub resident_pages: u64,
    pub total_pages: u64,
    pub file_size_bytes: u64,
}

impl PageCacheResidency {
    /// Resident fraction in `[0.0, 1.0]`. Returns `0.0` for empty inputs.
    pub fn ratio(&self) -> f64 {
        if self.total_pages == 0 {
            0.0
        } else {
            self.resident_pages as f64 / self.total_pages as f64
        }
    }

    pub fn add(&mut self, other: &Self) {
        self.resident_pages += other.resident_pages;
        self.total_pages += other.total_pages;
        self.file_size_bytes += other.file_size_bytes;
    }
}

#[cfg(target_os = "linux")]
pub fn measure_residency(path: &Path) -> io::Result<PageCacheResidency> {
    use std::fs::File;
    use std::os::unix::io::AsRawFd;

    let file = File::open(path)?;
    let file_size = file.metadata()?.len();
    if file_size == 0 {
        return Ok(PageCacheResidency::default());
    }

    // SAFETY: `sysconf` takes an integer name and returns a `long`; it reads no
    // user memory and has no preconditions. `_SC_PAGESIZE` is a valid name and
    // the result is validated (`> 0`) below before use.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(io::Error::new(io::ErrorKind::Other, "invalid page size"));
    }
    let page_size = page_size as u64;
    let total_pages = file_size.div_ceil(page_size);

    // mmap PROT_READ MAP_SHARED. mmap of a regular file does not perform
    // I/O — only sets up the VMA. Pages already in the page cache from
    // prior preads are reported as resident by mincore below.
    // SAFETY: a null `addr` lets the kernel place the mapping; `file_size` is the
    // file's real length (read above, non-zero), so the length is valid; `file`
    // owns a valid fd open for read, matching PROT_READ/MAP_SHARED; offset 0 is
    // page-aligned. The result is checked against MAP_FAILED before any use and
    // unmapped below. mmap of a regular file performs no I/O here.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            file_size as libc::size_t,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    // mincore: one byte per page; bit 0 = resident.
    let mut vec = vec![0u8; total_pages as usize];
    // SAFETY: `addr`/`file_size` are the exact base and length of the live
    // mapping created just above (not yet unmapped); `vec` holds `total_pages`
    // bytes (one per page over that range), so the kernel's residency write stays
    // in bounds.
    let rc = unsafe {
        libc::mincore(
            addr,
            file_size as libc::size_t,
            vec.as_mut_ptr() as *mut libc::c_uchar,
        )
    };
    let mincore_err = if rc != 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };

    // Always munmap, regardless of mincore outcome.
    // SAFETY: `addr`/`file_size` are the exact base and length returned by the
    // `mmap` above, unmapped exactly once (the mapping is still live here).
    let unmap_rc = unsafe { libc::munmap(addr, file_size as libc::size_t) };

    if let Some(err) = mincore_err {
        return Err(err);
    }
    if unmap_rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let resident_pages = vec.iter().filter(|&&b| b & 1 == 1).count() as u64;
    Ok(PageCacheResidency {
        resident_pages,
        total_pages,
        file_size_bytes: file_size,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn measure_residency(_path: &Path) -> io::Result<PageCacheResidency> {
    Ok(PageCacheResidency::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn empty_path_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::File::create(&path).unwrap();
        let r = measure_residency(&path).unwrap();
        assert_eq!(r, PageCacheResidency::default());
        assert_eq!(r.ratio(), 0.0);
    }

    #[test]
    fn ratio_handles_zero_total() {
        let r = PageCacheResidency::default();
        assert_eq!(r.ratio(), 0.0);
    }

    #[test]
    fn ratio_computes_correctly() {
        let r = PageCacheResidency {
            resident_pages: 3,
            total_pages: 10,
            file_size_bytes: 0,
        };
        assert_eq!(r.ratio(), 0.3);
    }

    #[test]
    fn add_aggregates() {
        let mut a = PageCacheResidency {
            resident_pages: 5,
            total_pages: 10,
            file_size_bytes: 40_000,
        };
        let b = PageCacheResidency {
            resident_pages: 7,
            total_pages: 8,
            file_size_bytes: 30_000,
        };
        a.add(&b);
        assert_eq!(a.resident_pages, 12);
        assert_eq!(a.total_pages, 18);
        assert_eq!(a.file_size_bytes, 70_000);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn measure_residency_after_read_reports_resident() {
        // After a full read, all pages should be resident in page cache.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sized.bin");
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let bytes = vec![0xab_u8; page_size * 4]; // 4 pages
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        // touch every byte → ensures the file is fully in page cache
        let _ = std::fs::read(&path).unwrap();

        let r = measure_residency(&path).unwrap();
        assert_eq!(r.total_pages, 4);
        // After a read, kernel populates page cache aggressively. Allow
        // some slack because page eviction can race; require ≥ 1 page.
        assert!(r.resident_pages >= 1);
        assert_eq!(r.file_size_bytes, (page_size * 4) as u64);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn macos_stub_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.bin");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"hello")
            .unwrap();
        let r = measure_residency(&path).unwrap();
        assert_eq!(r, PageCacheResidency::default());
    }
}
