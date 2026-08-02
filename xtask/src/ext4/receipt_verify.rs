use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Result;

use super::{crash_campaign::parse_fault_job_result, is_real_sha256, read_json, sha256_file};

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
    let run_dir = receipt_path
        .parent()
        .ok_or_else(|| {
            format!(
                "{}: receipt has no parent directory",
                receipt_path.display()
            )
        })?
        .canonicalize()
        .map_err(|err| {
            format!(
                "failed to canonicalize receipt run directory {}: {err}",
                receipt_path.display()
            )
        })?;
    let manifest_canonical =
        canonical_existing_path(&manifest_path, "artifact manifest", receipt_path)?;
    if manifest_canonical.parent() != Some(run_dir.as_path()) {
        return Err(format!(
            "{}: artifact manifest is outside receipt run directory {}: {}",
            receipt_path.display(),
            run_dir.display(),
            manifest_path.display()
        ));
    }

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
    let artifacts =
        verify_artifact_manifest(&artifact_manifest_json, &run_id, &manifest_path, &run_dir)?;
    verify_authority_digests(receipt_object, &artifacts, receipt_path)?;
    verify_role_images(receipt_object, &artifacts, receipt_path)?;
    verify_e2fsck_summary(receipt_object, &artifacts, receipt_path)?;
    verify_authority_ledger_evidence(&artifacts, receipt_path)?;
    verify_shell_scenario_authority_evidence(&artifacts, receipt_path)?;
    verify_required_log_artifacts(&artifacts, receipt_path)?;
    verify_g0_lint_log_evidence(&artifacts, receipt_path)?;
    verify_build_log_evidence(&artifacts, receipt_path)?;
    verify_guest_matrix_serial_evidence(&artifacts, receipt_path)?;
    verify_xfstests_source_lock_evidence(&artifacts, receipt_path)?;
    verify_xfstests_log_evidence(receipt_object, &artifacts, receipt_path)?;
    verify_crash_cut_outcome_manifest(&artifacts, receipt_path)?;
    verify_required_crash_cut_artifacts(&artifacts, receipt_path)?;
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
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let authorities = required_json_object(receipt, "authorities", path)?;
    for (key, artifact_name) in [
        ("capability_ledger_sha256", "authority-capability-ledger"),
        ("crash_cut_catalog_sha256", "authority-crash-cut-catalog"),
        ("xfstests_selection_sha256", "authority-xfstests-selection"),
        ("shell_scenario_sha256", "authority-shell-scenario"),
    ] {
        let digest = required_json_string(authorities, key, path)?;
        verify_real_sha(key, &digest, path)?;
        let artifact = artifacts
            .get(artifact_name)
            .ok_or_else(|| format!("{}: missing artifact {artifact_name}", path.display()))?;
        if artifact.sha256 != digest {
            return Err(format!(
                "{}: authority {key} does not match artifact {artifact_name}",
                path.display()
            ));
        }
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

fn verify_authority_ledger_evidence(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    verify_capability_ledger_evidence(artifacts, path)?;
    verify_xfstests_authority_acceptance_ready(artifacts, path)?;
    verify_crash_catalog_authority_acceptance_ready(artifacts, path)
}

fn verify_shell_scenario_authority_evidence(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let authority_path = artifacts
        .get("authority-shell-scenario")
        .ok_or_else(|| {
            format!(
                "{}: missing artifact authority-shell-scenario",
                path.display()
            )
        })?
        .path
        .clone();
    let scenario = std::fs::read_to_string(&authority_path)
        .map_err(|err| format!("failed to read {}: {err}", authority_path.display()))?;
    verify_shell_scenario_authority(&scenario, &authority_path)
}

fn verify_shell_scenario_authority(scenario: &str, path: &Path) -> Result<()> {
    for directive in [
        "wait \":mount:sdcard:ext4:ok\"",
        "expect \"tier1-test-role-status:0\"",
        "mount -o ro -t ext4 /dev/block/vdc /musl",
        "tier1-workload-ro-write:ok",
        "mount -t ext4 /dev/block/vdb /musl",
        "expect \"tier1-scratch-mount:0\"",
        "expect \"tier1-data-write:0\"",
        "expect \"tier1-setattr-status:0\"",
        "expect \"tier1-namespace-status:0\"",
        "expect \"tier1-orphan-status:0\"",
        "expect \"tier1-durability-status:0\"",
        "expect \"tier1-remount-status:0\"",
        "expect \"tier1-exec-status:0\"",
        "expect \"tier1-detach-status:0\"",
    ] {
        if !scenario.contains(directive) {
            return Err(format!(
                "{}: shell scenario authority missing directive `{directive}`",
                path.display()
            ));
        }
    }
    Ok(())
}

fn verify_capability_ledger_evidence(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let authority_path = artifacts
        .get("authority-capability-ledger")
        .ok_or_else(|| {
            format!(
                "{}: missing artifact authority-capability-ledger",
                path.display()
            )
        })?
        .path
        .clone();
    let authority = read_json(&authority_path)?;
    require_schema(&authority, "tx.ext4.capability_ledger.v1", &authority_path)?;
    let authority_object = json_object(&authority, "capability ledger", &authority_path)?;
    let profile = required_json_object(authority_object, "profile", &authority_path)?;
    require_usize_value(profile, "block_size", 4096, &authority_path)?;
    require_usize_array_value(profile, "inode_sizes", &[128, 256], &authority_path)?;

    let feature_bits = required_json_object(profile, "feature_bits", &authority_path)?;
    require_usize_value(feature_bits, "compat_allowed", 60, &authority_path)?;
    require_usize_value(feature_bits, "incompat_required", 64, &authority_path)?;
    require_usize_value(feature_bits, "incompat_allowed", 8902, &authority_path)?;
    require_usize_value(feature_bits, "ro_compat_allowed", 1131, &authority_path)?;
    require_bool_value(
        feature_bits,
        "metadata_csum_required",
        true,
        &authority_path,
    )?;
    require_bool_value(feature_bits, "ordered_jbd2_required", true, &authority_path)?;

    let mutation_shapes = required_json_object(profile, "mutation_shapes", &authority_path)?;
    require_field_value(mutation_shapes, "extent", "depth_one", &authority_path)?;
    require_string_array_value(
        mutation_shapes,
        "directory",
        &["linear", "htree_non_splitting"],
        &authority_path,
    )?;
    require_field_value(mutation_shapes, "orphan", "classic", &authority_path)?;
    require_string_array_value(
        authority_object,
        "unsupported",
        &[
            "extent_depth_growth",
            "htree_split",
            "orphan_file",
            "direct_io",
        ],
        &authority_path,
    )?;
    Ok(())
}

fn verify_xfstests_authority_acceptance_ready(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let authority_path = artifacts
        .get("authority-xfstests-selection")
        .ok_or_else(|| {
            format!(
                "{}: missing artifact authority-xfstests-selection",
                path.display()
            )
        })?
        .path
        .clone();
    let authority = read_json(&authority_path)?;
    require_schema(
        &authority,
        "tx.ext4.xfstests_selection_ledger.v1",
        &authority_path,
    )?;
    let authority_object = json_object(&authority, "xfstests selection", &authority_path)?;
    require_authority_status(authority_object, "xfstests selection", &authority_path)?;
    required_json_string(authority_object, "tier", &authority_path)?;
    Ok(())
}

fn verify_crash_catalog_authority_acceptance_ready(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let authority_path = artifacts
        .get("authority-crash-cut-catalog")
        .ok_or_else(|| {
            format!(
                "{}: missing artifact authority-crash-cut-catalog",
                path.display()
            )
        })?
        .path
        .clone();
    let authority = read_json(&authority_path)?;
    require_schema(&authority, "tx.ext4.crash_cut_catalog.v1", &authority_path)?;
    let authority_object = json_object(&authority, "crash-cut catalog", &authority_path)?;
    require_authority_status(authority_object, "crash-cut catalog", &authority_path)?;
    let expanded = required_json_usize(authority_object, "expanded_cut_count", &authority_path)?;
    if expanded != 1000 {
        return Err(format!(
            "{}: crash-cut catalog expanded_cut_count must be 1000, found {expanded}",
            authority_path.display()
        ));
    }
    let campaign = required_json_object(authority_object, "campaign", &authority_path)?;
    require_field_value(
        campaign,
        "kill_policy",
        "deterministic-phase-marker-v1",
        &authority_path,
    )?;
    require_field_value(campaign, "e2fsck_mode", "immutable-copy", &authority_path)?;
    required_json_string(campaign, "workload_script", &authority_path)?;
    required_json_string(campaign, "replay_script", &authority_path)?;
    verify_crash_catalog_families(authority_object, &authority_path)
}

fn require_authority_status(
    object: &serde_json::Map<String, serde_json::Value>,
    label: &str,
    path: &Path,
) -> Result<()> {
    let status = required_json_string(object, "status", path)?;
    if status != "acceptance-ready" {
        return Err(format!(
            "{}: {label} status is `{status}`; expected `acceptance-ready`",
            path.display()
        ));
    }
    Ok(())
}

fn require_field_value(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    expected: &str,
    path: &Path,
) -> Result<()> {
    let found = required_json_string(object, key, path)?;
    if found != expected {
        return Err(format!(
            "{}: {key} must be `{expected}`, found `{found}`",
            path.display()
        ));
    }
    Ok(())
}

fn require_usize_value(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    expected: usize,
    path: &Path,
) -> Result<()> {
    let found = required_json_usize(object, key, path)?;
    if found != expected {
        return Err(format!(
            "{}: {key} must be {expected}, found {found}",
            path.display()
        ));
    }
    Ok(())
}

fn require_bool_value(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    expected: bool,
    path: &Path,
) -> Result<()> {
    let found = object
        .get(key)
        .and_then(|value| value.as_bool())
        .ok_or_else(|| format!("{}: missing bool {key}", path.display()))?;
    if found != expected {
        return Err(format!(
            "{}: {key} must be {expected}, found {found}",
            path.display()
        ));
    }
    Ok(())
}

fn require_usize_array_value(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    expected: &[usize],
    path: &Path,
) -> Result<()> {
    let found = object
        .get(key)
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: missing array {key}", path.display()))?
        .iter()
        .map(|value| {
            value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| format!("{}: {key} entries must be integers", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    if found != expected {
        return Err(format!(
            "{}: {key} must be {:?}, found {:?}",
            path.display(),
            expected,
            found
        ));
    }
    Ok(())
}

fn require_string_array_value(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    expected: &[&str],
    path: &Path,
) -> Result<()> {
    let found = object
        .get(key)
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: missing array {key}", path.display()))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{}: {key} entries must be strings", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let expected = expected
        .iter()
        .copied()
        .map(str::to_string)
        .collect::<Vec<_>>();
    if found != expected {
        return Err(format!(
            "{}: {key} must be {:?}, found {:?}",
            path.display(),
            expected,
            found
        ));
    }
    Ok(())
}

fn verify_crash_catalog_families(
    object: &serde_json::Map<String, serde_json::Value>,
    path: &Path,
) -> Result<()> {
    let families = require_non_empty_array(object, "families", path)?;
    for family in [
        "D0", "D1", "D2", "D3", "D4", "D5", "D6", "D7", "D8", "D9", "D10", "D11", "D12",
    ] {
        let Some(entry) = families.iter().find(|entry| {
            entry
                .get("id")
                .and_then(|value| value.as_str())
                .is_some_and(|id| id == family)
        }) else {
            return Err(format!(
                "{}: crash-cut catalog missing family {family}",
                path.display()
            ));
        };
        let entry = entry.as_object().ok_or_else(|| {
            format!(
                "{}: crash-cut catalog family {family} must be an object",
                path.display()
            )
        })?;
        let marker = required_json_string(entry, "phase_marker", path)?;
        if marker != format!("tx.ext4.crash.phase.{family}") {
            return Err(format!(
                "{}: crash-cut catalog family {family} phase_marker mismatch",
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
    run_dir: &Path,
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
        let artifact_canonical =
            canonical_existing_path(&artifact_path, &format!("artifact {name}"), path)?;
        if !artifact_canonical.starts_with(run_dir) {
            return Err(format!(
                "{}: artifact {name} is outside receipt run directory {}: {}",
                path.display(),
                run_dir.display(),
                artifact_path.display()
            ));
        }
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
        verify_e2fsck_role_log_artifact(role, artifacts, path)?;
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

fn verify_e2fsck_role_log_artifact(
    role: &str,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let artifact_name = format!("e2fsck-{role}-log");
    let artifact = artifacts
        .get(&artifact_name)
        .ok_or_else(|| format!("{}: missing artifact {artifact_name}", path.display()))?;
    let log = std::fs::read_to_string(&artifact.path)
        .map_err(|err| format!("failed to read {}: {err}", artifact.path.display()))?;
    verify_e2fsck_clean_log(role, &log, &artifact.path)
}

fn verify_e2fsck_clean_log(role: &str, log: &str, path: &Path) -> Result<()> {
    let lower = log.to_ascii_lowercase();
    for marker in [
        "unexpected inconsistency",
        "filesystem still has errors",
        "file system still has errors",
        "filesystem was modified",
        "file system was modified",
        "inode bitmap differences",
        "block bitmap differences",
        "free blocks count wrong",
        "free inodes count wrong",
        "directory corrupted",
        "checksum does not match",
    ] {
        if lower.contains(marker) {
            return Err(format!(
                "{}: e2fsck {role} log contains dirty marker `{marker}`",
                path.display()
            ));
        }
    }
    if !log.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.contains(": clean,") || trimmed.contains(" clean,")
    }) {
        return Err(format!(
            "{}: e2fsck {role} log missing clean summary",
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
        "g0-ext4-lifecycle-ownership-log",
        "g0-ext4-no-direct-home-write-log",
        "g0-ext4-durability-flags-log",
        "candidate-full-build-log",
        "busybox-ext4-image-build-log",
        "busybox-base-image",
        "guest-matrix-serial-log",
        "crash-campaign-plan",
        "crash-cut-outcomes",
        "e2fsck-test-log",
        "e2fsck-scratch-log",
        "e2fsck-workload-log",
        "xfstests-source-lock-evidence",
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

fn verify_g0_lint_log_evidence(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    for rule in [
        "ext4-lifecycle-ownership",
        "ext4-no-direct-home-write",
        "ext4-durability-flags",
    ] {
        let artifact_name = format!("g0-{rule}-log");
        let artifact = artifacts
            .get(&artifact_name)
            .ok_or_else(|| format!("{}: missing artifact {artifact_name}", path.display()))?;
        let log = std::fs::read_to_string(&artifact.path)
            .map_err(|err| format!("failed to read {}: {err}", artifact.path.display()))?;
        verify_g0_lint_log(rule, &log, &artifact.path)?;
    }
    Ok(())
}

fn verify_g0_lint_log(rule: &str, log: &str, path: &Path) -> Result<()> {
    let expected_command = format!("$ cargo xtask lint invariants {rule}");
    if !log.lines().any(|line| line.trim() == expected_command) {
        return Err(format!(
            "{}: G0 lint log missing command `{expected_command}`",
            path.display()
        ));
    }
    if !log.lines().any(|line| line.trim() == "exit_code=0") {
        return Err(format!(
            "{}: G0 lint {rule} log missing exit_code=0",
            path.display()
        ));
    }
    Ok(())
}

fn verify_build_log_evidence(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    verify_command_log_artifact(
        artifacts,
        "candidate-full-build-log",
        "$ cargo xtask full-build --target rv64-qemu --skip-doctor",
        "candidate full-build",
        path,
    )?;
    verify_command_log_artifact(
        artifacts,
        "busybox-ext4-image-build-log",
        "$ cargo xtask image ext4 --profile busybox --target rv64-qemu",
        "busybox ext4 image build",
        path,
    )?;
    Ok(())
}

fn verify_command_log_artifact(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    artifact_name: &str,
    expected_command: &str,
    label: &str,
    path: &Path,
) -> Result<()> {
    let artifact = artifacts
        .get(artifact_name)
        .ok_or_else(|| format!("{}: missing artifact {artifact_name}", path.display()))?;
    let log = std::fs::read_to_string(&artifact.path)
        .map_err(|err| format!("failed to read {}: {err}", artifact.path.display()))?;
    verify_command_log(label, expected_command, &log, &artifact.path)
}

fn verify_command_log(label: &str, expected_command: &str, log: &str, path: &Path) -> Result<()> {
    if !log.lines().any(|line| line.trim() == expected_command) {
        return Err(format!(
            "{}: {label} log missing command `{expected_command}`",
            path.display()
        ));
    }
    if !log.lines().any(|line| line.trim() == "exit_code=0") {
        return Err(format!(
            "{}: {label} log missing exit_code=0",
            path.display()
        ));
    }
    Ok(())
}

fn verify_guest_matrix_serial_evidence(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let artifact = artifacts.get("guest-matrix-serial-log").ok_or_else(|| {
        format!(
            "{}: missing artifact guest-matrix-serial-log",
            path.display()
        )
    })?;
    let log = std::fs::read_to_string(&artifact.path)
        .map_err(|err| format!("failed to read {}: {err}", artifact.path.display()))?;
    verify_guest_matrix_serial_log(&log, &artifact.path)
}

fn verify_guest_matrix_serial_log(log: &str, path: &Path) -> Result<()> {
    for marker in [
        "tier1-test-role-status:0",
        "tier1-test-detach:0",
        "tier1-workload-ro-mount:0",
        "tier1-workload-ro-write:ok",
        "tier1-scratch-mount:0",
        "tier1-data-mkdir:0",
        "tier1-data-write:0",
        "alpha",
        "tier1-data-read:0",
        "tier1-setattr-status:0",
        "tier1-namespace-status:0",
        "orphan",
        "tier1-orphan-status:0",
        "tier1-durability-status:0",
        "tier1-remount-status:0",
        "tier1-exec-ok",
        "tier1-exec-status:0",
        "tier1-detach-status:0",
    ] {
        if !log.lines().any(|line| line.trim() == marker) {
            return Err(format!(
                "{}: guest matrix serial log missing marker `{marker}`",
                path.display()
            ));
        }
    }
    Ok(())
}

fn verify_xfstests_source_lock_evidence(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let authority_path = artifacts
        .get("authority-xfstests-selection")
        .ok_or_else(|| {
            format!(
                "{}: missing artifact authority-xfstests-selection",
                path.display()
            )
        })?
        .path
        .clone();
    let authority = read_json(&authority_path)?;
    require_schema(
        &authority,
        "tx.ext4.xfstests_selection_ledger.v1",
        &authority_path,
    )?;
    let authority_object = json_object(&authority, "xfstests selection", &authority_path)?;
    let source_lock = required_json_object(authority_object, "source_lock", &authority_path)?;
    let expected_revision = required_json_string(source_lock, "revision", &authority_path)?;
    verify_hex_string(
        "source_lock.revision",
        &expected_revision,
        40,
        &authority_path,
    )?;
    let expected_check_sha = required_json_string(source_lock, "check_sha256", &authority_path)?;
    verify_real_sha(
        "source_lock.check_sha256",
        &expected_check_sha,
        &authority_path,
    )?;

    let evidence_path = artifacts
        .get("xfstests-source-lock-evidence")
        .ok_or_else(|| {
            format!(
                "{}: missing required artifact xfstests-source-lock-evidence",
                path.display()
            )
        })?
        .path
        .clone();
    let evidence = read_json(&evidence_path)?;
    require_schema(
        &evidence,
        "tx.ext4.xfstests_source_lock_evidence.v1",
        &evidence_path,
    )?;
    let evidence_object = json_object(&evidence, "xfstests source-lock evidence", &evidence_path)?;
    required_json_string(evidence_object, "source_root", &evidence_path)?;
    required_json_string(evidence_object, "check_path", &evidence_path)?;
    let revision = required_json_string(evidence_object, "revision", &evidence_path)?;
    verify_hex_string("revision", &revision, 40, &evidence_path)?;
    if revision != expected_revision {
        return Err(format!(
            "{}: xfstests source-lock revision mismatch: expected {}, found {}",
            evidence_path.display(),
            expected_revision,
            revision
        ));
    }
    let check_sha = required_json_string(evidence_object, "check_sha256", &evidence_path)?;
    verify_real_sha("check_sha256", &check_sha, &evidence_path)?;
    if check_sha != expected_check_sha {
        return Err(format!(
            "{}: xfstests source-lock check_sha256 mismatch: expected {}, found {}",
            evidence_path.display(),
            expected_check_sha,
            check_sha
        ));
    }
    Ok(())
}

fn verify_xfstests_log_evidence(
    receipt: &serde_json::Map<String, serde_json::Value>,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let selected_count = authority_xfstests_selected_count(artifacts, path)?;
    let xfstests = required_json_object(receipt, "xfstests", path)?;
    let receipt_passed = required_json_usize(xfstests, "passed", path)?;
    if receipt_passed != selected_count {
        return Err(format!(
            "{}: xfstests.passed {receipt_passed} does not match authority selected count {selected_count}",
            path.display()
        ));
    }
    let log_artifact = artifacts
        .get("xfstests-log")
        .ok_or_else(|| format!("{}: missing required artifact xfstests-log", path.display()))?;
    let log = std::fs::read_to_string(&log_artifact.path)
        .map_err(|err| format!("failed to read {}: {err}", log_artifact.path.display()))?;
    let log_passed = parse_xfstests_passed_all(&log, &log_artifact.path)?;
    if log_passed != receipt_passed {
        return Err(format!(
            "{}: xfstests log passed count {log_passed} does not match receipt passed count {receipt_passed}",
            log_artifact.path.display()
        ));
    }
    Ok(())
}

fn authority_xfstests_selected_count(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<usize> {
    let authority_path = artifacts
        .get("authority-xfstests-selection")
        .ok_or_else(|| {
            format!(
                "{}: missing artifact authority-xfstests-selection",
                path.display()
            )
        })?
        .path
        .clone();
    let authority = read_json(&authority_path)?;
    require_schema(
        &authority,
        "tx.ext4.xfstests_selection_ledger.v1",
        &authority_path,
    )?;
    let authority_object = json_object(&authority, "xfstests selection", &authority_path)?;
    let selected = require_non_empty_array(authority_object, "selected", &authority_path)?;
    Ok(selected.len())
}

fn parse_xfstests_passed_all(log: &str, path: &Path) -> Result<usize> {
    for line in log.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Not run:") {
            return Err(format!(
                "{}: xfstests log reported not-run cases: {trimmed}",
                path.display()
            ));
        }
        if trimmed.starts_with("Failures:") || trimmed.starts_with("Failed ") {
            return Err(format!(
                "{}: xfstests log reported failures: {trimmed}",
                path.display()
            ));
        }
    }
    for line in log.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("Passed all ") else {
            continue;
        };
        let Some(count) = rest.split_whitespace().next() else {
            continue;
        };
        if let Ok(count) = count.parse::<usize>() {
            return Ok(count);
        }
    }
    Err(format!(
        "{}: xfstests log missing `Passed all N tests` summary",
        path.display()
    ))
}

fn verify_crash_cut_outcome_manifest(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let manifest_path = artifacts
        .get("crash-cut-outcomes")
        .ok_or_else(|| {
            format!(
                "{}: missing required artifact crash-cut-outcomes",
                path.display()
            )
        })?
        .path
        .clone();
    let value = read_json(&manifest_path)?;
    require_schema(
        &value,
        "tx.ext4.crash_cut_outcome_manifest.v1",
        &manifest_path,
    )?;
    let object = json_object(&value, "crash-cut outcome manifest", &manifest_path)?;
    let completed = required_json_usize(object, "completed", &manifest_path)?;
    let required = required_json_usize(object, "required", &manifest_path)?;
    if completed != 1000 || required != 1000 {
        return Err(format!(
            "{}: crash cut outcome counts are completed={completed} required={required}, expected 1000/1000",
            manifest_path.display()
        ));
    }
    require_non_empty_array(object, "families", &manifest_path)?;
    let outcomes = require_non_empty_array(object, "outcomes", &manifest_path)?;
    if outcomes.len() != 1000 {
        return Err(format!(
            "{}: crash cut outcome rows are {}, expected 1000",
            manifest_path.display(),
            outcomes.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (idx, outcome) in outcomes.iter().enumerate() {
        let outcome = outcome.as_object().ok_or_else(|| {
            format!(
                "{}: crash cut outcome row must be an object",
                manifest_path.display()
            )
        })?;
        let expected_cut_id = format!("crash-cut-{idx:04}");
        let cut_id = required_json_string(outcome, "cut_id", &manifest_path)?;
        if cut_id != expected_cut_id {
            return Err(format!(
                "{}: crash cut outcome row {idx} cut_id mismatch: expected {expected_cut_id}, found {cut_id}",
                manifest_path.display()
            ));
        }
        if !seen.insert(cut_id.clone()) {
            return Err(format!(
                "{}: duplicate crash cut outcome {cut_id}",
                manifest_path.display()
            ));
        }
        let image_sha = required_json_string(outcome, "immutable_image_sha256", &manifest_path)?;
        let replay_serial_sha =
            required_json_string(outcome, "replay_serial_sha256", &manifest_path)?;
        verify_real_sha(
            &format!("crash cut {cut_id} immutable_image_sha256"),
            &image_sha,
            &manifest_path,
        )?;
        verify_real_sha(
            &format!("crash cut {cut_id} replay_serial_sha256"),
            &replay_serial_sha,
            &manifest_path,
        )?;
        for (field, expected) in [("e2fsck_exit_code", 0), ("replay_exit_code", 0)] {
            let exit = required_json_i32(outcome, field, &manifest_path)?;
            if exit != expected {
                return Err(format!(
                    "{}: crash cut {cut_id} {field} is {exit}, expected {expected}",
                    manifest_path.display()
                ));
            }
        }
        let replay_artifact = artifacts
            .get(&format!("{cut_id}-replay-image"))
            .ok_or_else(|| {
                format!(
                    "{}: missing replay image artifact for {cut_id}",
                    manifest_path.display()
                )
            })?;
        if replay_artifact.sha256 != image_sha {
            return Err(format!(
                "{}: crash cut {cut_id} immutable_image_sha256 does not match replay-image artifact",
                manifest_path.display()
            ));
        }
        let replay_serial_artifact = artifacts
            .get(&format!("{cut_id}-replay-serial"))
            .ok_or_else(|| {
                format!(
                    "{}: missing replay serial artifact for {cut_id}",
                    manifest_path.display()
                )
            })?;
        if replay_serial_artifact.sha256 != replay_serial_sha {
            return Err(format!(
                "{}: crash cut {cut_id} replay_serial_sha256 does not match replay-serial artifact",
                manifest_path.display()
            ));
        }
    }
    Ok(())
}

fn verify_required_crash_cut_artifacts(
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<()> {
    let campaign_plan_sha256 = artifacts
        .get("crash-campaign-plan")
        .ok_or_else(|| {
            format!(
                "{}: missing required artifact crash-campaign-plan",
                path.display()
            )
        })?
        .sha256
        .clone();
    for idx in 0..1000 {
        let cut_id = format!("crash-cut-{idx:04}");
        for suffix in [
            "job-request",
            "executor-plan",
            "result",
            "serial",
            "replay-serial",
            "e2fsck-log",
            "crash-image",
            "replay-image",
            "linux-rw-replay-image",
            "linux-rw-replay-log",
            "linux-post-replay-e2fsck-log",
            "tx-remount-image",
            "tx-remount-log",
            "semantic-oracle-request",
            "semantic-oracle-image",
            "semantic-oracle-log",
            "fault-executor-log",
        ] {
            let name = format!("{cut_id}-{suffix}");
            if !artifacts.contains_key(&name) {
                return Err(format!(
                    "{}: missing required crash-cut artifact {name}",
                    path.display()
                ));
            }
        }
        verify_crash_cut_job_result(&cut_id, artifacts, path, &campaign_plan_sha256)?;
    }
    Ok(())
}

fn verify_crash_cut_job_result(
    cut_id: &str,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
    expected_campaign_plan_sha256: &str,
) -> Result<()> {
    let job_request = crash_cut_artifact_path(cut_id, "job-request", artifacts, path)?;
    let executor_plan = crash_cut_artifact_path(cut_id, "executor-plan", artifacts, path)?;
    let fault_executor_log =
        crash_cut_artifact_path(cut_id, "fault-executor-log", artifacts, path)?;
    let result = crash_cut_artifact_path(cut_id, "result", artifacts, path)?;
    let request_json = read_json(&job_request)?;
    require_schema(&request_json, "tx.ext4.fault_job_request.v1", &job_request)?;
    let request = json_object(&request_json, "fault job request", &job_request)?;
    let campaign_plan_sha256 = required_json_string(request, "campaign_plan_sha256", &job_request)?;
    verify_real_sha(
        "fault job campaign_plan_sha256",
        &campaign_plan_sha256,
        &job_request,
    )?;
    if campaign_plan_sha256 != expected_campaign_plan_sha256 {
        return Err(format!(
            "{}: fault job campaign_plan_sha256 does not match crash-campaign-plan artifact",
            job_request.display()
        ));
    }
    let job = required_json_object(request, "job", &job_request)?;
    let case_id = required_json_string(job, "case", &job_request)?;
    let request_cut = required_json_string(job, "cut", &job_request)?;
    if request_cut != cut_id {
        return Err(format!(
            "{}: job request cut mismatch: expected {cut_id}, found {request_cut}",
            job_request.display()
        ));
    }
    let qemu = required_json_object(request, "qemu", &job_request)?;
    let phase_marker = required_json_string(qemu, "cut_marker", &job_request)?;
    verify_fault_executor_log(&fault_executor_log, &job_request, path)?;
    verify_crash_cut_executor_plan(
        cut_id,
        &executor_plan,
        &job_request,
        request,
        expected_campaign_plan_sha256,
        &result,
    )?;
    let crash_image = required_bound_path(
        job,
        "crash_image",
        &crash_cut_artifact_path(cut_id, "crash-image", artifacts, path)?,
        &job_request,
    )?;
    let replay_image = required_bound_path(
        job,
        "replay_image",
        &crash_cut_artifact_path(cut_id, "replay-image", artifacts, path)?,
        &job_request,
    )?;
    let serial_log = required_bound_path(
        job,
        "serial_log",
        &crash_cut_artifact_path(cut_id, "serial", artifacts, path)?,
        &job_request,
    )?;
    let e2fsck_log = required_e2fsck_log_path(
        job,
        &replay_image,
        &crash_cut_artifact_path(cut_id, "e2fsck-log", artifacts, path)?,
        &job_request,
    )?;
    verify_job_request_replay_matrix(job, cut_id, artifacts, path, &job_request)?;
    verify_job_request_semantic_oracle(job, cut_id, artifacts, path, &job_request)?;
    parse_fault_job_result(
        &result,
        &case_id,
        cut_id,
        &campaign_plan_sha256,
        &crash_image,
        &replay_image,
        &serial_log,
        &phase_marker,
        &e2fsck_log,
    )?;
    Ok(())
}

fn verify_fault_executor_log(
    log_path: &Path,
    job_request: &Path,
    receipt_path: &Path,
) -> Result<()> {
    let text = std::fs::read_to_string(log_path).map_err(|err| {
        format!(
            "{}: failed to read {}: {err}",
            receipt_path.display(),
            log_path.display()
        )
    })?;
    let root = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current repository directory: {err}"))?;
    let command_line = format!(
        "$ python3 {} {}",
        root.join("tools/ext4/fault_qemu_executor.py").display(),
        job_request.display()
    );
    if !text.lines().any(|line| line == command_line) {
        return Err(format!(
            "{}: fault executor log {} missing command line `{command_line}`",
            receipt_path.display(),
            log_path.display()
        ));
    }
    if !text.lines().any(|line| line == "status=exit status: 0") {
        return Err(format!(
            "{}: fault executor log {} missing successful exit status",
            receipt_path.display(),
            log_path.display()
        ));
    }
    Ok(())
}

fn verify_crash_cut_executor_plan(
    cut_id: &str,
    executor_plan: &Path,
    job_request: &Path,
    request: &serde_json::Map<String, serde_json::Value>,
    expected_campaign_plan_sha256: &str,
    result_path: &Path,
) -> Result<()> {
    let plan_json = read_json(executor_plan)?;
    require_schema(
        &plan_json,
        "tx.ext4.fault_qemu_executor_plan.v1",
        executor_plan,
    )?;
    let plan = json_object(&plan_json, "fault executor plan", executor_plan)?;
    require_field_value(
        plan,
        "request",
        &job_request.display().to_string(),
        executor_plan,
    )?;
    require_field_value(
        plan,
        "campaign_plan_sha256",
        expected_campaign_plan_sha256,
        executor_plan,
    )?;

    let request_job = required_json_object(request, "job", job_request)?;
    let plan_job = required_json_object(plan, "job", executor_plan)?;
    for key in ["case", "cut", "crash_image", "replay_image", "serial_log"] {
        require_matching_json_string(plan_job, request_job, key, executor_plan)?;
    }
    let plan_iteration = required_json_usize(plan_job, "iteration", executor_plan)?;
    let request_iteration = required_json_usize(request_job, "iteration", job_request)?;
    if plan_iteration != request_iteration {
        return Err(format!(
            "{}: executor plan iteration mismatch: expected {request_iteration}, found {plan_iteration}",
            executor_plan.display()
        ));
    }
    let plan_cut = required_json_string(plan_job, "cut", executor_plan)?;
    if plan_cut != cut_id {
        return Err(format!(
            "{}: executor plan cut mismatch: expected {cut_id}, found {plan_cut}",
            executor_plan.display()
        ));
    }
    for key in ["checks", "replay_matrix", "semantic_oracles"] {
        require_matching_json_value(plan_job, request_job, key, executor_plan)?;
    }

    let request_qemu = required_json_object(request, "qemu", job_request)?;
    verify_executor_plan_staged_roles_and_command(
        plan,
        request,
        request_job,
        request_qemu,
        executor_plan,
    )?;
    let runner = required_json_object(plan, "runner", executor_plan)?;
    require_matching_json_string(runner, request_job, "serial_log", executor_plan)?;
    require_field_value(
        runner,
        "command_source",
        "shell-test-command",
        executor_plan,
    )?;
    require_field_value(runner, "status", "exited-after-cut", executor_plan)?;
    let plan_cut_marker = required_json_string(runner, "cut_marker", executor_plan)?;
    let request_cut_marker = required_json_string(request_qemu, "cut_marker", job_request)?;
    if plan_cut_marker != request_cut_marker {
        return Err(format!(
            "{}: executor plan runner cut_marker mismatch: expected {request_cut_marker}, found {plan_cut_marker}",
            executor_plan.display()
        ));
    }

    let hard_kill = required_json_object(plan, "hard_kill", executor_plan)?;
    require_bool_value(hard_kill, "required", true, executor_plan)?;
    require_field_value(hard_kill, "status", "observed-by-shell-test", executor_plan)?;

    verify_executor_plan_preserved_images(plan, plan_job, executor_plan)?;
    verify_executor_plan_executed_checks(plan, result_path, executor_plan)?;
    verify_executor_plan_preflights(plan, executor_plan)?;

    let result = required_json_object(plan, "result", executor_plan)?;
    require_field_value(
        result,
        "path",
        &result_path.display().to_string(),
        executor_plan,
    )?;
    require_field_value(result, "status", "written", executor_plan)?;
    Ok(())
}

fn verify_executor_plan_staged_roles_and_command(
    plan: &serde_json::Map<String, serde_json::Value>,
    request: &serde_json::Map<String, serde_json::Value>,
    request_job: &serde_json::Map<String, serde_json::Value>,
    request_qemu: &serde_json::Map<String, serde_json::Value>,
    executor_plan: &Path,
) -> Result<()> {
    let staged = required_json_object(plan, "staged_role_images", executor_plan)?;
    let role_images = required_json_object(request, "role_images", executor_plan)?;
    if staged.len() != 3 {
        return Err(format!(
            "{}: executor plan staged_role_images has {}, expected 3",
            executor_plan.display(),
            staged.len()
        ));
    }

    let job_dir = executor_plan
        .parent()
        .ok_or_else(|| format!("{}: executor plan has no parent", executor_plan.display()))?;
    for role in ["test", "scratch", "workload"] {
        let source = PathBuf::from(required_json_string(role_images, role, executor_plan)?);
        let expected_staged = job_dir.join("roles").join(format!("{role}.img"));
        let staged_path = required_bound_path(staged, role, &expected_staged, executor_plan)?;
        if !source.is_file() {
            return Err(format!(
                "{}: role image source is missing for {role}: {}",
                executor_plan.display(),
                source.display()
            ));
        }
        if !staged_path.is_file() {
            return Err(format!(
                "{}: staged role image is missing for {role}: {}",
                executor_plan.display(),
                staged_path.display()
            ));
        }
        let source_sha = sha256_file(&source)?;
        let staged_sha = sha256_file(&staged_path)?;
        if source_sha != staged_sha {
            return Err(format!(
                "{}: staged role image digest mismatch for {role}",
                executor_plan.display()
            ));
        }
    }

    let serial_log = required_json_string(request_job, "serial_log", executor_plan)?;
    let cut_marker = required_json_string(request_qemu, "cut_marker", executor_plan)?;
    let timeout_ms = required_json_usize(request_qemu, "timeout_ms", executor_plan)?;
    let expected = vec![
        "cargo".to_string(),
        "xtask".to_string(),
        "shell-test".to_string(),
        "--target".to_string(),
        required_json_string(request_qemu, "target", executor_plan)?,
        "--profile".to_string(),
        required_json_string(request_qemu, "profile", executor_plan)?,
        "--script".to_string(),
        required_json_string(request_qemu, "script", executor_plan)?,
        "--serial-log".to_string(),
        serial_log,
        "--timeout-ms".to_string(),
        timeout_ms.to_string(),
        "--stop-after-needle".to_string(),
        cut_marker,
        "--extra-rv64-ext4".to_string(),
        required_json_string(staged, "test", executor_plan)?,
        "--extra-rv64-ext4".to_string(),
        required_json_string(staged, "scratch", executor_plan)?,
        "--extra-rv64-ext4".to_string(),
        required_json_string(staged, "workload", executor_plan)?,
    ];
    let actual = required_json_string_array(plan, "shell_test_command", executor_plan)?;
    if actual != expected {
        return Err(format!(
            "{}: executor plan shell_test_command mismatch",
            executor_plan.display()
        ));
    }
    Ok(())
}

fn verify_executor_plan_preserved_images(
    plan: &serde_json::Map<String, serde_json::Value>,
    plan_job: &serde_json::Map<String, serde_json::Value>,
    executor_plan: &Path,
) -> Result<()> {
    let staged = required_json_object(plan, "staged_role_images", executor_plan)?;
    let preserved = required_json_object(plan, "preserved_images", executor_plan)?;
    if preserved.len() != 4 {
        return Err(format!(
            "{}: executor plan preserved_images has {}, expected 4",
            executor_plan.display(),
            preserved.len()
        ));
    }
    require_field_value(
        preserved,
        "status",
        "copied-after-runner-termination",
        executor_plan,
    )?;
    required_bound_path(
        preserved,
        "crash",
        &PathBuf::from(required_json_string(
            plan_job,
            "crash_image",
            executor_plan,
        )?),
        executor_plan,
    )?;
    required_bound_path(
        preserved,
        "replay",
        &PathBuf::from(required_json_string(
            plan_job,
            "replay_image",
            executor_plan,
        )?),
        executor_plan,
    )?;
    required_bound_path(
        preserved,
        "source",
        &PathBuf::from(required_json_string(staged, "scratch", executor_plan)?),
        executor_plan,
    )?;
    Ok(())
}

fn verify_executor_plan_executed_checks(
    plan: &serde_json::Map<String, serde_json::Value>,
    result_path: &Path,
    executor_plan: &Path,
) -> Result<()> {
    let result_json = read_json(result_path)?;
    require_schema(&result_json, "tx.ext4.fault_job_result.v1", result_path)?;
    let result = json_object(&result_json, "fault job result", result_path)?;
    let actual = plan.get("executed_checks").ok_or_else(|| {
        format!(
            "{}: executor plan missing executed_checks",
            executor_plan.display()
        )
    })?;
    let expected = result.get("e2fsck_checks").ok_or_else(|| {
        format!(
            "{}: fault job result missing e2fsck_checks",
            result_path.display()
        )
    })?;
    if actual != expected {
        return Err(format!(
            "{}: executor plan executed_checks mismatch",
            executor_plan.display()
        ));
    }
    Ok(())
}

fn verify_executor_plan_preflights(
    plan: &serde_json::Map<String, serde_json::Value>,
    executor_plan: &Path,
) -> Result<()> {
    let replay = required_json_object(plan, "replay_matrix_preflight", executor_plan)?;
    require_field_value(replay, "status", "ready", executor_plan)?;
    let commands = require_non_empty_array(replay, "commands", executor_plan)?;
    let repo_root = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current repository directory: {err}"))?;
    let expected = [
        (
            "linux-rw-replay",
            Some(repo_root.join("tools/ext4/fault_linux_rw_replay.py")),
            "repository-default",
        ),
        ("linux-post-replay-e2fsck", None, "e2fsck"),
        (
            "tx-remount",
            Some(repo_root.join("tools/ext4/fault_tx_remount.py")),
            "repository-default",
        ),
    ];
    if commands.len() != expected.len() {
        return Err(format!(
            "{}: executor plan replay_matrix_preflight commands has {}, expected {}",
            executor_plan.display(),
            commands.len(),
            expected.len()
        ));
    }
    for (entry, (expected_id, expected_command, expected_source)) in commands.iter().zip(expected) {
        let entry = entry.as_object().ok_or_else(|| {
            format!(
                "{}: executor plan replay_matrix_preflight command must be an object",
                executor_plan.display()
            )
        })?;
        require_field_value(entry, "id", expected_id, executor_plan)?;
        require_field_value(entry, "command_source", expected_source, executor_plan)?;
        let command = required_json_string_array(entry, "command", executor_plan)?;
        if command.is_empty() {
            return Err(format!(
                "{}: executor plan replay_matrix_preflight command is empty",
                executor_plan.display()
            ));
        }
        if let Some(expected_command) = expected_command {
            if command != vec![expected_command.display().to_string()] {
                return Err(format!(
                    "{}: executor plan replay_matrix_preflight {expected_id} command mismatch",
                    executor_plan.display()
                ));
            }
        }
    }

    let semantic = required_json_object(plan, "semantic_oracle_preflight", executor_plan)?;
    require_field_value(semantic, "status", "ready", executor_plan)?;
    require_field_value(
        semantic,
        "command_source",
        "repository-default",
        executor_plan,
    )?;
    let expected_semantic = repo_root
        .join("tools/ext4/fault_semantic_oracle.py")
        .display()
        .to_string();
    let command = required_json_string_array(semantic, "command", executor_plan)?;
    if command != vec![expected_semantic] {
        return Err(format!(
            "{}: executor plan semantic_oracle_preflight command mismatch",
            executor_plan.display()
        ));
    }
    Ok(())
}

fn verify_job_request_replay_matrix(
    job: &serde_json::Map<String, serde_json::Value>,
    cut_id: &str,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
    job_request: &Path,
) -> Result<()> {
    let entries = require_non_empty_array(job, "replay_matrix", job_request)?;
    let expected = [
        (
            "linux-rw-replay",
            "linux-rw-replay-image",
            None,
            "linux-rw-replay-log",
        ),
        (
            "linux-post-replay-e2fsck",
            "linux-rw-replay-image",
            Some("linux-rw-replay-image"),
            "linux-post-replay-e2fsck-log",
        ),
        ("tx-remount", "tx-remount-image", None, "tx-remount-log"),
    ];
    if entries.len() != expected.len() {
        return Err(format!(
            "{}: job request replay_matrix has {}, expected {}",
            job_request.display(),
            entries.len(),
            expected.len()
        ));
    }
    for (entry, (expected_id, image_suffix, args_image_suffix, log_suffix)) in
        entries.iter().zip(expected)
    {
        let entry = entry.as_object().ok_or_else(|| {
            format!(
                "{}: job request replay_matrix entry must be an object",
                job_request.display()
            )
        })?;
        require_job_request_observation_entry(
            entry,
            "replay_matrix",
            expected_id,
            &crash_cut_artifact_path(cut_id, image_suffix, artifacts, path)?,
            &crash_cut_artifact_path(cut_id, log_suffix, artifacts, path)?,
            job_request,
        )?;
        if let Some(args_image_suffix) = args_image_suffix {
            let image = crash_cut_artifact_path(cut_id, args_image_suffix, artifacts, path)?;
            let args = entry
                .get("args")
                .and_then(|value| value.as_array())
                .ok_or_else(|| {
                    format!(
                        "{}: job request replay_matrix {expected_id} missing args",
                        job_request.display()
                    )
                })?
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        format!(
                            "{}: job request replay_matrix {expected_id} args must be strings",
                            job_request.display()
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if args != ["-fn", image.display().to_string().as_str()] {
                return Err(format!(
                    "{}: job request replay_matrix {expected_id} args mismatch",
                    job_request.display()
                ));
            }
        } else if entry.contains_key("args") {
            return Err(format!(
                "{}: job request replay_matrix {expected_id} must not declare args",
                job_request.display()
            ));
        }
    }
    Ok(())
}

fn verify_job_request_semantic_oracle(
    job: &serde_json::Map<String, serde_json::Value>,
    cut_id: &str,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
    job_request: &Path,
) -> Result<()> {
    let entries = require_non_empty_array(job, "semantic_oracles", job_request)?;
    if entries.len() != 1 {
        return Err(format!(
            "{}: job request semantic_oracles has {}, expected 1",
            job_request.display(),
            entries.len()
        ));
    }
    let expected_image = crash_cut_artifact_path(cut_id, "semantic-oracle-image", artifacts, path)?;
    let expected_log = crash_cut_artifact_path(cut_id, "semantic-oracle-log", artifacts, path)?;
    let entry = entries[0].as_object().ok_or_else(|| {
        format!(
            "{}: job request semantic_oracles entry must be an object",
            job_request.display()
        )
    })?;
    verify_job_request_semantic_oracle_entry(entry, &expected_image, &expected_log, job_request)?;

    let oracle_request =
        crash_cut_artifact_path(cut_id, "semantic-oracle-request", artifacts, path)?;
    let oracle_json = read_json(&oracle_request)?;
    require_schema(
        &oracle_json,
        "tx.ext4.fault_semantic_oracle_request.v1",
        &oracle_request,
    )?;
    let oracle = json_object(&oracle_json, "semantic oracle request", &oracle_request)?;
    verify_job_request_semantic_oracle_entry(
        oracle,
        &expected_image,
        &expected_log,
        &oracle_request,
    )
}

fn verify_job_request_semantic_oracle_entry(
    entry: &serde_json::Map<String, serde_json::Value>,
    expected_image: &Path,
    expected_log: &Path,
    path: &Path,
) -> Result<()> {
    require_job_request_observation_entry(
        entry,
        "semantic_oracles",
        "debugfs-file-hash-namespace",
        expected_image,
        expected_log,
        path,
    )?;
    let expected = entry.get("expected").ok_or_else(|| {
        format!(
            "{}: job request semantic_oracles debugfs-file-hash-namespace missing expected",
            path.display()
        )
    })?;
    if expected
        != &serde_json::json!({
            "present": {
                "/": {}
            }
        })
    {
        return Err(format!(
            "{}: job request semantic_oracles debugfs-file-hash-namespace expected mismatch",
            path.display()
        ));
    }
    Ok(())
}

fn require_job_request_observation_entry(
    entry: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    expected_id: &str,
    expected_image: &Path,
    expected_log: &Path,
    path: &Path,
) -> Result<()> {
    let id = required_json_string(entry, "id", path)?;
    if id != expected_id {
        return Err(format!(
            "{}: job request {field} id mismatch: expected {expected_id}, found {id}",
            path.display()
        ));
    }
    required_bound_path(entry, "image", expected_image, path)?;
    required_bound_path(entry, "log", expected_log, path)?;
    Ok(())
}

fn require_matching_json_string(
    actual: &serde_json::Map<String, serde_json::Value>,
    expected: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<()> {
    let actual = required_json_string(actual, key, path)?;
    let expected = required_json_string(expected, key, path)?;
    if actual != expected {
        return Err(format!(
            "{}: executor plan {key} mismatch: expected {expected}, found {actual}",
            path.display()
        ));
    }
    Ok(())
}

fn require_matching_json_value(
    actual: &serde_json::Map<String, serde_json::Value>,
    expected: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<()> {
    let actual = actual
        .get(key)
        .ok_or_else(|| format!("{}: executor plan missing {key}", path.display()))?;
    let expected = expected
        .get(key)
        .ok_or_else(|| format!("{}: job request missing {key}", path.display()))?;
    if actual != expected {
        return Err(format!("{}: executor plan {key} mismatch", path.display()));
    }
    Ok(())
}

fn crash_cut_artifact_path(
    cut_id: &str,
    suffix: &str,
    artifacts: &BTreeMap<String, ArtifactRecord>,
    path: &Path,
) -> Result<PathBuf> {
    let name = format!("{cut_id}-{suffix}");
    artifacts
        .get(&name)
        .map(|artifact| artifact.path.clone())
        .ok_or_else(|| {
            format!(
                "{}: missing required crash-cut artifact {name}",
                path.display()
            )
        })
}

fn required_bound_path(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    expected: &Path,
    path: &Path,
) -> Result<PathBuf> {
    let actual = PathBuf::from(required_json_string(object, key, path)?);
    if actual != expected {
        return Err(format!(
            "{}: job request {key} mismatch: expected {}, found {}",
            path.display(),
            expected.display(),
            actual.display()
        ));
    }
    Ok(actual)
}

fn required_e2fsck_log_path(
    job: &serde_json::Map<String, serde_json::Value>,
    replay_image: &Path,
    expected_log: &Path,
    path: &Path,
) -> Result<PathBuf> {
    let checks = require_non_empty_array(job, "checks", path)?;
    if checks.len() != 1 {
        return Err(format!(
            "{}: job request e2fsck checks has {}, expected 1",
            path.display(),
            checks.len()
        ));
    }
    let check = checks[0].as_object().ok_or_else(|| {
        format!(
            "{}: job request e2fsck check must be an object",
            path.display()
        )
    })?;
    if check.get("tool").and_then(|value| value.as_str()) != Some("e2fsck") {
        return Err(format!(
            "{}: job request e2fsck check tool must be e2fsck",
            path.display()
        ));
    }
    let args = check
        .get("args")
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: job request e2fsck check missing args", path.display()))?
        .iter()
        .map(|value| {
            value.as_str().map(str::to_string).ok_or_else(|| {
                format!(
                    "{}: job request e2fsck args must be strings",
                    path.display()
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if args != ["-fn", replay_image.display().to_string().as_str()] {
        return Err(format!(
            "{}: job request e2fsck check must run -fn against {}",
            path.display(),
            replay_image.display()
        ));
    }
    required_bound_path(check, "log", expected_log, path)
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

fn verify_hex_string(label: &str, value: &str, len: usize, path: &Path) -> Result<()> {
    if value.len() != len || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!(
            "{}: {label} must be a {len}-byte hex string",
            path.display()
        ));
    }
    Ok(())
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

fn required_json_string_array(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<Vec<String>> {
    object
        .get(key)
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: missing array {key}", path.display()))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{}: array {key} entries must be strings", path.display()))
        })
        .collect()
}

fn require_non_empty_array<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
    path: &Path,
) -> Result<&'a Vec<serde_json::Value>> {
    let values = object
        .get(key)
        .and_then(|value| value.as_array())
        .ok_or_else(|| format!("{}: missing array {key}", path.display()))?;
    if values.is_empty() {
        return Err(format!("{}: array {key} is empty", path.display()));
    }
    Ok(values)
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

fn canonical_existing_path(path: &Path, label: &str, context: &Path) -> Result<PathBuf> {
    path.canonicalize().map_err(|err| {
        format!(
            "{}: failed to canonicalize {label} {}: {err}",
            context.display(),
            path.display()
        )
    })
}

#[derive(Debug)]
struct ArtifactRecord {
    path: PathBuf,
    sha256: String,
}
