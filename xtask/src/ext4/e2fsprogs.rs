use std::path::Path;
use std::process::Command;

use crate::util::{command_exists, command_or_candidates, shell_join};
use crate::Result;

use super::{DEFAULT_XFSTESTS_DOCKER_IMAGE, XFSTESTS_DOCKER_IMAGE_ENV};

const E2FSPROGS_DOCKER_WRAPPER: &str = "tools/ext4/e2fsprogs_docker.py";
const E2FSPROGS_CANDIDATES: &[&str] = &[
    "/opt/homebrew/opt/e2fsprogs/sbin/e2fsck",
    "/opt/homebrew/sbin/e2fsck",
    "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/e2fsck",
    "/usr/local/opt/e2fsprogs/sbin/e2fsck",
    "/usr/local/sbin/e2fsck",
];
const DEBUGFS_CANDIDATES: &[&str] = &[
    "/opt/homebrew/opt/e2fsprogs/sbin/debugfs",
    "/opt/homebrew/sbin/debugfs",
    "/opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/debugfs",
    "/usr/local/opt/e2fsprogs/sbin/debugfs",
    "/usr/local/sbin/debugfs",
];

#[derive(Clone, Debug)]
pub(crate) struct ToolCommand {
    program: String,
    args: Vec<String>,
}

impl ToolCommand {
    pub(super) fn program(&self) -> &str {
        &self.program
    }

    pub(super) fn args_with(&self, tail: &[String]) -> Vec<String> {
        let mut args = self.args.clone();
        args.extend(tail.iter().cloned());
        args
    }

    pub(super) fn env_command(&self) -> String {
        let mut rendered = vec![self.program.clone()];
        rendered.extend(self.args.iter().cloned());
        shell_join(&rendered)
    }
}

pub(super) fn resolve_e2fsck(root: &Path) -> Result<ToolCommand> {
    resolve_tool(root, "e2fsck", E2FSPROGS_CANDIDATES)
}

pub(super) fn resolve_debugfs(root: &Path) -> Result<ToolCommand> {
    resolve_tool(root, "debugfs", DEBUGFS_CANDIDATES)
}

fn resolve_tool(root: &Path, tool: &str, candidates: &[&str]) -> Result<ToolCommand> {
    if let Some(program) = command_or_candidates(tool, candidates) {
        return Ok(ToolCommand {
            program,
            args: Vec::new(),
        });
    }
    let wrapper = root.join(E2FSPROGS_DOCKER_WRAPPER);
    if !wrapper.is_file() {
        return Err(format!(
            "{tool} is unavailable and Docker wrapper {} is missing",
            wrapper.display()
        ));
    }
    if !command_exists("python3") || !command_exists("docker") {
        return Err(format!(
            "{tool} is unavailable; Docker fallback requires python3 and docker"
        ));
    }
    let image = std::env::var(XFSTESTS_DOCKER_IMAGE_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_XFSTESTS_DOCKER_IMAGE.to_string());
    let image_available = Command::new("docker")
        .args(["image", "inspect", &image])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    if !image_available {
        return Err(format!(
            "{tool} is unavailable and Docker image {image} is not available"
        ));
    }
    Ok(ToolCommand {
        program: "python3".to_string(),
        args: vec![
            wrapper.display().to_string(),
            "--tool".to_string(),
            tool.to_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::ToolCommand;

    #[test]
    fn docker_tool_command_preserves_prefix_before_e2fsprogs_arguments() {
        let command = ToolCommand {
            program: "python3".into(),
            args: vec![
                "/repo/tools/ext4/e2fsprogs_docker.py".into(),
                "--tool".into(),
                "e2fsck".into(),
            ],
        };
        let args = command.args_with(&["-fn".into(), "/tmp/image.ext4".into()]);
        assert_eq!(
            args,
            [
                "/repo/tools/ext4/e2fsprogs_docker.py",
                "--tool",
                "e2fsck",
                "-fn",
                "/tmp/image.ext4"
            ]
        );
        assert!(command.env_command().contains("--tool e2fsck"));
    }
}
