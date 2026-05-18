//! Process topology containers — shape-independent semantic wrappers.
use crate::process::adapter::step_engine::{Cap, SpinMutex, Weak};
use crate::process::structure::{ProcessGroup, ProcessIdentity};
use crate::thread_runtime::ThreadIdentity;
use alloc::vec::Vec;

pub struct ProcessChildren {
    pub(crate) inner: SpinMutex<Vec<Cap<ProcessIdentity>>>,
}
impl Default for ProcessChildren {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessChildren {
    pub fn new() -> Self {
        Self {
            inner: SpinMutex::new(Vec::new()),
        }
    }
    pub fn from_vec(v: Vec<Cap<ProcessIdentity>>) -> Self {
        Self {
            inner: SpinMutex::new(v),
        }
    }
    pub fn attach(&self, c: Cap<ProcessIdentity>) {
        self.inner.lock().push(c);
    }
    pub fn detach(&self, c: &ProcessIdentity) -> Option<Cap<ProcessIdentity>> {
        let k = c.pid.0;
        let mut i = self.inner.lock();
        i.iter().position(|x| x.pid.0 == k).map(|p| i.remove(p))
    }
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
    pub fn snapshot(&self) -> Vec<Cap<ProcessIdentity>> {
        self.inner.lock().clone()
    }
    pub fn drain(&self) -> Vec<Cap<ProcessIdentity>> {
        core::mem::take(&mut *self.inner.lock())
    }
    pub fn retain(&self, f: impl FnMut(&Cap<ProcessIdentity>) -> bool) {
        self.inner.lock().retain(f);
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn clear(&self) {
        self.inner.lock().clear();
    }
}

pub struct ProcessThreads {
    pub(crate) inner: SpinMutex<Vec<Cap<ThreadIdentity>>>,
}
impl Default for ProcessThreads {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessThreads {
    pub fn new() -> Self {
        Self {
            inner: SpinMutex::new(Vec::new()),
        }
    }
    pub fn from_vec(v: Vec<Cap<ThreadIdentity>>) -> Self {
        Self {
            inner: SpinMutex::new(v),
        }
    }
    pub fn attach(&self, t: Cap<ThreadIdentity>) {
        self.inner.lock().push(t);
    }
    pub fn detach(&self, t: &ThreadIdentity) -> Option<Cap<ThreadIdentity>> {
        let tid = t.tid;
        let mut i = self.inner.lock();
        i.iter().position(|x| x.tid == tid).map(|p| i.remove(p))
    }
    pub fn count(&self) -> usize {
        self.inner.lock().len()
    }
    pub fn nth(&self, idx: usize) -> Option<Cap<ThreadIdentity>> {
        self.inner.lock().get(idx).cloned()
    }
    pub fn find_by_tid(&self, tid: u32) -> Option<Cap<ThreadIdentity>> {
        self.inner.lock().iter().find(|t| t.tid.0 == tid).cloned()
    }
    pub fn snapshot(&self) -> Vec<Cap<ThreadIdentity>> {
        self.inner.lock().clone()
    }
    pub fn drain(&self) -> Vec<Cap<ThreadIdentity>> {
        core::mem::take(&mut *self.inner.lock())
    }
    pub fn retain(&self, f: impl FnMut(&Cap<ThreadIdentity>) -> bool) {
        self.inner.lock().retain(f);
    }
}

pub struct ProcessGroupMembers {
    pub(crate) inner: SpinMutex<Vec<Weak<ProcessIdentity>>>,
}
impl Default for ProcessGroupMembers {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessGroupMembers {
    pub fn new() -> Self {
        Self {
            inner: SpinMutex::new(Vec::new()),
        }
    }
    pub fn attach(&self, p: Weak<ProcessIdentity>) {
        self.inner.lock().push(p);
    }
    pub fn detach(&self, p: &ProcessIdentity) {
        let k = p.pid.0;
        self.inner.lock().retain(|w| w.key().raw() != k);
    }
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
    pub fn retain(&self, f: impl FnMut(&Weak<ProcessIdentity>) -> bool) {
        self.inner.lock().retain(f);
    }
    pub fn snapshot_live(
        &self,
        g: &crate::process::adapter::step_engine::Guard<'_>,
    ) -> Vec<Cap<ProcessIdentity>> {
        self.inner
            .lock()
            .iter()
            .filter_map(|w| w.upgrade(g))
            .collect()
    }
    pub fn count_live(&self, g: &crate::process::adapter::step_engine::Guard<'_>) -> usize {
        self.inner
            .lock()
            .iter()
            .filter(|w| w.upgrade(g).is_some())
            .count()
    }
}

pub struct SessionMembers {
    pub(crate) inner: SpinMutex<Vec<Weak<ProcessGroup>>>,
}
impl Default for SessionMembers {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionMembers {
    pub fn new() -> Self {
        Self {
            inner: SpinMutex::new(Vec::new()),
        }
    }
    pub fn attach(&self, p: Weak<ProcessGroup>) {
        self.inner.lock().push(p);
    }
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
    pub fn snapshot_live(
        &self,
        g: &crate::process::adapter::step_engine::Guard<'_>,
    ) -> Vec<Cap<ProcessGroup>> {
        self.inner
            .lock()
            .iter()
            .filter_map(|w| w.upgrade(g))
            .collect()
    }
}
