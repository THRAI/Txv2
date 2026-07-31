//! TTY Phase C execution steps.
//!
//! These are synchronous step entry points over already-copied byte slices.
//! VFS/UserBuf integration in Phase D can wrap these functions after resolving
//! fd permissions and copying user memory.

mod register_hardware;
mod step_hangup;
mod step_ingest;
mod step_ioctl;
mod step_master_close;
mod step_openpty;
mod step_poll_hardware;
mod step_read;
mod step_write;

pub use crate::tty::notification::{TTY_DEFERRED_SIGNAL, TTY_READABLE, TTY_WRITABLE};
pub use register_hardware::{
    register_console_alias, register_hardware, register_hardware_with_winsize,
};
pub use step_hangup::{step_hangup, HangupOutcome};
pub use step_ingest::{step_ingest_with_post, DeferredSignalEvent, IngestOutcome};
pub use step_ioctl::{
    deferred_signal_for_tty, deliver_signal_dispatch_for_process, step_ioctl_tcgets,
    step_ioctl_tcsets, step_ioctl_tiocgpgrp, step_ioctl_tiocgwinsz, step_ioctl_tiocnotty,
    step_ioctl_tiocnotty_for_process, step_ioctl_tiocsctty, step_ioctl_tiocsctty_for_process,
    step_ioctl_tiocspgrp, step_ioctl_tiocspgrp_for_process, step_ioctl_tiocswinsz, IoctlCaller,
    IoctlSideEffect, IoctlTcgetsOp, IoctlTcsetsOp, IoctlTiocgpgrpOp, IoctlTiocgwinszOp,
    IoctlTiocnottyOp, IoctlTiocscttyForProcessOp, IoctlTiocspgrpForProcessOp,
    IoctlTiocswinszOp, JobControlSignal, SessionCtlEvent, SignalDispatch, SignalTarget,
};
pub use step_master_close::step_master_close_last;
pub use step_openpty::{step_openpty, OpenPtyOutcome};
pub use step_poll_hardware::{step_poll_hardware_input, HardwarePollOutcome};
pub use step_read::{
    step_read, step_read_after_vtime, step_read_after_vtime_for_process, step_read_for_caller,
    step_read_for_process, tty_read_wait_plan, tty_read_would_complete, ReadForProcessAfterVtimeOp,
    ReadForProcessOp, TtyReadWaitPlan,
};
pub use step_write::{
    step_write, step_write_for_caller, step_write_for_process, WriteForProcessOp,
};
