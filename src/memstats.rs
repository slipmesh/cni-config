//! A periodic memory report: the live heap as counted at the allocator, next to the kernel's
//! view of the resident set. The two together tell a slow growth apart - a heap that grows is
//! memory the code keeps, a flat heap under a growing `RssAnon` is memory the allocator keeps.
//! Logged at debug level, so it stays quiet unless `RUST_LOG` names this module.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// The system allocator, counting the bytes it has handed out and not yet been given back.
pub struct CountingAllocator;

static LIVE_HEAP: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is forwarded to `System` unchanged; the counter only observes sizes.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            LIVE_HEAP.fetch_add(layout.size(), Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE_HEAP.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            LIVE_HEAP.fetch_add(new_size, Ordering::Relaxed);
            LIVE_HEAP.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        new
    }
}

/// Watch stream events and errors since start, reported next to the memory figures so that
/// growth can be set against them.
pub static WATCH_EVENTS: AtomicU64 = AtomicU64::new(0);
pub static WATCH_ERRORS: AtomicU64 = AtomicU64::new(0);

/// Resident memory as `/proc/self/status` reports it, in KiB.
#[derive(Debug, PartialEq, Eq)]
pub struct Rss {
    pub anon_kib: u64,
    pub file_kib: u64,
}

pub fn parse_status(status: &str) -> Option<Rss> {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .and_then(|value| value.trim().strip_suffix("kB")?.trim().parse().ok())
    };
    Some(Rss {
        anon_kib: field("RssAnon")?,
        file_kib: field("RssFile")?,
    })
}

/// Logs one line with the live heap, the resident set and the watch counters every `period`.
pub async fn report_every(period: Duration) {
    let mut ticks = tokio::time::interval(period);
    loop {
        ticks.tick().await;
        let rss = std::fs::read_to_string("/proc/self/status")
            .ok()
            .as_deref()
            .and_then(parse_status);
        tracing::debug!(
            heap_live_bytes = LIVE_HEAP.load(Ordering::Relaxed),
            rss_anon_kib = rss.as_ref().map(|r| r.anon_kib),
            rss_file_kib = rss.as_ref().map(|r| r.file_kib),
            watch_events = WATCH_EVENTS.load(Ordering::Relaxed),
            watch_errors = WATCH_ERRORS.load(Ordering::Relaxed),
            "memory"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_anonymous_and_file_resident_sets() {
        let status = "Name:\tcni-config\nVmRSS:\t    3300 kB\nRssAnon:\t    1200 kB\n\
                      RssFile:\t    2100 kB\nRssShmem:\t       0 kB\nThreads:\t5\n";
        assert_eq!(
            parse_status(status),
            Some(Rss {
                anon_kib: 1200,
                file_kib: 2100
            })
        );
    }

    #[test]
    fn a_status_without_them_reads_as_none() {
        assert_eq!(parse_status("Name:\tcni-config\n"), None);
    }
}
