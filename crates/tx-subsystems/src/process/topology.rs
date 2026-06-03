//! Process topology containers — shape-independent semantic wrappers.
use crate::process::adapter::step_engine::{process_spin_mutex, Cap, ProcessSpinMutex, Weak};
use crate::process::structure::{ProcessGroup, ProcessIdentity};
use crate::thread_runtime::ThreadIdentity;
use alloc::vec::Vec;

macro_rules! measure_process_ds {
    ($method_name:expr, $body:block) => {{
        #[cfg(all(tx_ds_metrics, tx_ds_metrics_process))]
        {
            crate::process::ds_metrics::measure($method_name, || $body)
        }
        #[cfg(not(all(tx_ds_metrics, tx_ds_metrics_process)))]
        {
            $body
        }
    }};
}

pub struct ProcessChildren {
    pub(crate) inner: ProcessSpinMutex<Vec<Cap<ProcessIdentity>>>,
}
impl Default for ProcessChildren {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessChildren {
    pub fn new() -> Self {
        Self {
            inner: process_spin_mutex(Vec::new(), b"debug.lock.process.children"),
        }
    }
    pub fn from_vec(v: Vec<Cap<ProcessIdentity>>) -> Self {
        Self {
            inner: process_spin_mutex(v, b"debug.lock.process.children"),
        }
    }
    pub fn attach(&self, c: Cap<ProcessIdentity>) {
        measure_process_ds!(b"debug.ds.process.children.attach", {
            self.inner.lock().push(c);
        });
    }
    pub fn detach(&self, c: &ProcessIdentity) -> Option<Cap<ProcessIdentity>> {
        measure_process_ds!(b"debug.ds.process.children.detach", {
            let k = c.pid.0;
            let mut i = self.inner.lock();
            i.iter().position(|x| x.pid.0 == k).map(|p| i.remove(p))
        })
    }
    pub fn len(&self) -> usize {
        measure_process_ds!(b"debug.ds.process.children.len", {
            self.inner.lock().len()
        })
    }
    pub fn is_empty(&self) -> bool {
        measure_process_ds!(b"debug.ds.process.children.is_empty", {
            self.inner.lock().is_empty()
        })
    }
    pub fn snapshot(&self) -> Vec<Cap<ProcessIdentity>> {
        measure_process_ds!(b"debug.ds.process.children.snapshot", {
            self.inner.lock().clone()
        })
    }
    pub fn drain(&self) -> Vec<Cap<ProcessIdentity>> {
        measure_process_ds!(b"debug.ds.process.children.drain", {
            core::mem::take(&mut *self.inner.lock())
        })
    }
    pub fn retain(&self, f: impl FnMut(&Cap<ProcessIdentity>) -> bool) {
        measure_process_ds!(b"debug.ds.process.children.retain", {
            self.inner.lock().retain(f);
        });
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn clear(&self) {
        self.inner.lock().clear();
    }
}

pub struct ProcessThreads {
    pub(crate) inner: ProcessSpinMutex<Vec<Cap<ThreadIdentity>>>,
}
impl Default for ProcessThreads {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessThreads {
    pub fn new() -> Self {
        Self {
            inner: process_spin_mutex(Vec::new(), b"debug.lock.process.threads"),
        }
    }
    pub fn from_vec(v: Vec<Cap<ThreadIdentity>>) -> Self {
        Self {
            inner: process_spin_mutex(v, b"debug.lock.process.threads"),
        }
    }
    pub fn attach(&self, t: Cap<ThreadIdentity>) {
        measure_process_ds!(b"debug.ds.process.threads.attach", {
            self.inner.lock().push(t);
        });
    }
    pub fn detach(&self, t: &ThreadIdentity) -> Option<Cap<ThreadIdentity>> {
        measure_process_ds!(b"debug.ds.process.threads.detach", {
            let tid = t.tid;
            let mut i = self.inner.lock();
            i.iter().position(|x| x.tid == tid).map(|p| i.remove(p))
        })
    }
    pub fn count(&self) -> usize {
        measure_process_ds!(b"debug.ds.process.threads.count", {
            self.inner.lock().len()
        })
    }
    pub fn nth(&self, idx: usize) -> Option<Cap<ThreadIdentity>> {
        measure_process_ds!(b"debug.ds.process.threads.nth", {
            self.inner.lock().get(idx).cloned()
        })
    }
    pub fn find_by_tid(&self, tid: u32) -> Option<Cap<ThreadIdentity>> {
        measure_process_ds!(b"debug.ds.process.threads.find_by_tid", {
            self.inner.lock().iter().find(|t| t.tid.0 == tid).cloned()
        })
    }
    pub fn snapshot(&self) -> Vec<Cap<ThreadIdentity>> {
        measure_process_ds!(b"debug.ds.process.threads.snapshot", {
            self.inner.lock().clone()
        })
    }
    pub fn drain(&self) -> Vec<Cap<ThreadIdentity>> {
        measure_process_ds!(b"debug.ds.process.threads.drain", {
            core::mem::take(&mut *self.inner.lock())
        })
    }
    pub fn retain(&self, f: impl FnMut(&Cap<ThreadIdentity>) -> bool) {
        measure_process_ds!(b"debug.ds.process.threads.retain", {
            self.inner.lock().retain(f);
        });
    }
}

pub struct ProcessGroupMembers {
    pub(crate) inner: ProcessSpinMutex<Vec<Weak<ProcessIdentity>>>,
}
impl Default for ProcessGroupMembers {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessGroupMembers {
    pub fn new() -> Self {
        Self {
            inner: process_spin_mutex(Vec::new(), b"debug.lock.process.group_members"),
        }
    }
    pub fn attach(&self, p: Weak<ProcessIdentity>) {
        measure_process_ds!(b"debug.ds.process.group_members.attach", {
            self.inner.lock().push(p);
        });
    }
    pub fn detach(&self, p: &ProcessIdentity) {
        measure_process_ds!(b"debug.ds.process.group_members.detach", {
            let k = p.pid.0;
            self.inner.lock().retain(|w| w.key().raw() != k);
        });
    }
    pub fn len(&self) -> usize {
        measure_process_ds!(b"debug.ds.process.group_members.len", {
            self.inner.lock().len()
        })
    }
    pub fn is_empty(&self) -> bool {
        measure_process_ds!(b"debug.ds.process.group_members.is_empty", {
            self.inner.lock().is_empty()
        })
    }
    pub fn retain(&self, f: impl FnMut(&Weak<ProcessIdentity>) -> bool) {
        measure_process_ds!(b"debug.ds.process.group_members.retain", {
            self.inner.lock().retain(f);
        });
    }
    pub fn snapshot_live(
        &self,
        g: &crate::process::adapter::step_engine::Guard<'_>,
    ) -> Vec<Cap<ProcessIdentity>> {
        measure_process_ds!(b"debug.ds.process.group_members.snapshot_live", {
            self.inner
                .lock()
                .iter()
                .filter_map(|w| w.upgrade(g))
                .collect()
        })
    }
    pub fn count_live(&self, g: &crate::process::adapter::step_engine::Guard<'_>) -> usize {
        measure_process_ds!(b"debug.ds.process.group_members.count_live", {
            self.inner
                .lock()
                .iter()
                .filter(|w| w.upgrade(g).is_some())
                .count()
        })
    }
}

pub struct SessionMembers {
    pub(crate) inner: ProcessSpinMutex<Vec<Weak<ProcessGroup>>>,
}
impl Default for SessionMembers {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionMembers {
    pub fn new() -> Self {
        Self {
            inner: process_spin_mutex(Vec::new(), b"debug.lock.process.session_members"),
        }
    }
    pub fn attach(&self, p: Weak<ProcessGroup>) {
        measure_process_ds!(b"debug.ds.process.session_members.attach", {
            self.inner.lock().push(p);
        });
    }
    pub fn len(&self) -> usize {
        measure_process_ds!(b"debug.ds.process.session_members.len", {
            self.inner.lock().len()
        })
    }
    pub fn is_empty(&self) -> bool {
        measure_process_ds!(b"debug.ds.process.session_members.is_empty", {
            self.inner.lock().is_empty()
        })
    }
    pub fn snapshot_live(
        &self,
        g: &crate::process::adapter::step_engine::Guard<'_>,
    ) -> Vec<Cap<ProcessGroup>> {
        measure_process_ds!(b"debug.ds.process.session_members.snapshot_live", {
            self.inner
                .lock()
                .iter()
                .filter_map(|w| w.upgrade(g))
                .collect()
        })
    }
}
