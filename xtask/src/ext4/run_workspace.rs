use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::receipt::{Tier1AcceptanceReceipt, Tier1AuthorityInputs, authority_input_summary};
use crate::Result;

#[derive(Debug)]
pub(crate) struct RunWorkspace {
    run_id: String,
    final_dir: PathBuf,
    temporary: PathBuf,
    child_processes: Vec<Child>,
    artifacts: BTreeMap<String, PathBuf>,
    authorities: Option<Tier1AuthorityInputs>,
    failure_reason: Option<String>,
    finalized: bool,
}

#[allow(dead_code)]
impl RunWorkspace {
    pub(crate) fn create(root: &Path, run_id: &str) -> Result<Self> {
        let base = root.join("target/ext4/tier1");
        let final_dir = base.join(run_id);
        let temporary = base.join(format!(".{run_id}.tmp"));
        if final_dir.exists() {
            return Err(format!(
                "run workspace already exists: {}",
                final_dir.display()
            ));
        }
        if temporary.exists() {
            fs::remove_dir_all(&temporary)
                .map_err(|err| format!("failed to remove stale {}: {err}", temporary.display()))?;
        }
        fs::create_dir_all(&temporary)
            .map_err(|err| format!("failed to create {}: {err}", temporary.display()))?;
        Ok(Self {
            run_id: run_id.into(),
            final_dir,
            temporary,
            child_processes: Vec::new(),
            artifacts: BTreeMap::new(),
            authorities: None,
            failure_reason: None,
            finalized: false,
        })
    }

    pub(crate) fn record_authority_inputs(&mut self, inputs: &Tier1AuthorityInputs) -> Result<()> {
        self.authorities = Some(inputs.clone());
        Ok(())
    }

    pub(crate) fn record_artifact(&mut self, name: impl Into<String>, path: PathBuf) -> Result<()> {
        self.artifacts.insert(name.into(), path);
        Ok(())
    }

    pub(crate) fn spawn_test_child(&mut self, command: &str) -> Result<Child> {
        let child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("failed to spawn test child: {err}"))?;
        Ok(child)
    }

    pub(crate) fn record_child(&mut self, child: Child) {
        self.child_processes.push(child);
    }

    pub(crate) fn mark_failed_for_test(&mut self, reason: &str) {
        self.failure_reason = Some(reason.into());
    }

    pub(crate) fn temporary_path_for_test(&self) -> &Path {
        &self.temporary
    }

    pub(crate) fn finalize(&mut self) -> Result<PathBuf> {
        let receipt = self.placeholder_receipt("finalized")?;
        self.finalize_with_receipt(receipt)
    }

    pub(crate) fn finalize_with_receipt(
        &mut self,
        receipt: Tier1AcceptanceReceipt,
    ) -> Result<PathBuf> {
        self.ensure_not_finalized()?;
        receipt
            .write_json(&self.temporary.join("acceptance-receipt.json"))
            .map_err(|err| format!("failed to write final receipt: {err}"))?;
        if self.final_dir.exists() {
            fs::remove_dir_all(&self.final_dir).map_err(|err| {
                format!(
                    "failed to remove stale final dir {}: {err}",
                    self.final_dir.display()
                )
            })?;
        }
        fs::rename(&self.temporary, &self.final_dir).map_err(|err| {
            format!(
                "failed to finalize run workspace {} -> {}: {err}",
                self.temporary.display(),
                self.final_dir.display()
            )
        })?;
        self.finalized = true;
        Ok(self.final_dir.join("acceptance-receipt.json"))
    }

    fn placeholder_receipt(&self, note: &str) -> Result<Tier1AcceptanceReceipt> {
        let authorities = self.authorities.clone().unwrap_or_else(|| {
            authority_input_summary(
                "0".repeat(64),
                "0".repeat(64),
                "0".repeat(64),
                "0".repeat(64),
                0,
                1000,
            )
        });
        let commit = git_rev_parse_head()?;
        Ok(Tier1AcceptanceReceipt::from_dry_run(
            &self.run_id,
            commit,
            authorities,
            &[format!("workspace note: {note}")],
        ))
    }

    fn ensure_not_finalized(&self) -> Result<()> {
        if self.finalized {
            Err("run workspace already finalized".into())
        } else {
            Ok(())
        }
    }

    fn kill_children(&mut self) {
        for child in &mut self.child_processes {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child_processes.clear();
    }

    fn write_failed_receipt(&self) {
        let authorities = self.authorities.clone().unwrap_or_else(|| {
            authority_input_summary(
                "0".repeat(64),
                "0".repeat(64),
                "0".repeat(64),
                "0".repeat(64),
                0,
                1000,
            )
        });
        let Ok(commit) = git_rev_parse_head() else {
            return;
        };
        let reason = self.failure_reason.as_deref().unwrap_or("dropped");
        let note = format!(
            "failed run; final receipt not produced; reason={reason}; artifacts={}",
            self.artifacts.len()
        );
        let receipt =
            Tier1AcceptanceReceipt::from_dry_run(&self.run_id, commit, authorities, &[note]);
        let _ = fs::create_dir_all(&self.final_dir);
        let _ = receipt.write_json(&self.final_dir.join("failed-receipt.json"));
    }
}

impl Drop for RunWorkspace {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        self.kill_children();
        self.write_failed_receipt();
        let _ = fs::remove_dir_all(&self.temporary);
    }
}

fn git_rev_parse_head() -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|err| format!("failed to run git rev-parse HEAD: {err}"))?;
    if !output.status.success() {
        return Err(format!("git rev-parse HEAD exited with {}", output.status));
    }
    let commit = String::from_utf8(output.stdout)
        .map_err(|err| format!("git rev-parse HEAD produced invalid utf8: {err}"))?;
    Ok(commit.trim().to_string())
}
