//! Netfilter hook skeleton.
//!
//! This is intentionally only the hook surface and default-ACCEPT policy.
//! Rule storage, iptables translation, conntrack, and NAT land after the
//! bridge/route datapaths are stable.

use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterHook {
    Prerouting,
    Input,
    Forward,
    Output,
    Postrouting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterVerdict {
    Accept,
    Drop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetfilterFrameContext {
    pub hook: NetfilterHook,
    pub bridge: Option<&'static str>,
    pub ingress: Option<&'static str>,
    pub egress: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetfilterStatsSnapshot {
    pub prerouting: u64,
    pub input: u64,
    pub forward: u64,
    pub output: u64,
    pub postrouting: u64,
}

struct NetfilterStats {
    prerouting: AtomicU64,
    input: AtomicU64,
    forward: AtomicU64,
    output: AtomicU64,
    postrouting: AtomicU64,
}

static NETFILTER_STATS: NetfilterStats = NetfilterStats::new();

impl NetfilterStats {
    const fn new() -> Self {
        Self {
            prerouting: AtomicU64::new(0),
            input: AtomicU64::new(0),
            forward: AtomicU64::new(0),
            output: AtomicU64::new(0),
            postrouting: AtomicU64::new(0),
        }
    }

    fn counter(&self, hook: NetfilterHook) -> &AtomicU64 {
        match hook {
            NetfilterHook::Prerouting => &self.prerouting,
            NetfilterHook::Input => &self.input,
            NetfilterHook::Forward => &self.forward,
            NetfilterHook::Output => &self.output,
            NetfilterHook::Postrouting => &self.postrouting,
        }
    }

    fn snapshot(&self) -> NetfilterStatsSnapshot {
        NetfilterStatsSnapshot {
            prerouting: self.prerouting.load(Ordering::Relaxed),
            input: self.input.load(Ordering::Relaxed),
            forward: self.forward.load(Ordering::Relaxed),
            output: self.output.load(Ordering::Relaxed),
            postrouting: self.postrouting.load(Ordering::Relaxed),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    fn reset(&self) {
        self.prerouting.store(0, Ordering::Relaxed);
        self.input.store(0, Ordering::Relaxed);
        self.forward.store(0, Ordering::Relaxed);
        self.output.store(0, Ordering::Relaxed);
        self.postrouting.store(0, Ordering::Relaxed);
    }
}

pub fn run_frame_hook(ctx: NetfilterFrameContext, _frame: &[u8]) -> NetfilterVerdict {
    NETFILTER_STATS
        .counter(ctx.hook)
        .fetch_add(1, Ordering::Relaxed);
    NetfilterVerdict::Accept
}

pub fn netfilter_stats_snapshot() -> NetfilterStatsSnapshot {
    NETFILTER_STATS.snapshot()
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_netfilter_for_test() {
    NETFILTER_STATS.reset();
}
