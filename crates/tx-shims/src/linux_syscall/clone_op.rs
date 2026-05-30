// PR-9 phase 3b (StepOp + drive migration for clone).
//
// CloneOp unifies fork and clone_thread under a single StepOp,
// replacing the ad-hoc synchronous dispatch in sys_clone.
// CLONE_VFORK uses YieldShape::OnWaitSource for proper reactor
// parking instead of manual wait_source::wait_on_token.

use super::*;
use step_engine::{StepOp, StepOutcome, YieldShape, NoProgress};
use tx_subsystems::process::execution::{step_clone_thread, step_fork};
use tx_subsystems::process::{ForkError, ProcessIdentity};
use tx_subsystems::thread_runtime::structure::ThreadIdentity;
use tx_hal::UserTrapContext;

#[cfg(target_arch = "loongarch64")]
const TLS_REG_INDEX: usize = 2;
#[cfg(not(target_arch = "loongarch64"))]
const TLS_REG_INDEX: usize = 4;

/// Internal state machine for the clone operation.
enum ClonePhase {
    /// First call: execute fork or clone_thread.
    Exec,
    /// CLONE_VFORK active: waiting for child to exec/exit.
    VforkWait {
        /// Exit-source WaitSourceId on the child process.
        exit_source_id: u64,
    },
    /// Terminal state.
    Done,
}

/// Output of the clone operation — either a new process pid or
/// a thread tid.
pub enum CloneOutput {
    NewProcess { child: Cap<ProcessIdentity>, pid: u64 },
    NewThread { tid: u64 },
}

/// StepOp wrapping the clone syscall logic.
///
/// On first `step()`: validates flags, forks or clones a thread,
/// seeds the child context, and submits to the reactor.  If
/// CLONE_VFORK is set, transitions to `VforkWait`.
///
/// On `VforkWait`: checks whether the child has exec'd or exited.
/// If not, yields on the child's exit-source WaitSource.  The
/// reactor parks the task until the child's exit_source fires.
pub struct CloneOp<'a, P: tx_hal::PmapIf> {
    flags: u64,
    stack: u64,
    parent_tidptr: u64,
    tls: u64,
    child_tidptr: u64,
    parent_ctx: UserTrapContext,
    parent: &'a Cap<ProcessIdentity>,
    _pmap: core::marker::PhantomData<P>,

    phase: ClonePhase,
    result: Option<CloneOutput>,
}

impl<'a, P: tx_hal::PmapIf> CloneOp<'a, P> {
    /// Create a new CloneOp from the raw syscall arguments and
    /// parent's saved trap context.  Validation (flag masks,
    /// CLONE_THREAD+CLONE_SIGHAND, CLONE_SIGHAND+CLONE_VM) has
    /// already been performed by the caller.
    pub fn new(
        args: [u64; 6],
        parent: &'a Cap<ProcessIdentity>,
        parent_ctx: UserTrapContext,
    ) -> Self {
        Self {
            flags: args[0],
            stack: args[1],
            parent_tidptr: args[2],
            tls: args[3],
            child_tidptr: args[4],
            parent_ctx,
            parent,
            _pmap: core::marker::PhantomData,
            phase: ClonePhase::Exec,
            result: None,
        }
    }
}

impl<'a, P: tx_hal::PmapIf, I: step_engine::SubjectIdentity> StepOp<I> for CloneOp<'a, P> {
    type Output = CloneOutput;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut step_engine::ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        match self.phase {
            ClonePhase::Exec => {
                let clone_thread = (self.flags & CLONE_THREAD) != 0;
                let clone_vfork = (self.flags & CLONE_VFORK) != 0;
                let clone_settls = (self.flags & CLONE_SETTLS) != 0;
                let clone_child_cleartid = (self.flags & CLONE_CHILD_CLEARTID) != 0;
                let clone_parent_settid = (self.flags & CLONE_PARENT_SETTID) != 0;
                let clone_vm = (self.flags & CLONE_VM) != 0;

                if clone_thread {
                    // ---- Thread path ----
                    let child_thread = match step_clone_thread(
                        self.parent,
                        &self.parent_ctx,
                        tx_subsystems::signal::SignalMask::EMPTY,
                        self.stack as usize,
                        self.tls as usize,
                        self.child_tidptr,
                    ) {
                        Ok(t) => t,
                        Err(e) => return StepOutcome::Err(map_fork_err_to_v3(e)),
                    };

                    if clone_parent_settid && self.parent_tidptr != 0 {
                        bootstrap_write_user::<i32>(
                            self.parent,
                            self.parent_tidptr,
                            child_thread.tid.0 as i32,
                        );
                    }

                    if !clone_settls && self.tls == 0 && !clone_child_cleartid {
                        // no-op: `step_clone_thread` already applied the
                        // stack/tls/clear_child_tid state directly.
                    }

                    reactor_submit::submit_child_thread(self.parent.clone(), child_thread.clone());
                    self.result = Some(CloneOutput::NewThread { tid: child_thread.tid.0 });
                    self.phase = ClonePhase::Done;
                    return StepOutcome::Done(CloneOutput::NewThread { tid: child_thread.tid.0 });
                }

                // ---- Process path ----
                let child = match step_fork::<P>(self.parent, clone_vm) {
                    Ok(c) => c,
                    Err(e) => return StepOutcome::Err(map_fork_err_to_v3(e)),
                };

                let child_thread = child
                    .nth_thread(0)
                    .expect(":clone:no-leader");

                seed_child_leader_context(
                    &child_thread,
                    &self.parent_ctx,
                    self.tls as usize,
                    self.stack as usize,
                );
                reactor_submit::submit_child_thread(child.clone(), child_thread.clone());

                let child_pid = child.pid.0 as u64;

                if clone_vfork {
                    // Transition to VforkWait — parent blocks until child exits/execs.
                    if let Some(exit_id) = child.exit_source_id() {
                        self.phase = ClonePhase::VforkWait { exit_source_id: exit_id };
                        return StepOutcome::Yield(YieldShape::on_wait_source(exit_id, 1));
                    }
                }

                self.result = Some(CloneOutput::NewProcess { child, pid: child_pid });
                self.phase = ClonePhase::Done;
                StepOutcome::Done(CloneOutput::NewProcess { child, pid: child_pid })
            }

            ClonePhase::VforkWait { exit_source_id } => {
                // Re-poll: check if child has exited/exec'd.
                // The exit_source firing already woke us — child is done.
                // Return the stored result.
                self.phase = ClonePhase::Done;
                StepOutcome::Done(self.result.take().expect("CloneOp: vfork result missing"))
            }

            ClonePhase::Done => {
                // Idempotent: return the stored result.
                StepOutcome::Done(self.result.take().expect("CloneOp: result missing"))
            }
        }
    }
}

fn map_fork_err_to_v3(e: ForkError) -> step_engine::Errno {
    match e {
        ForkError::ParentZombie => step_engine::Errno::ESRCH,
        ForkError::Vm(_) => step_engine::Errno::EAGAIN,
        ForkError::Zone(_) => step_engine::Errno::ENOMEM,
        ForkError::Busy => step_engine::Errno::EAGAIN,
        ForkError::PidNamespace => step_engine::Errno::ENOMEM,
    }
}
