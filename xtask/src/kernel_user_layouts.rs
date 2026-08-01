use std::path::Path;

use crate::Result;
use crate::util::run_cmd_owned;

pub(crate) fn kernel_user_layouts(root: &Path, args: Vec<String>) -> Result<()> {
    let script = root.join("tools/check-kernel-user-layouts.py");
    if !script.exists() {
        return Err(format!("missing {}", script.display()));
    }
    let mut argv = vec![script.to_string_lossy().into_owned()];
    argv.extend(args);
    run_cmd_owned(root, "python3", &argv)
}
