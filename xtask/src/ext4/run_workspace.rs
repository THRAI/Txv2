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

    pub(crate) fn stage_copy(
        &mut self,
        name: impl Into<String>,
        source: &Path,
        file_name: &str,
    ) -> Result<PathBuf> {
        let destination = self.temporary.join(file_name);
        fs::copy(source, &destination).map_err(|err| {
            format!(
                "failed to copy {} -> {}: {err}",
                source.display(),
                destination.display()
            )
        })?;
        self.record_artifact(name, destination.clone())?;
        Ok(destination)
    }

    pub(crate) fn working_dir(&self) -> &Path {
        &self.temporary
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
        mut receipt: Tier1AcceptanceReceipt,
    ) -> Result<PathBuf> {
        self.ensure_not_finalized()?;
        let manifest = self.write_artifacts_manifest(&self.temporary)?;
        self.bind_artifact_manifest(&mut receipt, &manifest)?;
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

    fn write_failed_receipt(&self, dir: &Path) {
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
        let mut receipt =
            Tier1AcceptanceReceipt::from_dry_run(&self.run_id, commit, authorities, &[note]);
        let _ = fs::create_dir_all(dir);
        if let Ok(manifest) = self.write_artifacts_manifest(dir) {
            let _ = self.bind_artifact_manifest(&mut receipt, &manifest);
        }
        let _ = receipt.write_json(&dir.join("failed-receipt.json"));
    }

    fn write_artifacts_manifest(&self, dir: &Path) -> Result<PathBuf> {
        let artifacts = self
            .artifacts
            .iter()
            .map(|(name, path)| {
                let stable_path = path
                    .strip_prefix(&self.temporary)
                    .map(|relative| self.final_dir.join(relative))
                    .unwrap_or_else(|_| path.clone());
                let sha256 = sha256_file(path)?;
                Ok(serde_json::json!({
                    "name": name,
                    "path": stable_path.display().to_string(),
                    "sha256": sha256
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let manifest = serde_json::json!({
            "schema": "tx.ext4.tier1_artifacts.v1",
            "run_id": self.run_id,
            "artifacts": artifacts
        });
        let text = serde_json::to_string_pretty(&manifest)
            .map_err(|err| format!("failed to encode artifact manifest: {err}"))?;
        let path = dir.join("artifacts.json");
        fs::write(&path, text)
            .map_err(|err| format!("failed to write artifact manifest: {err}"))?;
        Ok(path)
    }

    fn bind_artifact_manifest(
        &self,
        receipt: &mut Tier1AcceptanceReceipt,
        manifest_path: &Path,
    ) -> Result<()> {
        receipt.bind_artifact_manifest(
            self.final_dir.join("artifacts.json").display().to_string(),
            sha256_file(manifest_path)?,
        );
        Ok(())
    }
}

impl Drop for RunWorkspace {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        self.kill_children();
        self.write_failed_receipt(&self.temporary);
        if self.final_dir.exists() {
            let _ = fs::remove_dir_all(&self.final_dir);
        }
        if self.temporary.exists() {
            let _ = fs::rename(&self.temporary, &self.final_dir);
        } else {
            self.write_failed_receipt(&self.final_dir);
        }
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

fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    Ok(hex_string(tx_ext4_format::capability::sha256(&bytes)))
}

fn hex_string(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
