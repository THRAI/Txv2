//! Session-leader authorization check used by controlling-tty ioctls.

use crate::execution::Errno;
use crate::tty::execution::IoctlCaller;

pub fn require_session_leader(caller: IoctlCaller) -> Result<(), Errno> {
    if !caller.is_session_leader || caller.has_controlling_tty {
        return Err(Errno::EINVAL);
    }
    Ok(())
}
