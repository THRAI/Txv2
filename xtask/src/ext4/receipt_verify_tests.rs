use std::fs;
use std::path::PathBuf;

use super::super::{sha256_file, verify_tier1_receipt};
use super::{temp_root, write_json, write_text};

#[test]
fn tier1_verify_receipt_accepts_locked_product_evidence() {
    let root = temp_root("verify-receipt-ok");
    let receipt = write_acceptance_receipt_fixture(&root);

    verify_tier1_receipt(&receipt).expect("locked product receipt verifies");
}

#[test]
fn tier1_verify_receipt_rejects_tampered_artifact() {
    let root = temp_root("verify-receipt-tampered");
    let receipt = write_acceptance_receipt_fixture(&root);
    write_text(
        &root.join("target/ext4/tier1/accepted-run/crash-cut-0000-replay.img"),
        "tampered\n",
    );

    let error = verify_tier1_receipt(&receipt).expect_err("tampered artifact must fail");
    assert!(error.contains("artifact crash-cut-0000-replay-image sha256 mismatch"));
}

#[test]
fn tier1_verify_receipt_rejects_missing_per_cut_serial_artifact() {
    let root = temp_root("verify-receipt-missing-crash-serial");
    let receipt = write_acceptance_receipt_fixture(&root);
    fs::remove_file(root.join("target/ext4/tier1/accepted-run/crash-cut-0000-serial.txt"))
        .expect("remove serial artifact");

    let error = verify_tier1_receipt(&receipt).expect_err("missing serial artifact must fail");
    assert!(error.contains("artifact crash-cut-0000-serial is missing"));
}

fn write_acceptance_receipt_fixture(root: &PathBuf) -> PathBuf {
    let run_dir = root.join("target/ext4/tier1/accepted-run");
    fs::create_dir_all(&run_dir).expect("create run dir");
    let mut artifacts = Vec::new();
    let mut e2fsck_images = Vec::new();

    let mut add_artifact = |name: &str, file_name: String, contents: String| {
        let path = run_dir.join(file_name);
        write_text(&path, &contents);
        let sha256 = sha256_file(&path).expect("artifact sha");
        artifacts.push(serde_json::json!({
            "name": name,
            "path": path.display().to_string(),
            "sha256": sha256
        }));
        (path, sha256)
    };

    let (test_image, test_sha) =
        add_artifact("test-image", "test.img".into(), "test image\n".into());
    let (scratch_image, scratch_sha) = add_artifact(
        "scratch-image",
        "scratch.img".into(),
        "scratch image\n".into(),
    );
    let (workload_image, workload_sha) = add_artifact(
        "workload-image",
        "workload.img".into(),
        "workload image\n".into(),
    );
    for (role, sha256) in [
        ("test", test_sha.clone()),
        ("scratch", scratch_sha.clone()),
        ("workload", workload_sha.clone()),
    ] {
        e2fsck_images.push(serde_json::json!({
            "role": role,
            "image_sha256": sha256,
            "exit_code": 0
        }));
    }

    for (name, file_name, contents) in [
        ("crash-campaign-plan", "crash-campaign-plan.json", "{}\n"),
        ("crash-cut-outcomes", "crash-cut-outcomes.json", "{}\n"),
        ("e2fsck-test-log", "e2fsck-test.log", "test clean\n"),
        (
            "e2fsck-scratch-log",
            "e2fsck-scratch.log",
            "scratch clean\n",
        ),
        (
            "e2fsck-workload-log",
            "e2fsck-workload.log",
            "workload clean\n",
        ),
        ("xfstests-log", "xfstests.log", "Passed all 8 tests\n"),
    ] {
        add_artifact(name, file_name.into(), contents.into());
    }

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
            "linux-rw-replay-image",
            "linux-rw-replay-log",
            "linux-post-replay-e2fsck-log",
            "tx-remount-image",
            "tx-remount-log",
            "semantic-oracle-request",
            "semantic-oracle-image",
            "semantic-oracle-log",
        ] {
            add_artifact(
                &format!("{cut_id}-{suffix}"),
                format!("{cut_id}-{suffix}.txt"),
                format!("{cut_id} {suffix}\n"),
            );
        }
        let (_path, sha256) = add_artifact(
            &format!("{cut_id}-replay-image"),
            format!("{cut_id}-replay.img"),
            format!("{cut_id} replay image\n"),
        );
        e2fsck_images.push(serde_json::json!({
            "role": cut_id,
            "image_sha256": sha256,
            "exit_code": 0
        }));
    }

    let artifacts_path = run_dir.join("artifacts.json");
    write_json(
        &artifacts_path,
        &serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.tier1_artifacts.v1",
            "run_id": "accepted-run",
            "artifacts": artifacts
        }))
        .expect("artifact manifest json"),
    );
    let artifacts_sha = sha256_file(&artifacts_path).expect("artifact manifest sha");

    let receipt_path = run_dir.join("acceptance-receipt.json");
    write_json(
        &receipt_path,
        &serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.tier1_acceptance_receipt.v1",
            "candidate": {
                "commit": "0123456789abcdef0123456789abcdef01234567",
                "run_id": "accepted-run"
            },
            "authorities": {
                "capability_ledger_sha256": "1".repeat(64),
                "crash_cut_catalog_sha256": "2".repeat(64),
                "xfstests_selection_sha256": "3".repeat(64),
                "shell_scenario_sha256": "4".repeat(64)
            },
            "artifact_manifest": {
                "path": artifacts_path.display().to_string(),
                "sha256": artifacts_sha
            },
            "role_images": {
                "test": {
                    "path": test_image.display().to_string(),
                    "sha256": test_sha
                },
                "scratch": {
                    "path": scratch_image.display().to_string(),
                    "sha256": scratch_sha
                },
                "workload": {
                    "path": workload_image.display().to_string(),
                    "sha256": workload_sha
                }
            },
            "crash_cuts": {
                "completed": 1000,
                "required": 1000,
                "families": ["D0","D1","D2","D3","D4","D5","D6","D7","D8","D9","D10","D11","D12"]
            },
            "e2fsck": {
                "immutable_images": e2fsck_images,
                "failures": 0
            },
            "xfstests": {
                "skipped": 0,
                "not_run": 0,
                "passed": 8,
                "failed": 0
            },
            "gates": {
                "G0": "passed",
                "G1": "passed",
                "G2": "passed",
                "G3": "passed",
                "G4": "passed",
                "G5": "passed",
                "G6": "passed",
                "G7": "passed"
            },
            "planned_actions": ["test fixture"],
            "notes": ["test fixture"]
        }))
        .expect("receipt json"),
    );
    let receipt_sha = sha256_file(&receipt_path).expect("receipt sha");
    write_json(
        &run_dir.join("receipt-lock.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.tier1_receipt_lock.v1",
            "run_id": "accepted-run",
            "receipt": {
                "path": receipt_path.display().to_string(),
                "sha256": receipt_sha
            },
            "artifact_manifest": {
                "path": artifacts_path.display().to_string(),
                "sha256": artifacts_sha
            }
        }))
        .expect("receipt lock json"),
    );
    receipt_path
}
