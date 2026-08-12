use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::receipt::{authority_input_summary, Tier1AcceptanceReceipt, Tier1AuthorityInputs};
use crate::util::shell_join;
use crate::Result;

#[derive(Debug)]
pub(crate) struct RunWorkspace {
    run_id: String,
    final_dir: PathBuf,
    temporary: PathBuf,
    child_processes: Vec<Child>,
    artifacts: BTreeMap<String, PathBuf>,
    artifact_sha256: BTreeMap<PathBuf, String>,
    authorities: Option<Tier1AuthorityInputs>,
    failure_reason: Option<String>,
    finalized: bool,
}

#[allow(dead_code)]
impl RunWorkspace {
    pub(crate) fn create(root: &Path, run_id: &str) -> Result<Self> {
        Self::create_with_mode(root, run_id, false)
    }

    pub(crate) fn resume(root: &Path, run_id: &str) -> Result<Self> {
        Self::create_with_mode(root, run_id, true)
    }

    fn create_with_mode(root: &Path, run_id: &str, preserve_existing: bool) -> Result<Self> {
        let base = root.join("target/ext4/tier1");
        let final_dir = base.join(run_id);
        let temporary = base.join(format!(".{run_id}.tmp"));
        let mut artifact_sha256 = BTreeMap::new();
        if final_dir.exists() {
            if preserve_existing {
                artifact_sha256 = reopen_failed_final_workspace(&final_dir, &temporary)?;
            } else {
                return Err(format!(
                    "run workspace already exists: {}",
                    final_dir.display()
                ));
            }
        }
        if temporary.exists() && !preserve_existing {
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
            artifact_sha256,
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

    pub(crate) fn record_artifact_with_sha256(
        &mut self,
        name: impl Into<String>,
        path: PathBuf,
        sha256: impl Into<String>,
    ) -> Result<()> {
        let sha256 = sha256.into();
        if !is_real_sha256(&sha256) {
            return Err(format!("invalid artifact sha256 for {}", path.display()));
        }
        self.artifact_sha256.insert(path.clone(), sha256);
        self.record_artifact(name, path)
    }

    pub(crate) fn cached_artifact_sha256(&self, path: &Path) -> Option<&str> {
        self.artifact_sha256.get(path).map(String::as_str)
    }

    pub(crate) fn stage_copy(
        &mut self,
        name: impl Into<String>,
        source: &Path,
        file_name: &str,
    ) -> Result<PathBuf> {
        let destination = self.temporary.join(file_name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
        }
        if destination.is_file() {
            fs::remove_file(&destination)
                .map_err(|err| format!("failed to remove {}: {err}", destination.display()))?;
            self.artifact_sha256.remove(&destination);
        }
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

    pub(crate) fn stage_image_clone(
        &mut self,
        name: impl Into<String>,
        source: &Path,
        file_name: &str,
    ) -> Result<PathBuf> {
        let destination = self.temporary.join(file_name);
        if destination.is_file() {
            fs::remove_file(&destination)
                .map_err(|err| format!("failed to remove {}: {err}", destination.display()))?;
            self.artifact_sha256.remove(&destination);
        }
        copy_image_cow(source, &destination)?;
        self.record_artifact(name, destination.clone())?;
        Ok(destination)
    }

    pub(crate) fn working_dir(&self) -> &Path {
        &self.temporary
    }

    pub(crate) fn stable_path(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.temporary)
            .map(|relative| self.final_dir.join(relative))
            .unwrap_or_else(|_| path.to_path_buf())
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

    pub(crate) fn mark_failed(&mut self, reason: impl Into<String>) {
        self.failure_reason = Some(reason.into());
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
        self.write_receipt_lock(&self.temporary, "acceptance-receipt.json", &manifest)?;
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
            if receipt.write_json(&dir.join("failed-receipt.json")).is_ok() {
                let _ = self.write_receipt_lock(dir, "failed-receipt.json", &manifest);
            }
            return;
        }
        let _ = receipt.write_json(&dir.join("failed-receipt.json"));
    }

    fn write_artifacts_manifest(&self, dir: &Path) -> Result<PathBuf> {
        let artifacts = self
            .artifacts
            .iter()
            .map(|(name, path)| {
                let stable_path = self.stable_path(path);
                let sha256 = if path.is_file() {
                    self.artifact_sha256
                        .get(path)
                        .cloned()
                        .map(Ok)
                        .unwrap_or_else(|| sha256_file(path))?
                } else {
                    sha256_file(path)?
                };
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

    fn write_receipt_lock(
        &self,
        dir: &Path,
        receipt_name: &str,
        manifest_path: &Path,
    ) -> Result<PathBuf> {
        let receipt_path = dir.join(receipt_name);
        let stable_receipt_path = self.final_dir.join(receipt_name);
        let stable_manifest_path = self.final_dir.join("artifacts.json");
        let lock = serde_json::json!({
            "schema": "tx.ext4.tier1_receipt_lock.v1",
            "run_id": self.run_id,
            "receipt": {
                "path": stable_receipt_path.display().to_string(),
                "sha256": sha256_file(&receipt_path)?
            },
            "artifact_manifest": {
                "path": stable_manifest_path.display().to_string(),
                "sha256": sha256_file(manifest_path)?
            }
        });
        let text = serde_json::to_string_pretty(&lock)
            .map_err(|err| format!("failed to encode receipt lock: {err}"))?;
        let path = dir.join("receipt-lock.json");
        fs::write(&path, text).map_err(|err| format!("failed to write receipt lock: {err}"))?;
        Ok(path)
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

pub(super) fn tier1_image_cow_clone_supported(root: &Path) -> bool {
    let probe_dir = root
        .join("target")
        .join("ext4")
        .join("tier1")
        .join("preflight")
        .join(format!(".cow-probe-{}", std::process::id()));
    let source = probe_dir.join("source.img");
    let target = probe_dir.join("target.img");
    let result = (|| -> Result<()> {
        fs::create_dir_all(&probe_dir)
            .map_err(|err| format!("failed to create {}: {err}", probe_dir.display()))?;
        fs::write(&source, [0u8; 4096])
            .map_err(|err| format!("failed to write {}: {err}", source.display()))?;
        copy_image_cow(&source, &target)
    })();
    let _ = fs::remove_dir_all(&probe_dir);
    result.is_ok()
}

fn reopen_failed_final_workspace(
    final_dir: &Path,
    temporary: &Path,
) -> Result<BTreeMap<PathBuf, String>> {
    let acceptance_receipt = final_dir.join("acceptance-receipt.json");
    if acceptance_receipt.exists() {
        return Err(format!(
            "run workspace already finalized with acceptance receipt: {}",
            final_dir.display()
        ));
    }
    if temporary.exists() {
        return Err(format!(
            "cannot resume {}; temporary workspace also exists: {}",
            final_dir.display(),
            temporary.display()
        ));
    }
    let artifact_sha256 = load_failed_artifact_sha256_cache(final_dir, temporary)?;
    fs::rename(final_dir, temporary).map_err(|err| {
        format!(
            "failed to reopen failed run workspace {} -> {}: {err}",
            final_dir.display(),
            temporary.display()
        )
    })?;
    for name in ["failed-receipt.json", "artifacts.json", "receipt-lock.json"] {
        let stale = temporary.join(name);
        if stale.exists() {
            fs::remove_file(&stale)
                .map_err(|err| format!("failed to remove stale {}: {err}", stale.display()))?;
        }
    }
    Ok(artifact_sha256)
}

fn load_failed_artifact_sha256_cache(
    final_dir: &Path,
    temporary: &Path,
) -> Result<BTreeMap<PathBuf, String>> {
    let manifest_path = final_dir.join("artifacts.json");
    if !manifest_path.is_file() {
        return Ok(BTreeMap::new());
    }
    let Ok(text) = fs::read_to_string(&manifest_path) else {
        return Ok(BTreeMap::new());
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(BTreeMap::new());
    };
    let Some(artifacts) = value.get("artifacts").and_then(|value| value.as_array()) else {
        return Ok(BTreeMap::new());
    };
    let mut cache = BTreeMap::new();
    for artifact in artifacts {
        let Some(path) = artifact.get("path").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(sha256) = artifact.get("sha256").and_then(|value| value.as_str()) else {
            continue;
        };
        if !is_real_sha256(sha256) {
            continue;
        }
        let path = PathBuf::from(path);
        let runtime_path = path
            .strip_prefix(final_dir)
            .map(|relative| temporary.join(relative))
            .unwrap_or(path);
        cache.insert(runtime_path, sha256.to_string());
    }
    Ok(cache)
}

pub(super) fn copy_image_cow(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_file() {
        return Err(format!("missing image source: {}", source.display()));
    }
    if destination.exists() {
        return Err(format!(
            "refusing to overwrite existing image clone target: {}",
            destination.display()
        ));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    let args = cow_clone_command_args(source, destination)?;
    let status = Command::new(&args[0])
        .args(&args[1..])
        .status()
        .map_err(|err| format!("failed to launch {}: {err}", shell_join(&args)))?;
    if !status.success() {
        return Err(format!("{} exited with {status}", shell_join(&args)));
    }
    Ok(())
}

fn cow_clone_command_args(source: &Path, destination: &Path) -> Result<Vec<String>> {
    let source = source.display().to_string();
    let destination = destination.display().to_string();
    if cfg!(target_os = "macos") {
        Ok(vec!["cp".into(), "-c".into(), source, destination])
    } else if cfg!(target_os = "linux") {
        Ok(vec![
            "cp".into(),
            "--reflink=always".into(),
            source,
            destination,
        ])
    } else {
        Err(format!(
            "CoW image clone is required for Tier 1 image staging on this host OS ({})",
            std::env::consts::OS
        ))
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

fn is_real_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
