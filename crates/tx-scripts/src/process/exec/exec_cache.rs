//! Small exec parser cache.
//!
//! The cache sits below `exec_script`'s policy work: it remembers the bounded
//! ELF header window and pure parser output for a `PageContainer`. It does not
//! cache ASLR-adjusted addresses, stack images, credentials, or VM recipes.

use alloc::vec::Vec;

use tx_subsystems::page_backed::PageContainer;

use super::loader::ExecImagePlan;
use crate::adapter::step_engine::{Cap, SpinMutex};

const EXEC_PARSE_CACHE_ENTRIES: usize = 32;

static EXEC_PARSE_CACHE: SpinMutex<ExecParseCache> = SpinMutex::new(ExecParseCache::empty());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecParseMode {
    Main,
    Interpreter,
}

#[derive(Clone)]
pub struct CachedExecParse {
    pub header_bytes: Vec<u8>,
    pub parsed: ExecImagePlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExecParseCacheKey {
    pc_trace_id: u64,
    size_bytes: u64,
    content_epoch: u64,
    mode: ExecParseMode,
}

struct ExecParseCache {
    clock: u64,
    entries: Vec<ExecParseCacheEntry>,
}

impl ExecParseCache {
    const fn empty() -> Self {
        Self {
            clock: 0,
            entries: Vec::new(),
        }
    }

    fn lookup(&mut self, key: ExecParseCacheKey) -> Option<CachedExecParse> {
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        self.clock = self.clock.wrapping_add(1);
        let entry = &mut self.entries[index];
        entry.last_used = self.clock;
        Some(entry.value.clone())
    }

    fn insert(&mut self, key: ExecParseCacheKey, value: CachedExecParse) {
        self.clock = self.clock.wrapping_add(1);
        self.entries.retain(|entry| {
            !(entry.key.pc_trace_id == key.pc_trace_id && entry.key.mode == key.mode)
        });
        if self.entries.len() >= EXEC_PARSE_CACHE_ENTRIES {
            if let Some((victim, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_used)
            {
                self.entries.remove(victim);
            }
        }
        self.entries.push(ExecParseCacheEntry {
            key,
            value,
            last_used: self.clock,
        });
    }

    #[cfg(test)]
    fn clear(&mut self) {
        self.clock = 0;
        self.entries.clear();
    }
}

struct ExecParseCacheEntry {
    key: ExecParseCacheKey,
    value: CachedExecParse,
    last_used: u64,
}

fn cache_key(pc: &Cap<PageContainer>, size_bytes: u64, mode: ExecParseMode) -> ExecParseCacheKey {
    ExecParseCacheKey {
        pc_trace_id: pc.trace_id(),
        size_bytes,
        content_epoch: pc.content_epoch(),
        mode,
    }
}

pub fn lookup_cached_parse(
    pc: &Cap<PageContainer>,
    size_bytes: u64,
    mode: ExecParseMode,
) -> Option<CachedExecParse> {
    EXEC_PARSE_CACHE
        .lock()
        .lookup(cache_key(pc, size_bytes, mode))
}

pub fn insert_cached_parse(
    pc: &Cap<PageContainer>,
    size_bytes: u64,
    mode: ExecParseMode,
    header_bytes: &[u8],
    parsed: &ExecImagePlan,
) {
    let value = CachedExecParse {
        header_bytes: header_bytes.to_vec(),
        parsed: parsed.clone(),
    };
    EXEC_PARSE_CACHE
        .lock()
        .insert(cache_key(pc, size_bytes, mode), value);
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;
    use tx_subsystems::page_backed::{AnonSwapPolicy, PageContainerKind};

    fn dummy_plan(entry: u64) -> ExecImagePlan {
        ExecImagePlan {
            entry,
            at_phdr: 0x40,
            at_phent: 56,
            at_phnum: 1,
            load_segments: Vec::new(),
            bss_extension: None,
            load_bias: 0,
            executable_stack: false,
            interp: None,
        }
    }

    fn anon_pc() -> Cap<PageContainer> {
        tx_test_support::init_host();
        tx_subsystems::zones::register_all().expect("kernel zones");
        PageContainer::new_cap(
            PageContainerKind::Anon {
                swap_policy: AnonSwapPolicy::Reclaimable,
            },
            1,
        )
        .expect("exec parse cache test page container")
    }

    #[test]
    fn exec_parse_cache_reuses_matching_page_container_snapshot() {
        EXEC_PARSE_CACHE.lock().clear();
        let pc = anon_pc();
        let plan = dummy_plan(0x1200);

        insert_cached_parse(&pc, pc.size_bytes(), ExecParseMode::Main, b"\x7fELF", &plan);
        let cached =
            lookup_cached_parse(&pc, pc.size_bytes(), ExecParseMode::Main).expect("cache hit");

        assert_eq!(cached.header_bytes, b"\x7fELF");
        assert_eq!(cached.parsed.entry, 0x1200);
    }

    #[test]
    fn exec_parse_cache_misses_after_size_snapshot_changes() {
        EXEC_PARSE_CACHE.lock().clear();
        let pc = anon_pc();
        let plan = dummy_plan(0x2200);
        let old_size = pc.size_bytes();

        insert_cached_parse(&pc, old_size, ExecParseMode::Interpreter, b"interp", &plan);
        pc.set_size_bytes(old_size / 2);

        assert!(lookup_cached_parse(&pc, pc.size_bytes(), ExecParseMode::Interpreter).is_none());
    }
}
