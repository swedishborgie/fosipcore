/// Process-wide pool of loopback live-stream (HTTP-FLV) ports.
///
/// The reference assigns one port per spawned core
/// (`livePort = tid % 6000 + 20000`). Our single process serves many
/// concurrent sessions, so each logged-in session allocates its own port
/// from this pool, runs its own `VideoServer` + HTTP-FLV listener on it,
/// and receives it in its own `InitInfo` (50009) as `livePort`.
///
/// `alloc` does one better than the reference's bare modulo: candidates are
/// validated with a throwaway loopback bind, so a port occupied by another
/// process is skipped instead of handed out (the reference has no
/// collision handling at all).
///
/// The allocated port is returned in the session's own `InitInfo` (50009)
/// as `livePort`.
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

/// Default base of the live-port range (the reference's `+ 20000`).
pub const DEFAULT_BASE: u16 = 20000;
/// Default size of the live-port range (the reference's `% 6000`).
pub const DEFAULT_COUNT: u16 = 6000;

/// Monotonic-counter port pool over `[base, base + count)`.
///
/// The counter never resets (sessions that disconnect early do not
/// "reclaim" the counter's position); `in_use` tracks handed-out ports so
/// a wrapped candidate is not re-handed until it is released.
#[derive(Debug)]
pub struct PortPool {
    base: u16,
    count: u16,
    next: AtomicU16,
    in_use: Mutex<HashSet<u16>>,
}

impl PortPool {
    /// Create a pool over `[base, base + count)`. `count` is clamped to ≥ 1.
    pub fn new(base: u16, count: u16) -> Arc<Self> {
        Arc::new(Self {
            base,
            count: count.max(1),
            next: AtomicU16::new(0),
            in_use: Mutex::new(HashSet::new()),
        })
    }

    /// Build a pool from `FOSIPCORE_LIVE_PORT_BASE` /
    /// `FOSIPCORE_LIVE_PORT_COUNT`, falling back to the reference's range
    /// (20000, 6000).
    pub fn from_env() -> Arc<Self> {
        let base = std::env::var("FOSIPCORE_LIVE_PORT_BASE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_BASE);
        let count = std::env::var("FOSIPCORE_LIVE_PORT_COUNT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_COUNT);
        Self::new(base, count)
    }

    /// The `[first, last]` ports this pool may hand out.
    pub fn range(&self) -> (u16, u16) {
        (self.base, self.base.saturating_add(self.count.saturating_sub(1)))
    }

    /// Allocate a free port, or `None` if the pool is exhausted.
    ///
    /// Walks up to `count` candidates from the monotonic counter, skipping
    /// ports still in the pool's in-use set, and hands a candidate back only
    /// after a throwaway loopback `bind` succeeds on it (external occupancy
    /// is skipped, as in the reference-free world we live in).
    pub async fn alloc(&self) -> Option<u16> {
        let mut in_use = self.in_use.lock().await;
        for _ in 0..self.count {
            let start = u32::from(self.next.fetch_add(1, Ordering::Relaxed));
            // u32 math: `base + start % count` can exceed u16::MAX for
            // high base values; wrap into the 16-bit port space.
            let candidate = (u32::from(self.base) + start % u32::from(self.count)) % 65536;
            let Some(candidate) = u16::try_from(candidate).ok() else {
                continue;
            };
            if in_use.contains(&candidate) {
                continue;
            }
            // Validate with a throwaway loopback bind: the probe drops at
            // the end of this match arm, freeing the port immediately — the
            // session's HTTP-FLV listener re-binds it right after.
            if tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], candidate))).await
                .is_ok()
            {
                in_use.insert(candidate);
                return Some(candidate);
            }
        }
        None
    }

    /// Release a previously allocated port (re-allocatable once the
    /// monotonic counter wraps back around it).
    pub async fn release(&self, port: u16) {
        self.in_use.lock().await.remove(&port);
    }

    /// Number of ports currently handed out (active video sessions).
    pub async fn in_use_count(&self) -> usize {
        self.in_use.lock().await.len()
    }

    /// Snapshot of the currently handed-out ports (tests, debug).
    pub async fn in_use(&self) -> HashSet<u16> {
        self.in_use.lock().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool whose base is a free ephemeral port, so tests never collide
    /// with anything else on the machine.
    async fn fresh_pool(count: u16) -> (Arc<PortPool>, u16) {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind probe");
        let base = probe.local_addr().expect("probe addr").port();
        drop(probe);
        (PortPool::new(base, count), base)
    }

    #[tokio::test]
    async fn sequential_alloc_walks_forward() {
        let (pool, base) = fresh_pool(100).await;
        assert_eq!(pool.alloc().await, Some(base));
        assert_eq!(pool.alloc().await, Some(base + 1));
        assert_eq!(pool.alloc().await, Some(base + 2));
        assert_eq!(pool.in_use_count().await, 3);
    }

    #[tokio::test]
    async fn exhaustion_returns_none_then_release_allows() {
        let (pool, base) = fresh_pool(3).await;
        assert_eq!(pool.alloc().await, Some(base));
        assert_eq!(pool.alloc().await, Some(base + 1));
        assert_eq!(pool.alloc().await, Some(base + 2));
        // All three in use; the wrapped candidates are skipped.
        assert_eq!(pool.alloc().await, None);

        pool.release(base).await;
        // The counter wraps back to the freed port.
        assert_eq!(pool.alloc().await, Some(base));
        assert_eq!(pool.in_use_count().await, 3);
    }

    #[tokio::test]
    async fn bind_retry_skips_externally_bound_port() {
        let (pool, base) = fresh_pool(100).await;
        // Occupy the first candidate for the duration of the test.
        let blocker = tokio::net::TcpListener::bind(format!("127.0.0.1:{base}"))
            .await
            .expect("blocker bind");
        assert_eq!(blocker.local_addr().unwrap().port(), base);

        let port = pool.alloc().await.expect("should skip occupied port");
        assert_eq!(port, base + 1);
        assert_eq!(pool.in_use_count().await, 1);
    }

    #[tokio::test]
    async fn release_shrinks_in_use_set() {
        let (pool, _base) = fresh_pool(10).await;
        let a = pool.alloc().await.expect("alloc a");
        let b = pool.alloc().await.expect("alloc b");
        assert_eq!(pool.in_use().await, HashSet::from([a, b]));

        pool.release(a).await;
        assert_eq!(pool.in_use().await, HashSet::from([b]));
        pool.release(b).await;
        assert_eq!(pool.in_use_count().await, 0);
    }

    #[tokio::test]
    async fn range_reports_bounds() {
        let pool = PortPool::new(20000, 6000);
        assert_eq!(pool.range(), (20000, 25999));
    }
}
