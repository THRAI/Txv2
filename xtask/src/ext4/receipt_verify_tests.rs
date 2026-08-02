use std::fs;
use std::path::PathBuf;

use super::super::{sha256_file, verify_tier1_receipt};
use super::{temp_root, write_json, write_text};

#[test]
fn tier1_verify_receipt_accepts_locked_product_evidence() {
    let root = temp_root("verify-receipt-ok");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);

    verify_tier1_receipt(&receipt).expect("locked product receipt verifies");
}

#[test]
fn tier1_verify_receipt_rejects_outcome_sha_mismatch() {
    let root = temp_root("verify-receipt-outcome-sha");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture_with_bad_outcome(&root);

    let error = verify_tier1_receipt(&receipt).expect_err("bad outcome sha must fail");
    assert!(error.contains(
        "crash cut crash-cut-0000 replay_serial_sha256 does not match replay-serial artifact"
    ));
}

#[test]
fn tier1_verify_receipt_rejects_campaign_plan_sha_mismatch() {
    let root = temp_root("verify-receipt-campaign-sha");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture_with_bad_campaign_plan_sha(&root);

    let error = verify_tier1_receipt(&receipt).expect_err("bad campaign plan sha must fail");
    assert!(
        error
            .contains("fault job campaign_plan_sha256 does not match crash-campaign-plan artifact")
    );
}

#[test]
fn tier1_verify_receipt_rejects_authority_artifact_sha_mismatch() {
    let root = temp_root("verify-receipt-authority-sha");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture_with_bad_authority_sha(&root);

    let error = verify_tier1_receipt(&receipt).expect_err("bad authority sha must fail");
    assert!(error.contains(
        "authority capability_ledger_sha256 does not match artifact authority-capability-ledger"
    ));
}

#[test]
fn tier1_verify_receipt_rejects_missing_g0_log_artifact() {
    let root = temp_root("verify-receipt-missing-g0-log");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    remove_artifact_record(&root, "g0-ext4-lifecycle-ownership-log");

    let error = verify_tier1_receipt(&receipt).expect_err("missing G0 log must fail");
    assert!(error.contains("missing required artifact g0-ext4-lifecycle-ownership-log"));
}

#[test]
fn tier1_verify_receipt_rejects_g0_lint_log_without_zero_exit() {
    let root = temp_root("verify-receipt-g0-log-exit");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    rewrite_artifact_contents(
        &root,
        "g0-ext4-no-direct-home-write-log",
        "$ cargo xtask lint invariants ext4-no-direct-home-write\nexit_code=1\nfound direct write\n",
    );

    let error = verify_tier1_receipt(&receipt).expect_err("bad G0 exit must fail");
    assert!(error.contains("G0 lint ext4-no-direct-home-write log missing exit_code=0"));
}

#[test]
fn tier1_verify_receipt_rejects_g0_lint_log_for_wrong_rule() {
    let root = temp_root("verify-receipt-g0-log-wrong-rule");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    rewrite_artifact_contents(
        &root,
        "g0-ext4-durability-flags-log",
        "$ cargo xtask lint invariants ext4-no-direct-home-write\nexit_code=0\nok\n",
    );

    let error = verify_tier1_receipt(&receipt).expect_err("wrong G0 rule must fail");
    assert!(error.contains(
        "G0 lint log missing command `$ cargo xtask lint invariants ext4-durability-flags`"
    ));
}

#[test]
fn tier1_verify_receipt_rejects_missing_guest_matrix_serial_log() {
    let root = temp_root("verify-receipt-missing-guest-serial");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    remove_artifact_record(&root, "guest-matrix-serial-log");

    let error = verify_tier1_receipt(&receipt).expect_err("missing guest matrix log must fail");
    assert!(error.contains("missing required artifact guest-matrix-serial-log"));
}

#[test]
fn tier1_verify_receipt_rejects_missing_build_log_artifact() {
    let root = temp_root("verify-receipt-missing-build-log");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    remove_artifact_record(&root, "candidate-full-build-log");

    let error = verify_tier1_receipt(&receipt).expect_err("missing build log must fail");
    assert!(error.contains("missing required artifact candidate-full-build-log"));
}

#[test]
fn tier1_verify_receipt_rejects_dirty_role_e2fsck_log() {
    let root = temp_root("verify-receipt-dirty-role-e2fsck-log");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    rewrite_artifact_contents(
        &root,
        "e2fsck-scratch-log",
        "scratch.img: Inode bitmap differences: -58\nscratch.img: Free inodes count wrong\n",
    );

    let error = verify_tier1_receipt(&receipt).expect_err("dirty e2fsck log must fail");
    assert!(error.contains("e2fsck scratch log contains dirty marker"));
}

#[test]
fn tier1_verify_receipt_rejects_role_e2fsck_log_without_clean_summary() {
    let root = temp_root("verify-receipt-role-e2fsck-missing-clean");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    rewrite_artifact_contents(&root, "e2fsck-test-log", "Pass 1: Checking inodes\n");

    let error = verify_tier1_receipt(&receipt).expect_err("missing clean summary must fail");
    assert!(error.contains("e2fsck test log missing clean summary"));
}

#[test]
fn tier1_verify_receipt_rejects_missing_xfstests_source_lock_evidence() {
    let root = temp_root("verify-receipt-missing-xfstests-source-lock");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    remove_artifact_record(&root, "xfstests-source-lock-evidence");

    let error = verify_tier1_receipt(&receipt).expect_err("missing xfstests source lock must fail");
    assert!(error.contains("missing required artifact xfstests-source-lock-evidence"));
}

#[test]
fn tier1_verify_receipt_rejects_xfstests_source_lock_mismatch() {
    let root = temp_root("verify-receipt-xfstests-source-lock-mismatch");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    rewrite_artifact_contents(
        &root,
        "xfstests-source-lock-evidence",
        &serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.xfstests_source_lock_evidence.v1",
            "source_root": "external/xfstests",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "check_path": "external/xfstests/check",
            "check_sha256": "d".repeat(64)
        }))
        .expect("source lock evidence json"),
    );

    let error = verify_tier1_receipt(&receipt).expect_err("mismatched source lock must fail");
    assert!(error.contains("xfstests source-lock check_sha256 mismatch"));
}

#[test]
fn tier1_verify_receipt_rejects_xfstests_log_count_mismatch() {
    let root = temp_root("verify-receipt-xfstests-log-count");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    rewrite_artifact_contents(&root, "xfstests-log", "Passed all 7 tests\n");

    let error = verify_tier1_receipt(&receipt).expect_err("mismatched xfstests log must fail");
    assert!(error.contains("xfstests log passed count 7 does not match receipt passed count 8"));
}

#[test]
fn tier1_verify_receipt_rejects_xfstests_log_failures() {
    let root = temp_root("verify-receipt-xfstests-log-failures");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    rewrite_artifact_contents(&root, "xfstests-log", "Failures: generic/001\n");

    let error = verify_tier1_receipt(&receipt).expect_err("failed xfstests log must fail");
    assert!(error.contains("xfstests log reported failures"));
}

#[test]
fn tier1_verify_receipt_rejects_tampered_artifact() {
    let root = temp_root("verify-receipt-tampered");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    write_text(
        &root.join("target/ext4/tier1/accepted-run/crash-cuts/crash-cut-0000/replay.img"),
        "tampered\n",
    );

    let error = verify_tier1_receipt(&receipt).expect_err("tampered artifact must fail");
    assert!(error.contains("artifact crash-cut-0000-replay-image sha256 mismatch"));
}

#[test]
fn tier1_verify_receipt_rejects_missing_per_cut_serial_artifact() {
    let root = temp_root("verify-receipt-missing-crash-serial");
    let _cleanup = TempCleanup(root.clone());
    let receipt = write_acceptance_receipt_fixture(&root);
    fs::remove_file(
        root.join("target/ext4/tier1/accepted-run/crash-cuts/crash-cut-0000/serial.log"),
    )
    .expect("remove serial artifact");

    let error = verify_tier1_receipt(&receipt).expect_err("missing serial artifact must fail");
    assert!(error.contains("artifact crash-cut-0000-serial is missing"));
}

struct TempCleanup(PathBuf);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_acceptance_receipt_fixture(root: &PathBuf) -> PathBuf {
    write_acceptance_receipt_fixture_inner(root, FixtureOptions::default())
}

fn write_acceptance_receipt_fixture_with_bad_outcome(root: &PathBuf) -> PathBuf {
    write_acceptance_receipt_fixture_inner(
        root,
        FixtureOptions {
            bad_first_outcome: true,
            ..FixtureOptions::default()
        },
    )
}

fn write_acceptance_receipt_fixture_with_bad_campaign_plan_sha(root: &PathBuf) -> PathBuf {
    write_acceptance_receipt_fixture_inner(
        root,
        FixtureOptions {
            bad_first_campaign_plan_sha: true,
            ..FixtureOptions::default()
        },
    )
}

fn write_acceptance_receipt_fixture_with_bad_authority_sha(root: &PathBuf) -> PathBuf {
    write_acceptance_receipt_fixture_inner(
        root,
        FixtureOptions {
            bad_authority_sha: true,
            ..FixtureOptions::default()
        },
    )
}

#[derive(Clone, Copy, Default)]
struct FixtureOptions {
    bad_first_outcome: bool,
    bad_first_campaign_plan_sha: bool,
    bad_authority_sha: bool,
}

fn write_acceptance_receipt_fixture_inner(root: &PathBuf, options: FixtureOptions) -> PathBuf {
    let run_dir = root.join("target/ext4/tier1/accepted-run");
    fs::create_dir_all(&run_dir).expect("create run dir");
    let mut artifacts = Vec::new();
    let mut e2fsck_images = Vec::new();
    let mut crash_outcomes = Vec::new();

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

    let (_capability_ledger, capability_ledger_sha256) = add_artifact(
        "authority-capability-ledger",
        "authorities/capability-ledger.json".into(),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#.into(),
    );
    let (_crash_cut_catalog, crash_cut_catalog_sha256) = add_artifact(
        "authority-crash-cut-catalog",
        "authorities/crash-cuts.json".into(),
        r#"{"schema":"tx.ext4.crash_cut_catalog.v1"}"#.into(),
    );
    let (_xfstests_selection, xfstests_selection_sha256) = add_artifact(
        "authority-xfstests-selection",
        "authorities/xfstests-selection.json".into(),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.xfstests_selection_ledger.v1",
            "status": "acceptance-ready",
            "tier": "tier1",
            "source_lock": {
                "path": "external/xfstests",
                "revision": "0123456789abcdef0123456789abcdef01234567",
                "check_sha256": "c".repeat(64)
            },
            "selected": [
                {"case_id": "generic/001"},
                {"case_id": "generic/002"},
                {"case_id": "generic/003"},
                {"case_id": "generic/004"},
                {"case_id": "generic/005"},
                {"case_id": "generic/006"},
                {"case_id": "generic/007"},
                {"case_id": "generic/008"}
            ]
        }))
        .expect("xfstests authority json"),
    );
    let (_shell_scenario, shell_scenario_sha256) = add_artifact(
        "authority-shell-scenario",
        "authorities/ext4-tier1.scn".into(),
        "# ext4 tier1\n".into(),
    );
    for rule in [
        "ext4-lifecycle-ownership",
        "ext4-no-direct-home-write",
        "ext4-durability-flags",
    ] {
        add_artifact(
            &format!("g0-{rule}-log"),
            format!("g0/{rule}.log"),
            format!("$ cargo xtask lint invariants {rule}\nexit_code=0\nok\n"),
        );
    }
    add_artifact(
        "guest-matrix-serial-log",
        "guest-matrix-serial.log".into(),
        "shell-test: groups: 8 passed, 0 failed\nshell-test: ok\n".into(),
    );
    add_artifact(
        "candidate-full-build-log",
        "build/full-build.log".into(),
        "$ cargo xtask full-build --target rv64-qemu --skip-doctor\nexit_code=0\nok\n".into(),
    );
    add_artifact(
        "busybox-ext4-image-build-log",
        "build/busybox-ext4-image.log".into(),
        "$ cargo xtask image ext4 --profile busybox --target rv64-qemu\nexit_code=0\nok\n".into(),
    );
    add_artifact(
        "busybox-base-image",
        "base.img".into(),
        "busybox base image\n".into(),
    );

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

    let (_campaign_plan, campaign_plan_sha256) = add_artifact(
        "crash-campaign-plan",
        "crash-campaign-plan.json".into(),
        "{}\n".into(),
    );

    for (name, file_name, contents) in [
        (
            "e2fsck-test-log",
            "e2fsck-test.log",
            "test.img: clean, 12/1024 files, 256/4096 blocks\n",
        ),
        (
            "e2fsck-scratch-log",
            "e2fsck-scratch.log",
            "scratch.img: clean, 12/1024 files, 256/4096 blocks\n",
        ),
        (
            "e2fsck-workload-log",
            "e2fsck-workload.log",
            "workload.img: clean, 12/1024 files, 256/4096 blocks\n",
        ),
        ("xfstests-log", "xfstests.log", "Passed all 8 tests\n"),
    ] {
        add_artifact(name, file_name.into(), contents.into());
    }
    add_artifact(
        "xfstests-source-lock-evidence",
        "xfstests-source-lock-evidence.json".into(),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.xfstests_source_lock_evidence.v1",
            "source_root": "external/xfstests",
            "revision": "0123456789abcdef0123456789abcdef01234567",
            "check_path": "external/xfstests/check",
            "check_sha256": "c".repeat(64)
        }))
        .expect("xfstests source lock evidence json"),
    );

    for idx in 0..1000 {
        let cut_id = format!("crash-cut-{idx:04}");
        let cut_dir = format!("crash-cuts/{cut_id}");
        let per_cut_campaign_plan_sha256 = if options.bad_first_campaign_plan_sha && idx == 0 {
            "b".repeat(64)
        } else {
            campaign_plan_sha256.clone()
        };
        let (crash_image, _) = add_artifact(
            &format!("{cut_id}-crash-image"),
            format!("{cut_dir}/crash.img"),
            format!("{cut_id} crash image\n"),
        );
        let (replay_image, replay_sha256) = add_artifact(
            &format!("{cut_id}-replay-image"),
            format!("{cut_dir}/replay.img"),
            format!("{cut_id} replay image\n"),
        );
        let (linux_replay_image, linux_replay_sha256) = add_artifact(
            &format!("{cut_id}-linux-rw-replay-image"),
            format!("{cut_dir}/linux-rw-replay.img"),
            format!("{cut_id} linux replay image\n"),
        );
        let (tx_remount_image, tx_remount_sha256) = add_artifact(
            &format!("{cut_id}-tx-remount-image"),
            format!("{cut_dir}/tx-remount.img"),
            format!("{cut_id} tx remount image\n"),
        );
        let (semantic_oracle_image, semantic_oracle_sha256) = add_artifact(
            &format!("{cut_id}-semantic-oracle-image"),
            format!("{cut_dir}/semantic-oracle.img"),
            format!("{cut_id} semantic oracle image\n"),
        );
        let (serial_log, _) = add_artifact(
            &format!("{cut_id}-serial"),
            format!("{cut_dir}/serial.log"),
            format!("{cut_id}\ntx.ext4.crash.phase.D7\n"),
        );
        add_artifact(
            &format!("{cut_id}-replay-serial"),
            format!("{cut_dir}/replay-serial.log"),
            format!("{cut_id} replay serial\n"),
        );
        let replay_serial_sha256 =
            sha256_file(&run_dir.join(format!("{cut_dir}/replay-serial.log")))
                .expect("replay serial sha");
        let (e2fsck_log, e2fsck_log_sha256) = add_artifact(
            &format!("{cut_id}-e2fsck-log"),
            format!("{cut_dir}/e2fsck-fn.log"),
            format!("{cut_id} e2fsck clean\n"),
        );
        let (linux_log, linux_log_sha256) = add_artifact(
            &format!("{cut_id}-linux-rw-replay-log"),
            format!("{cut_dir}/linux-rw-replay.log"),
            format!("{cut_id} linux replay ok\n"),
        );
        let (linux_e2fsck_log, linux_e2fsck_log_sha256) = add_artifact(
            &format!("{cut_id}-linux-post-replay-e2fsck-log"),
            format!("{cut_dir}/linux-post-replay-e2fsck.log"),
            format!("{cut_id} linux e2fsck ok\n"),
        );
        let (tx_remount_log, tx_remount_log_sha256) = add_artifact(
            &format!("{cut_id}-tx-remount-log"),
            format!("{cut_dir}/tx-remount.log"),
            format!("{cut_id} tx remount ok\n"),
        );
        let (semantic_oracle_log, semantic_oracle_log_sha256) = add_artifact(
            &format!("{cut_id}-semantic-oracle-log"),
            format!("{cut_dir}/semantic-oracle.log"),
            format!("{cut_id} semantic oracle ok\n"),
        );
        let (_semantic_oracle_request, _) = add_artifact(
            &format!("{cut_id}-semantic-oracle-request"),
            format!("{cut_dir}/semantic-oracle-01-request.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": "tx.ext4.fault_semantic_oracle_request.v1",
                "id": "debugfs-file-hash-namespace",
                "image": semantic_oracle_image.display().to_string(),
                "log": semantic_oracle_log.display().to_string(),
                "expected": {"present": {"/": {}}}
            }))
            .expect("semantic oracle request json"),
        );
        add_artifact(
            &format!("{cut_id}-executor-plan"),
            format!("{cut_dir}/executor-plan.json"),
            format!(
                "{{\"schema\":\"tx.ext4.fault_qemu_executor_plan.v1\",\"cut\":\"{cut_id}\"}}\n"
            ),
        );
        add_artifact(
            &format!("{cut_id}-job-request"),
            format!("{cut_dir}/job-request.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": "tx.ext4.fault_job_request.v1",
                "campaign_plan_sha256": per_cut_campaign_plan_sha256.clone(),
                "job": {
                    "case": "D7",
                    "cut": &cut_id,
                    "iteration": 1,
                    "serial_log": serial_log.display().to_string(),
                    "crash_image": crash_image.display().to_string(),
                    "replay_image": replay_image.display().to_string(),
                    "checks": [{
                        "tool": "e2fsck",
                        "args": ["-fn", replay_image.display().to_string()],
                        "log": e2fsck_log.display().to_string()
                    }],
                    "replay_matrix": [
                        {
                            "id": "linux-rw-replay",
                            "image": linux_replay_image.display().to_string(),
                            "log": linux_log.display().to_string()
                        },
                        {
                            "id": "linux-post-replay-e2fsck",
                            "image": linux_replay_image.display().to_string(),
                            "args": ["-fn", linux_replay_image.display().to_string()],
                            "log": linux_e2fsck_log.display().to_string()
                        },
                        {
                            "id": "tx-remount",
                            "image": tx_remount_image.display().to_string(),
                            "log": tx_remount_log.display().to_string()
                        }
                    ],
                    "semantic_oracles": [{
                        "id": "debugfs-file-hash-namespace",
                        "image": semantic_oracle_image.display().to_string(),
                        "log": semantic_oracle_log.display().to_string(),
                        "expected": {"present": {"/": {}}}
                    }]
                },
                "role_images": {
                    "test": test_image.display().to_string(),
                    "scratch": scratch_image.display().to_string(),
                    "workload": workload_image.display().to_string()
                },
                "qemu": {
                    "target": "rv64-qemu",
                    "profile": "busybox",
                    "script": "tools/ext4/tier1/crash-workload.scn",
                    "timeout_ms": 120000,
                    "cut_marker": "tx.ext4.crash.phase.D7"
                }
            }))
            .expect("job request json"),
        );
        add_artifact(
            &format!("{cut_id}-result"),
            format!("{cut_dir}/result.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": "tx.ext4.fault_job_result.v1",
                "campaign_plan_sha256": per_cut_campaign_plan_sha256.clone(),
                "case": "D7",
                "cut": &cut_id,
                "iteration": 1,
                "hard_kill_observed": true,
                "replay_attempted": true,
                "e2fsck_exit": 0,
                "e2fsck_checks": [{
                    "tool": "e2fsck",
                    "args": ["-fn", replay_image.display().to_string()],
                    "log": e2fsck_log.display().to_string(),
                    "log_sha256": e2fsck_log_sha256,
                    "exit_code": 0
                }],
                "replay_matrix": [
                    {
                        "id": "linux-rw-replay",
                        "image": linux_replay_image.display().to_string(),
                        "log": linux_log.display().to_string(),
                        "log_sha256": linux_log_sha256,
                        "image_sha256": linux_replay_sha256,
                        "exit_code": 0
                    },
                    {
                        "id": "linux-post-replay-e2fsck",
                        "image": linux_replay_image.display().to_string(),
                        "args": ["-fn", linux_replay_image.display().to_string()],
                        "log": linux_e2fsck_log.display().to_string(),
                        "log_sha256": linux_e2fsck_log_sha256,
                        "image_sha256": linux_replay_sha256,
                        "exit_code": 0
                    },
                    {
                        "id": "tx-remount",
                        "image": tx_remount_image.display().to_string(),
                        "log": tx_remount_log.display().to_string(),
                        "log_sha256": tx_remount_log_sha256,
                        "image_sha256": tx_remount_sha256,
                        "exit_code": 0
                    }
                ],
                "semantic_oracles": [{
                    "id": "debugfs-file-hash-namespace",
                    "image": semantic_oracle_image.display().to_string(),
                    "log": semantic_oracle_log.display().to_string(),
                    "expected": {"present": {"/": {}}},
                    "log_sha256": semantic_oracle_log_sha256,
                    "image_sha256": semantic_oracle_sha256,
                    "exit_code": 0
                }]
            }))
            .expect("fault job result json"),
        );
        e2fsck_images.push(serde_json::json!({
            "role": &cut_id,
            "image_sha256": replay_sha256.clone(),
            "exit_code": 0
        }));
        crash_outcomes.push(serde_json::json!({
            "cut_id": &cut_id,
            "immutable_image_sha256": replay_sha256,
            "replay_serial_sha256": if options.bad_first_outcome && idx == 0 {
                "1".repeat(64)
            } else {
                replay_serial_sha256
            },
            "e2fsck_exit_code": 0,
            "replay_exit_code": 0
        }));
    }

    add_artifact(
        "crash-cut-outcomes",
        "crash-cut-outcomes.json".into(),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.crash_cut_outcome_manifest.v1",
            "completed": 1000,
            "required": 1000,
            "families": ["D7"],
            "outcomes": crash_outcomes
        }))
        .expect("crash outcome manifest json"),
    );

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
                "capability_ledger_sha256": if options.bad_authority_sha {
                    "5".repeat(64)
                } else {
                    capability_ledger_sha256
                },
                "crash_cut_catalog_sha256": crash_cut_catalog_sha256,
                "xfstests_selection_sha256": xfstests_selection_sha256,
                "shell_scenario_sha256": shell_scenario_sha256
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

fn remove_artifact_record(root: &PathBuf, name: &str) {
    let artifacts_path = root.join("target/ext4/tier1/accepted-run/artifacts.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifacts_path).expect("read artifacts"))
            .expect("parse artifacts");
    manifest["artifacts"]
        .as_array_mut()
        .expect("artifacts array")
        .retain(|artifact| artifact["name"] != name);
    write_json(
        &artifacts_path,
        &serde_json::to_string_pretty(&manifest).expect("artifact manifest json"),
    );
    rewrite_receipt_artifact_manifest_sha(root);
}

fn rewrite_artifact_contents(root: &PathBuf, name: &str, contents: &str) {
    let artifacts_path = root.join("target/ext4/tier1/accepted-run/artifacts.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&artifacts_path).expect("read artifacts"))
            .expect("parse artifacts");
    let artifact = manifest["artifacts"]
        .as_array_mut()
        .expect("artifacts array")
        .iter_mut()
        .find(|artifact| artifact["name"] == name)
        .unwrap_or_else(|| panic!("missing artifact {name}"));
    let path = PathBuf::from(artifact["path"].as_str().expect("artifact path"));
    write_text(&path, contents);
    artifact["sha256"] = serde_json::Value::String(sha256_file(&path).expect("artifact sha"));
    write_json(
        &artifacts_path,
        &serde_json::to_string_pretty(&manifest).expect("artifact manifest json"),
    );
    rewrite_receipt_artifact_manifest_sha(root);
}

fn rewrite_receipt_artifact_manifest_sha(root: &PathBuf) {
    let run_dir = root.join("target/ext4/tier1/accepted-run");
    let artifacts_path = run_dir.join("artifacts.json");
    let artifacts_sha = sha256_file(&artifacts_path).expect("artifact manifest sha");
    let receipt_path = run_dir.join("acceptance-receipt.json");
    let mut receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt_path).expect("read receipt"))
            .expect("parse receipt");
    receipt["artifact_manifest"]["sha256"] = serde_json::Value::String(artifacts_sha.clone());
    write_json(
        &receipt_path,
        &serde_json::to_string_pretty(&receipt).expect("receipt json"),
    );
    let receipt_sha = sha256_file(&receipt_path).expect("receipt sha");
    let mut lock: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(run_dir.join("receipt-lock.json")).expect("read lock"),
    )
    .expect("parse lock");
    lock["receipt"]["sha256"] = serde_json::Value::String(receipt_sha);
    lock["artifact_manifest"]["sha256"] = serde_json::Value::String(artifacts_sha);
    write_json(
        &run_dir.join("receipt-lock.json"),
        &serde_json::to_string_pretty(&lock).expect("lock json"),
    );
}
