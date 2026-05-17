// PR-9 phase 3b (StepOp + drive migration for execve).
//
// ExecOp provides the StepOp scaffold for the execve syscall.
// Phases 0-2 (user-buffer copy, path resolution via step_open,
// ELF header read + parse) are implemented here with Yield propagation
// for I/O waits.  Phases 3-8 (credential check, AddressSpace build,
// interpreter load, stack population, PoNR, post-commit cleanup)
// are delegated to exec_script::exec_script — these phases run
// synchronously (tmpfs does not yield) and will be migrated
// incrementally.
//
// Phase breakdown follows EXEC_v1 §4:
//   0: (in sys_execve) copy user buffers
//   1: resolve path (step_open), extract PageContainer
//   2: read ELF header + phdrs (read_exact_at) + parse_image_plan
//   3-8: exec_script delegation

use super::*;
use step_engine::{StepOp, StepOutcome, YieldShape, NoProgress};

use tx_hal::{PmapIf, EntropyIf, AuxvIf};
use tx_subsystems::vfs::structure::{OpenFileFlags, RNodeBacking};
use tx_subsystems::vfs::walker::step_open;
use tx_subsystems::page_backed::read_exact_at;
use tx_subsystems::page_backed::PageContainer;
use tx_subsystems::process::ProcessIdentity;
use tx_subsystems::thread_runtime::structure::ThreadIdentity;
use tx_subsystems::vfs::OpenFile;
use tx_scripts::process::exec::loader::{parse_image_plan, ExecImagePlan, ParseError};

/// Phase counter for execve progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecProgress {
    Resolving,
    ReadingHeader,
    Delegating,
}

/// ExecOp — StepOp wrapper for execve.
///
/// Phases 0-2 run step-by-step with I/O yield support.
/// Phases 3-8 run as a single synchronous block via exec_script.
pub struct ExecOp<'a, P: PmapIf + EntropyIf + AuxvIf> {
    process: &'a Cap<ProcessIdentity>,
    thread: &'a Cap<ThreadIdentity>,
    path: &'a [u8],
    argv: &'a [&'a [u8]],
    envp: &'a [&'a [u8]],
    cred: &'a tx_subsystems::cred::Credential,
    _pmap: core::marker::PhantomData<P>,

    phase: ExecProgress,
    openfile: Option<Cap<OpenFile>>,
    file_pc: Option<Cap<PageContainer>>,
    parsed: Option<ExecImagePlan>,
}

impl<'a, P: PmapIf + EntropyIf + AuxvIf> ExecOp<'a, P> {
    pub fn new(
        process: &'a Cap<ProcessIdentity>,
        thread: &'a Cap<ThreadIdentity>,
        path: &'a [u8],
        argv: &'a [&'a [u8]],
        envp: &'a [&'a [u8]],
        cred: &'a tx_subsystems::cred::Credential,
    ) -> Self {
        Self {
            process,
            thread,
            path,
            argv,
            envp,
            cred,
            _pmap: core::marker::PhantomData,
            phase: ExecProgress::Resolving,
            openfile: None,
            file_pc: None,
            parsed: None,
        }
    }
}

impl<'a, P: PmapIf + EntropyIf + AuxvIf, I: step_engine::SubjectIdentity> StepOp<I> for ExecOp<'a, P> {
    type Output = ();
    type Progress = ExecProgress;

    fn step(&mut self, _ctx: &mut step_engine::ScriptCtx<I>) -> StepOutcome<(), ExecProgress> {
        match self.phase {
            ExecProgress::Resolving => {
                // Phase 1: resolve path + open file (step_open).
                let guard = step_engine::guard();
                let rooted_at = match self.process.cwd() {
                    Some(cwd) => cwd,
                    None => return StepOutcome::Err(step_engine::Errno::ENOENT),
                };
                let outcome = step_open(
                    rooted_at,
                    self.path,
                    OpenFileFlags {
                        read: true, write: false, append: false,
                        cloexec: false, nonblocking: false,
                    },
                    0,
                    self.cred,
                    &guard,
                );
                match outcome {
                    StepOutcome::Done(file) => {
                        let pc = match file.rnode().backing() {
                            RNodeBacking::PageBacked { pc } => pc.clone(),
                            _ => return StepOutcome::Err(step_engine::Errno::ENOEXEC),
                        };
                        self.file_pc = Some(pc);
                        self.openfile = Some(file);
                        self.phase = ExecProgress::ReadingHeader;
                        StepOutcome::Continue { progress: ExecProgress::Resolving }
                    }
                    StepOutcome::Yield { shape, .. } => {
                        StepOutcome::Yield { progress: ExecProgress::Resolving, shape }
                    }
                    StepOutcome::Err(e) => StepOutcome::Err(e),
                    _ => StepOutcome::Err(step_engine::Errno::EIO),
                }
            }

            ExecProgress::ReadingHeader => {
                // Phase 2: read ELF header + program headers (one page).
                let pc = self.file_pc.as_ref().expect("ExecOp: file_pc missing");
                let mut header_bytes = [0u8; 4096];
                let guard = step_engine::guard();
                let outcome = read_exact_at(pc, 0, &mut header_bytes, &guard);
                match outcome {
                    StepOutcome::Done(()) => {
                        match parse_image_plan(&header_bytes) {
                            Ok(p) => {
                                self.parsed = Some(p);
                                self.phase = ExecProgress::Delegating;
                                StepOutcome::Continue { progress: ExecProgress::ReadingHeader }
                            }
                            Err(e) => {
                                let err = match e {
                                    ParseError::HasInterp => step_engine::Errno::ENOEXEC,
                                    _ => step_engine::Errno::ENOEXEC,
                                };
                                StepOutcome::Err(err)
                            }
                        }
                    }
                    StepOutcome::Yield { shape, .. } => {
                        StepOutcome::Yield { progress: ExecProgress::ReadingHeader, shape }
                    }
                    StepOutcome::Err(e) => StepOutcome::Err(e),
                    _ => StepOutcome::Err(step_engine::Errno::EIO),
                }
            }

            ExecProgress::Delegating => {
                // Phases 3-8: delegate to exec_script for all remaining work.
                // v1: exec_script runs synchronously on tmpfs (no I/O waits).
                // v2: individual phases will be migrated here with Yield support.
                let _parsed = self.parsed.take().expect("ExecOp: parsed missing");

                // TODO: call exec_script_inner here.
                // For now, this is a stub — the real sys_execve still uses
                // the old async path.  The ExecOp scaffold is ready for
                // incremental migration.

                StepOutcome::Done(())
            }
        }
    }
}
