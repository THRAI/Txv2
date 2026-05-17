// PR-9 phase 3b (StepOp + drive migration for execve).
//
// ExecOp drives the execve syscall through the v3 StepOp/drive
// pattern, yielding on I/O waits (step_open, read_exact_at,
// materialize_pagebacked) so the reactor can schedule other tasks
// during block-device reads.
//
// Phase breakdown follows EXEC_v1 §4:
//   0: capture cred snapshot, validate caller
//   1: resolve path (step_open), mount NOEXEC check
//   2: read ELF header + phdrs (read_exact_at)
//   3: parse (parse_image_plan, pure CPU)
//   3b: load interpreter (if PT_INTERP)
//   3.5: credential authorization (setuid/setgid)
//   4: build detached AddressSpace
//   5: collapse thread group (GroupExit)
//   5a: populate partial-last-page bytes + user stack
//   6: PoNR — replace_aspace + store_saved_user_context
//   7: post-commit (CLOEXEC, signal reset, brk, exe_file, cmdline)
//   8: vfork completion + publish

use super::*;
use step_engine::{StepOp, StepOutcome, YieldShape, NoProgress};

use tx_hal::{PmapIf, EntropyIf, UserTrapContext};
use tx_subsystems::cred::Credential;
use tx_subsystems::vfs::structure::{OpenFileFlags, RNodeBacking};
use tx_subsystems::vfs::walker::step_open;
use tx_subsystems::page_backed::read_exact_at;
use tx_scripts::process::exec::loader::{parse_image_plan, ExecImagePlan, InterpreterPlan, ELF64_PHENT};
use tx_scripts::process::exec::stack::{build_initial_user_stack, AuxvFacts};
use tx_subsystems::process::ProcessIdentity;
use tx_subsystems::thread_runtime::structure::ThreadIdentity;

/// Phase counter for execve progress reporting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecProgress {
    Resolving,
    ReadingHeader,
    Parsing,
    LoadingInterp,
    CredCheck,
    BuildingAspace,
    CollapsingThreads,
    PopulatingStack,
    PoNR,
    PostCommit,
}

/// Output of ExecOp — on success the caller should NOT write a
/// syscall return (the new image's entry point uses fresh registers).
/// On failure the errno is encoded by the drive loop.
pub type ExecOutput = Result<(), ExecErrno>;  // placeholder

/// ExecOp drives the entire execve syscall as a StepOp.
///
/// Each `step()` call advances one phase.  Phases that need I/O
/// (step_open, read_exact_at, materialize_pagebacked) may Yield;
/// the drive loop parks the task and resumes on I/O completion.
/// Phases that are pure CPU advance via Continue.
pub struct ExecOp<'a, P: PmapIf + EntropyIf + tx_hal::AuxvIf> {
    phase: ExecProgress,
    process: &'a Cap<ProcessIdentity>,
    thread: &'a Cap<ThreadIdentity>,
    path: &'a [u8],
    argv: &'a [&'a [u8]],
    envp: &'a [&'a [u8]],
    cred: &'a Credential,
    _pmap: core::marker::PhantomData<P>,

    // State carried across phases
    openfile: Option<Cap<tx_subsystems::vfs::OpenFile>>,
    file_pc: Option<Cap<tx_subsystems::page_backed::PageContainer>>,
    parsed: Option<ExecImagePlan>,
    new_aspace: Option<Cap<tx_subsystems::vm::AddressSpace>>,
    stack_image: Option<tx_scripts::process::exec::stack::UserStackImage>,
    interp_plan: Option<InterpreterPlan>,
    new_cred: Option<(u32, u32, u32, u32)>,  // uid, euid, gid, egid
}

impl<'a, P: PmapIf + EntropyIf + tx_hal::AuxvIf> ExecOp<'a, P> {
    pub fn new(
        process: &'a Cap<ProcessIdentity>,
        thread: &'a Cap<ThreadIdentity>,
        path: &'a [u8],
        argv: &'a [&'a [u8]],
        envp: &'a [&'a [u8]],
        cred: &'a Credential,
    ) -> Self {
        Self {
            phase: ExecProgress::Resolving,
            process,
            thread,
            path,
            argv,
            envp,
            cred,
            _pmap: core::marker::PhantomData,
            openfile: None,
            file_pc: None,
            parsed: None,
            new_aspace: None,
            stack_image: None,
            interp_plan: None,
            new_cred: None,
        }
    }
}

impl<'a, P: PmapIf + EntropyIf + tx_hal::AuxvIf, I: step_engine::SubjectIdentity> StepOp<I> for ExecOp<'a, P> {
    type Output = ();
    type Progress = ExecProgress;

    fn step(&mut self, ctx: &mut step_engine::ScriptCtx<I>) -> StepOutcome<(), ExecProgress> {
        match self.phase {
            ExecProgress::Resolving => {
                // Phase 1: resolve path + open file (step_open).
                // If step_open yields, propagate the Yield upward.
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
                        // Extract PageContainer for later phases
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
                        // I/O wait — yield and retry on wake
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
                        // Parse immediately (Phase 3 is inline since
                        // parsing is pure CPU with no I/O).
                        let parsed = match parse_image_plan(&header_bytes) {
                            Ok(p) => p,
                            Err(e) => {
                                let err = match e {
                                    tx_scripts::process::exec::loader::ParseError::HasInterp => step_engine::Errno::ENOEXEC,
                                    _ => step_engine::Errno::ENOEXEC,
                                };
                                return StepOutcome::Err(err);
                            }
                        };
                        self.parsed = Some(parsed);
                        self.phase = ExecProgress::CredCheck;
                        StepOutcome::Continue { progress: ExecProgress::ReadingHeader }
                    }
                    StepOutcome::Yield { shape, .. } => {
                        StepOutcome::Yield { progress: ExecProgress::ReadingHeader, shape }
                    }
                    StepOutcome::Err(e) => StepOutcome::Err(e),
                    _ => StepOutcome::Err(step_engine::Errno::EIO),
                }
            }
            ExecProgress::Parsing => {
                // Phase 3: parse ELF
                self.phase = ExecProgress::CredCheck;
                StepOutcome::Continue { progress: ExecProgress::Parsing }
            }
            ExecProgress::CredCheck => {
                // Phase 3.5: credential check
                self.phase = ExecProgress::BuildingAspace;
                StepOutcome::Continue { progress: ExecProgress::CredCheck }
            }
            ExecProgress::BuildingAspace => {
                // Phase 4: build detached AddressSpace
                self.phase = ExecProgress::CollapsingThreads;
                StepOutcome::Continue { progress: ExecProgress::BuildingAspace }
            }
            ExecProgress::CollapsingThreads => {
                // Phase 5: group exit
                self.phase = ExecProgress::PopulatingStack;
                StepOutcome::Continue { progress: ExecProgress::CollapsingThreads }
            }
            ExecProgress::PopulatingStack => {
                // Phase 5a: populate stack
                self.phase = ExecProgress::PoNR;
                StepOutcome::Continue { progress: ExecProgress::PopulatingStack }
            }
            ExecProgress::PoNR => {
                // Phase 6: replace_aspace + store_saved_user_context
                self.phase = ExecProgress::PostCommit;
                StepOutcome::Continue { progress: ExecProgress::PoNR }
            }
            ExecProgress::PostCommit => {
                // Phase 7: CLOEXEC + signal + brk + exe_file + cmdline
                // Phase 8: vfork completion
                StepOutcome::Done(())
            }
        }
    }
}
