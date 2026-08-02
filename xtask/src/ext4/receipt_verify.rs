use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Result;

use super::{is_real_sha256, read_json, sha256_file};

pub(crate) fn verify_tier1_receipt(receipt_path: &Path) -> Result<()> {
    let receipt = read_json(receipt_path)?;
    require_schema(
        &receipt,
        "tx.ext4.tier1_acceptance_receipt.v1",
        receipt_path,
    )?;
    let receipt_sha256 = sha256_file(receipt_path)?;
    let receipt_object = json_object(&receipt, "receipt", receipt_path)?;
    let candidate = required_json_object(receipt_object, "candidate", receipt_path)?;
    let run_id = required_json_string(candidate, "run_id", receipt_path)?;
    verify_authority_digests(receipt_object, receipt_path)?;
    verify_gates_passed(receipt_object, receipt_path)?;
    verify_crash_cut_summary(receipt_object, receipt_path)?;
    verify_xfstests_summary(receipt_object, receipt_path)?;

    let artifact_manifest =
        required_json_object(receipt_object, "artifact_manifest", receipt_path)?;
    let manifest_path = PathBuf::from(required_json_string(
        artifact_manifest,
        "path",
        receipt_path,
    )?);
    let manifest_sha256 = required_json_string(artifact_manifest, "sha256", receipt_path)?;
    verify_real_sha("artifact_manifest.sha256", &manifest_sha256, receipt_path)?;
    verify_file_digest(
        &manifest_path,
        &manifest_sha256,
        "artifact manifest",
        receipt_path,
    )?;

    let lock_path = receipt_path
        .parent()
        .ok_or_else(|| {
            format!(
                "{}: receipt has no parent directory",
                receipt_path.display()
            )
        })?
        .join("receipt-lock.json");
    verify_receipt_lock(
        receipt_path,
        &receipt_sha256,
        &manifest_path,
        &manifest_sha256,
        &lock_path,
    )?;

    let artifact_manifest_json = read_json(&manifest_path)?;
    let artifacts = verify_artifact_manifest(&artifact_manifest_json, &run_id, &manifest_path)?;
    verify_role_images(receipt_object, &artifacts, receipt_path)?;
    verify_e2fsck_summary(receipt_object, &artifacts, receipt_path)?;
    verify_required_log_artifacts(&artifacts, receipt_path)?;
    Ok(())
}

fn verify_receipt_lock(
    receipt_path: &Path,
    receipt_sha256: &str,
    manifest_path: &Path,
    manifest_sha256: &str,
    lock_path: &Path,
) -> Result<()> {
    let lock = read_json(lock_path)?;
    require_schema(&lock, "tx.ext4.tier1_receipt_lock.v1", lock_path)?;
    let lock_object = json_object(&lock, "receipt lock", lock_path)?;
    let receipt = required_json_object(lock_object, "receipt", lock_path)?;
    let locked_receipt_path = required_json_string(receipt, "path", lock_path)?;
    if locked_receipt_path != receipt_path.display().to_string() {
        return Err(format!(
            "{}: receipt.path mismatch: expected {}, found {}",
            lock_path.display(),
            receipt_path.display(),
            locked_receipt_path
        ));
    }
    let locked_receipt_sha = required_json_string(receipt, "sha256", lock_path)?;
    if locked_receipt_sha != receipt_sha256 {
        return Err(format!(
            "{}: receipt.sha256 mismatch for {}",
            lock_path.display(),
            receipt_path.display()
        ));
    }

    let manifest = required_json_object(lock_object, "artifact_manifest", lock_path)?;
    let locked_manifest_path = required_json_string(manifest, "path", lock_path)?;
    if locked_manifest_path != manifest_path.display().to_string() {
        return Err(format!(
            "{}: artifact_manifest.path mismatch: expected {}, found {}",
            lock_path.display(),
            manifest_path.display(),
            locked_manifest_path
        ));
    }
    let locked_manifest_sha = required_json_string(manifest, "sha256", lock_path)?;
    if locked_manifest_sha != manifest_sha256 {
        return Err(format!(
            "{}: artifact_manifest.sha256 mismatch for {}",
            lock_path.display(),
            manifest_path.display()
        ));
    }
    Ok(())
}

fn verify_authority_digests(
    receipt: &serde_json::Map<String, serde_json::Value>,
    path: &Path,
) -> Result<()> {
    let authorities = required_json_object(receipt, "authorities", path)?;
    for key in [
        "capability_ledger_sha256",
        "crash_cut_catalog_sha256",
        "xfstests_selection_sha256",
        "shell_scenario_sha256",
    ] {
        let digest = required_json_string(authorities, key, path)?;
        verify_real_sha(key, &digest, path)?;
    }
    Ok(())
}

fn verify_gates_passed(
    receipt: &serde_json::Map<String, serde_json::Value>,
    path: &Path,
) -> Result<()> {
    let gates = required_json_object(receipt, "gates", path)?;
    for gate in ["G0", "G1", "G2", "G3", "G4", "G5", "G6", "G7"] {
        let status = required_json_string(gates, gate, path)?;
        if status != "passed" {
            return Err(format!(
                "{}: gate {gate} is {status}, expected passed",
                path.display()
            ));
        }
    }
    Ok(())
}

fn verify_crash_cut_summary(
    receipt: &serde_json::Map<String, serde_json::Value>,
    path: &Path,
) -> Result<()> {
    let crash_cuts = required_json_object(receipt, "crash_cuts", path)?;
    let completed = required_json_usize(crash_cuts, "completed", path)?;
    let required = required_json_usize(crash_cuts, "required", path)?;
    if completed != 1000 || required != 1000 {
        return Err(format!(
            "{}: crash_cuts must be completed=1000 required=1000, found completed={completed} required={required}",
            path.display()
        ));
    }
    Ok(())
}

fn verify_xfstests_summary(
    receipt: &serde_json::Map<String, serde_json::Value>,
    path: &Path,
) -> Result<()> {
    let xfstests = required_json_object(receipt, "xfstests", path)?;
    for key in ["skipped", "not_run", "failed"] {
        let value = required_json_usize(xfstests, key, path)?;
        if value != 0 {
            return Err(format!(
                "{}: xfstests.{key} is {value}, expected 0",
                path.display()
            ));
        }
    }
    let passed = required_json_usize(xfstests, "passed", path)?;
    if passed == 0 {
        return Err(format!(
            "{}: xfstests.passed must be nonzero",
            path.display()
        ));
    }
    Ok(())
}

fn verify_artifact_manifest(
    manifest: &serde_json::Value,
    run_id: &str,
    path: &Path,
) -> Result<BTreeMap<String, ArtifactRecord>> {
    require_schema(manifest, "tx.ext4.tier1_artifacts.v1", path)?;
    let object = json_object(manifest, "artifact manifest", path)?;
    let manifest_run_id = required_json_string(object, "run_id", path)?;
    if manifest_run_id != run_id {
        return Err(format!(
            "{}: run_id mismatch: expected {run_id}, found {manifest_run_id}",
            path.display()
        ));
    }
    let artifacts = object
        .get("artifacts")
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: missing artifacts array", path.display()))?;
    if artifacts.is_empty() {
        return Err(format!("{}: artifacts array is empty", path.display()));
    }
    let mut by_name = BTreeMap::new();
    for artifact in artifacts {
        let artifact = artifact
            .as_object()
            .ok_or_else(|| format!("{}: artifact row must be an object", path.display()))?;
        let name = required_json_string(artifact, "name", path)?;
        let artifact_path = PathBuf::from(required_json_string(artifact, "path", path)?);
        let sha256 = required_json_string(artifact, "sha256", path)?;
        verify_real_sha(&format!("artifact {name} sha256"), &sha256, path)?;
        verify_file_digest(&artifact_path, &sha256, &format!("artifact {name}"), path)?;
        if by_name
            .insert(
                name.clone(),
                ArtifactRecord {
                    path: artifact_path,
                    sha256,
                },
            )
            .is_some()
        {
            return Err(format!(
                "{}: duplicate artifact name {name}",
                path.display()
            ));
        }
    }
    Ok(by_name)
}

fn verify_role_images(
    receipt: &serde_json::Map<String, serde_json::Value>,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let role_images = required_json_object(receipt, "role_images", path)?;
    for (role, artifact_name) in [
        ("test", "test-image"),
        ("scratch", "scratch-image"),
        ("workload", "workload-image"),
    ] {
        let image = required_json_object(role_images, role, path)?;
        let image_path = PathBuf::from(required_json_string(image, "path", path)?);
        let image_sha = required_json_string(image, "sha256", path)?;
        verify_real_sha(&format!("role_images.{role}.sha256"), &image_sha, path)?;
        verify_file_digest(&image_path, &image_sha, &format!("role image {role}"), path)?;
        let artifact = artifacts
            .get(artifact_name)
            .ok_or_else(|| format!("{}: missing artifact {artifact_name}", path.display()))?;
        if artifact.path != image_path || artifact.sha256 != image_sha {
            return Err(format!(
                "{}: artifact {artifact_name} does not match role_images.{role}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn verify_e2fsck_summary(
    receipt: &serde_json::Map<String, serde_json::Value>,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let e2fsck = required_json_object(receipt, "e2fsck", path)?;
    let failures = required_json_usize(e2fsck, "failures", path)?;
    if failures != 0 {
        return Err(format!(
            "{}: e2fsck.failures is {failures}, expected 0",
            path.display()
        ));
    }
    let images = e2fsck
        .get("immutable_images")
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: missing e2fsck.immutable_images", path.display()))?;
    if images.len() < 1003 {
        return Err(format!(
            "{}: e2fsck.immutable_images has {}, expected at least 1003",
            path.display(),
            images.len()
        ));
    }

    let mut by_role = BTreeMap::new();
    for image in images {
        let image = image
            .as_object()
            .ok_or_else(|| format!("{}: e2fsck image row must be an object", path.display()))?;
        let role = required_json_string(image, "role", path)?;
        let image_sha = required_json_string(image, "image_sha256", path)?;
        let exit_code = required_json_i32(image, "exit_code", path)?;
        if exit_code != 0 {
            return Err(format!(
                "{}: e2fsck image {role} exit_code is {exit_code}, expected 0",
                path.display()
            ));
        }
        verify_real_sha(&format!("e2fsck image {role} sha256"), &image_sha, path)?;
        if by_role.insert(role.clone(), image_sha).is_some() {
            return Err(format!(
                "{}: duplicate e2fsck image role {role}",
                path.display()
            ));
        }
    }

    for (role, artifact_name) in [
        ("test", "test-image"),
        ("scratch", "scratch-image"),
        ("workload", "workload-image"),
    ] {
        verify_e2fsck_role_artifact(role, artifact_name, &by_role, artifacts, path)?;
    }
    for idx in 0..1000 {
        let role = format!("crash-cut-{idx:04}");
        let artifact_name = format!("{role}-replay-image");
        verify_e2fsck_role_artifact(&role, &artifact_name, &by_role, artifacts, path)?;
    }
    Ok(())
}

fn verify_e2fsck_role_artifact(
    role: &str,
    artifact_name: &str,
    e2fsck_by_role: &BTreeMap<String, String>,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let image_sha = e2fsck_by_role
        .get(role)
        .ok_or_else(|| format!("{}: missing e2fsck image role {role}", path.display()))?;
    let artifact = artifacts
        .get(artifact_name)
        .ok_or_else(|| format!("{}: missing artifact {artifact_name}", path.display()))?;
    if &artifact.sha256 != image_sha {
        return Err(format!(
            "{}: artifact {artifact_name} sha256 does not match e2fsck role {role}",
            path.display()
        ));
    }
    Ok(())
}

fn verify_required_log_artifacts(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    for name in [
        "crash-campaign-plan",
        "crash-cut-outcomes",
        "e2fsck-test-log",
        "e2fsck-scratch-log",
        "e2fsck-workload-log",
        "xfstests-log",
    ] {
        if !artifacts.contains_key(name) {
            return Err(format!(
                "{}: missing required artifact {name}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn require_schema(value: &serde_json::Value, expected: &str, path: &Path) -> Result<()> {
    let schema = value
        .get("schema")
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("{}: missing schema", path.display()))?;
    if schema != expected {
        return Err(format!(
            "{}: expected schema {expected}, found {schema}",
            path.display()
        ));
    }
    Ok(())
}

fn json_object<'a>(
    value: &'a serde_json::Value,
    label: &str,
    path: &Path,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    value
        .as_object()
        .ok_or_else(|| format!("{}: {label} must be a JSON object", path.display()))
}

fn required_json_object<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<&'a serde_json::Map<String, serde_json::Value>> {
    object
        .get(key)
        .and_then(|value| value.as_object())
        .ok_or_else(|| format!("{}: missing object {key}", path.display()))
}

fn required_json_string(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<String> {
    object
        .get(key)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{}: missing string {key}", path.display()))
}

fn required_json_usize(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<usize> {
    object
        .get(key)
        .and_then(|value| value.as_u64())
        .ok_or_else(|| format!("{}: missing integer {key}", path.display()))
        .and_then(|raw| {
            usize::try_from(raw).map_err(|_| format!("{}: {key} out of range", path.display()))
        })
}

fn required_json_i32(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<i32> {
    object
        .get(key)
        .and_then(|value| value.as_i64())
        .ok_or_else(|| format!("{}: missing integer {key}", path.display()))
        .and_then(|raw| {
            i32::try_from(raw).map_err(|_| format!("{}: {key} out of range", path.display()))
        })
}

fn verify_real_sha(label: &str, value: &str, path: &Path) -> Result<()> {
    if !is_real_sha256(value) {
        return Err(format!(
            "{}: {label} is not a nonzero sha256 digest",
            path.display()
        ));
    }
    Ok(())
}

fn verify_file_digest(
    artifact_path: &Path,
    expected_sha256: &str,
    label: &str,
    context: &Path,
) -> Result<()> {
    if !artifact_path.is_file() {
        return Err(format!(
            "{}: {label} is missing: {}",
            context.display(),
            artifact_path.display()
        ));
    }
    let actual = sha256_file(artifact_path)?;
    if actual != expected_sha256 {
        return Err(format!(
            "{}: {label} sha256 mismatch for {}: expected {}, found {}",
            context.display(),
            artifact_path.display(),
            expected_sha256,
            actual
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct ArtifactRecord {
    path: PathBuf,
    sha256: String,
}
