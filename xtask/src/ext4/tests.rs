use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::crash_campaign::{
    campaign_start_index, restore_completed_crash_cut_state,
    restore_completed_crash_cut_state_from, run_replay_probe_with_retries_for_test,
    write_crash_cut_outcome_manifest,
};
use super::live_preflight::collect_storage_capacity_preflight_for_test;
use super::{
    CrashCutCampaignEvidence, CrashCutCampaignPlan, CrashCutCatalog, CrashCutFamily,
    CrashCutOutcome, Tier1Authorities, XfstestsSourceLock, crash_cut_shell_test_args,
    parse_fault_job_result, parse_tier1_args, parse_xfstests_summary, run_and_record_command,
    run_crash_cut_campaign, run_live_preflight, run_workspace::RunWorkspace, sha256_file,
    stage_authority_artifacts, tier1_shell_test_args, verify_xfstests_source_lock,
    write_fault_job_request, xfstests_selected_cases_sha256,
};
use crate::target::TxTarget;

#[path = "receipt_verify_tests.rs"]
mod receipt_verify_tests;

#[test]
fn run_workspace_finalizes_once_and_cleans_temporary_state() {
    let root = temp_root("finalize");
    let mut run = RunWorkspace::create(&root, "test-run").unwrap();
    let artifact = run.temporary_path_for_test().join("scratch.img");
    write_text(&artifact, "scratch-image\n");
    let artifact_sha = sha256_file(&artifact).unwrap();
    run.record_artifact("scratch", artifact).unwrap();
    let receipt = run.finalize().unwrap();
    assert!(receipt.exists());
    assert!(!run.temporary_path_for_test().exists());
    let run_dir = root.join("target/ext4/tier1/test-run");
    let manifest_path = run_dir.join("artifacts.json");
    let lock_path = run_dir.join("receipt-lock.json");
    assert!(run_dir.exists());
    assert!(manifest_path.exists());
    assert!(lock_path.exists());
    let artifacts: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    assert_eq!(artifacts["artifacts"][0]["name"], "scratch");
    assert_eq!(artifacts["artifacts"][0]["sha256"], artifact_sha);
    let receipt_value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&receipt).unwrap()).unwrap();
    assert_eq!(
        receipt_value["artifact_manifest"]["path"],
        manifest_path.display().to_string()
    );
    assert_eq!(
        receipt_value["artifact_manifest"]["sha256"],
        sha256_file(&manifest_path).unwrap()
    );
    let lock: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&lock_path).unwrap()).unwrap();
    assert_eq!(lock["schema"], "tx.ext4.tier1_receipt_lock.v1");
    assert_eq!(
        lock["receipt"]["path"],
        run_dir
            .join("acceptance-receipt.json")
            .display()
            .to_string()
    );
    assert_eq!(lock["receipt"]["sha256"], sha256_file(&receipt).unwrap());
    assert_eq!(
        lock["artifact_manifest"]["sha256"],
        sha256_file(&manifest_path).unwrap()
    );
}

#[test]
fn run_workspace_failure_kills_children_writes_receipt_and_cleans_on_drop() {
    let root = temp_root("failure");
    let temp = root.join("target/ext4/tier1/.failed-run.tmp");
    {
        let mut run = RunWorkspace::create(&root, "failed-run").unwrap();
        write_text(
            &run.temporary_path_for_test()
                .join("crash-campaign-plan.json"),
            "{}\n",
        );
        run.record_artifact(
            "crash-campaign-plan",
            run.temporary_path_for_test()
                .join("crash-campaign-plan.json"),
        )
        .unwrap();
        let child = run.spawn_test_child("exit 17").unwrap();
        run.record_child(child);
        run.mark_failed_for_test("child-exit");
    }
    assert!(
        root.join("target/ext4/tier1/failed-run/failed-receipt.json")
            .exists()
    );
    assert!(
        root.join("target/ext4/tier1/failed-run/crash-campaign-plan.json")
            .exists()
    );
    assert!(
        root.join("target/ext4/tier1/failed-run/receipt-lock.json")
            .exists()
    );
    let artifacts: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("target/ext4/tier1/failed-run/artifacts.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(artifacts["schema"], "tx.ext4.tier1_artifacts.v1");
    assert_eq!(artifacts["artifacts"][0]["name"], "crash-campaign-plan");
    assert_eq!(
        artifacts["artifacts"][0]["sha256"],
        sha256_file(&root.join("target/ext4/tier1/failed-run/crash-campaign-plan.json")).unwrap()
    );
    let receipt: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("target/ext4/tier1/failed-run/failed-receipt.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        receipt["artifact_manifest"]["sha256"],
        sha256_file(&root.join("target/ext4/tier1/failed-run/artifacts.json")).unwrap()
    );
    let lock: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root.join("target/ext4/tier1/failed-run/receipt-lock.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        lock["receipt"]["path"],
        root.join("target/ext4/tier1/failed-run/failed-receipt.json")
            .display()
            .to_string()
    );
    assert_eq!(
        lock["receipt"]["sha256"],
        sha256_file(&root.join("target/ext4/tier1/failed-run/failed-receipt.json")).unwrap()
    );
    assert_eq!(
        lock["artifact_manifest"]["sha256"],
        sha256_file(&root.join("target/ext4/tier1/failed-run/artifacts.json")).unwrap()
    );
    assert!(!temp.exists());
}

#[test]
fn run_workspace_resume_preserves_existing_temporary_state() {
    let root = temp_root("resume");
    let temp_dir = root.join("target/ext4/tier1/.resume-run.tmp");
    fs::create_dir_all(&temp_dir).unwrap();
    let marker = temp_dir.join("marker.txt");
    write_text(&marker, "keep-me\n");

    let run = RunWorkspace::resume(&root, "resume-run").unwrap();

    assert_eq!(run.temporary_path_for_test(), temp_dir);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "keep-me\n");
}

#[test]
fn run_workspace_resume_reopens_failed_final_state() {
    let root = temp_root("resume-failed-final");
    let final_dir = root.join("target/ext4/tier1/resume-run");
    fs::create_dir_all(&final_dir).unwrap();
    write_text(&final_dir.join("failed-receipt.json"), "{}\n");
    write_text(&final_dir.join("artifacts.json"), "{}\n");
    write_text(&final_dir.join("receipt-lock.json"), "{}\n");
    let marker = final_dir.join("crash-cuts/crash-cut-0772/result.json");
    write_text(&marker, "{\"schema\":\"tx.ext4.fault_job_result.v1\"}\n");

    let run = RunWorkspace::resume(&root, "resume-run").unwrap();
    let temp_dir = root.join("target/ext4/tier1/.resume-run.tmp");

    assert_eq!(run.temporary_path_for_test(), temp_dir);
    assert!(!final_dir.exists());
    assert!(
        temp_dir
            .join("crash-cuts/crash-cut-0772/result.json")
            .exists()
    );
    assert!(!temp_dir.join("failed-receipt.json").exists());
    assert!(!temp_dir.join("artifacts.json").exists());
    assert!(!temp_dir.join("receipt-lock.json").exists());
}

#[test]
fn run_workspace_resume_reuses_failed_artifact_manifest_hash_cache() {
    let root = temp_root("resume-artifact-hash-cache");
    let final_dir = root.join("target/ext4/tier1/resume-run");
    let artifact = final_dir.join("crash-cuts/crash-cut-0999/replay.img");
    let cached_sha256 = "a".repeat(64);
    write_text(&artifact, "existing replay image\n");
    write_json(
        &final_dir.join("artifacts.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.tier1_artifacts.v1",
            "run_id": "resume-run",
            "artifacts": [
                {
                    "name": "crash-cut-0999-replay-image",
                    "path": artifact.display().to_string(),
                    "sha256": cached_sha256
                }
            ]
        }))
        .unwrap(),
    );
    write_text(&final_dir.join("failed-receipt.json"), "{}\n");
    write_text(&final_dir.join("receipt-lock.json"), "{}\n");

    {
        let mut run = RunWorkspace::resume(&root, "resume-run").unwrap();
        let temp_artifact = run
            .temporary_path_for_test()
            .join("crash-cuts/crash-cut-0999/replay.img");
        assert_eq!(
            run.cached_artifact_sha256(&temp_artifact),
            Some(cached_sha256.as_str())
        );
        run.record_artifact("crash-cut-0999-replay-image", temp_artifact)
            .unwrap();
        run.mark_failed_for_test("cache-rewrite");
    }

    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(final_dir.join("artifacts.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["artifacts"][0]["sha256"], cached_sha256);
}

#[test]
fn run_workspace_resume_rejects_accepted_final_state() {
    let root = temp_root("resume-accepted-final");
    let final_dir = root.join("target/ext4/tier1/resume-run");
    fs::create_dir_all(&final_dir).unwrap();
    write_text(&final_dir.join("acceptance-receipt.json"), "{}\n");

    let error = RunWorkspace::resume(&root, "resume-run")
        .expect_err("accepted receipts must not be reopened");

    assert!(error.contains("already finalized with acceptance receipt"));
    assert!(final_dir.exists());
    assert!(!root.join("target/ext4/tier1/.resume-run.tmp").exists());
}

#[test]
fn tier1_workspace_stages_authority_artifacts() {
    let root = temp_root("authority-artifacts");
    write_tier1_authority_fixture(&root);
    let authorities = Tier1Authorities::load(&root).unwrap();
    let mut run = RunWorkspace::create(&root, "authority-run").unwrap();

    stage_authority_artifacts(&mut run, &authorities).expect("stage authority artifacts");
    let receipt = run.finalize().expect("finalize run");
    assert!(receipt.exists());

    let run_dir = root.join("target/ext4/tier1/authority-run");
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("artifacts.json")).unwrap()).unwrap();
    let artifacts = manifest["artifacts"].as_array().unwrap();
    for (name, relative_path, expected_sha256) in [
        (
            "authority-capability-ledger",
            "authorities/capability-ledger.json",
            authorities.capability.sha256.clone(),
        ),
        (
            "authority-crash-cut-catalog",
            "authorities/crash-cuts.json",
            authorities.crash_cuts.sha256().to_string(),
        ),
        (
            "authority-xfstests-selection",
            "authorities/xfstests-selection.json",
            authorities.selection.sha256().to_string(),
        ),
        (
            "authority-shell-scenario",
            "authorities/ext4-tier1.scn",
            authorities.shell_scenario.sha256.clone(),
        ),
    ] {
        let artifact = artifacts
            .iter()
            .find(|artifact| artifact["name"] == name)
            .unwrap_or_else(|| panic!("missing authority artifact {name}"));
        let path = run_dir.join(relative_path);
        assert!(path.exists(), "missing staged authority {}", path.display());
        assert_eq!(artifact["path"], path.display().to_string());
        assert_eq!(artifact["sha256"], expected_sha256);
        assert_eq!(sha256_file(&path).unwrap(), expected_sha256);
    }
}

#[test]
fn tier1_workspace_records_command_log_artifact() {
    let root = temp_root("command-log-artifact");
    let mut run = RunWorkspace::create(&root, "command-log-run").unwrap();
    run_and_record_command(
        &root,
        &mut run,
        "candidate-full-build-log",
        "build/full-build.log",
        "sh",
        &["-c".into(), "printf build-ok".into()],
        "test command",
    )
    .expect("record command log");
    let receipt = run.finalize().expect("finalize run");
    assert!(receipt.exists());

    let run_dir = root.join("target/ext4/tier1/command-log-run");
    let log_path = run_dir.join("build/full-build.log");
    let log = fs::read_to_string(&log_path).expect("read command log");
    assert!(log.contains("exit_code=0"));
    assert!(log.contains("build-ok"));
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(run_dir.join("artifacts.json")).unwrap()).unwrap();
    let artifact = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["name"] == "candidate-full-build-log")
        .expect("command log artifact");
    assert_eq!(artifact["path"], log_path.display().to_string());
    assert_eq!(artifact["sha256"], sha256_file(&log_path).unwrap());
}

#[test]
fn tier1_rejects_missing_or_stale_authority_inputs() {
    let root = temp_root("missing-cut");
    write_json(
        &root.join("tools/ext4/tier1/capability-ledger.json"),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        r#"{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "tier":"tier1",
          "source_lock":{
            "path":"external/xfstests",
            "revision":"acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509",
            "check_sha256":"104d9351e1b2d47f7992af650e0fed0054be0cecfd8fddd43e076e175ba80642"
          },
          "selected":["generic/001"]
        }"#,
    );
    write_json(&root.join("tools/ext4/tier1/crash-cuts.json"), r#"{}"#);
    write_text(
        &root.join("tools/shell-tests/ext4-tier1.scn"),
        "# ext4 tier1\n",
    );
    let error = parse_tier1_args(&root, &["tier1".into(), "--dry-run".into()])
        .expect_err("authority is mandatory");
    assert!(error.contains("tx.ext4.crash_cut_catalog.v1"));
}

#[test]
fn tier1_live_rejects_placeholder_authorities_before_acceptance_receipt() {
    let root = temp_root("placeholder-authorities");
    write_json(
        &root.join("tools/ext4/tier1/capability-ledger.json"),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        r#"{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"selection-authority-declared",
          "tier":"tier1",
          "source_lock":{
            "path":"external/xfstests",
            "revision":"acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509",
            "check_sha256":"104d9351e1b2d47f7992af650e0fed0054be0cecfd8fddd43e076e175ba80642"
          },
          "selected":[{"case_id":"generic/001"}]
        }"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/crash-cuts.json"),
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"catalog-authority-declared",
          "expanded_cut_count":1000,
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    write_text(
        &root.join("tools/shell-tests/ext4-tier1.scn"),
        "# ext4 tier1\n",
    );

    parse_tier1_args(&root, &["tier1".into(), "--dry-run".into()])
        .expect("dry-run still resolves placeholder authority hashes");
    let error = Tier1Authorities::load(&root)
        .unwrap()
        .ensure_live_acceptance_ready()
        .expect_err("live runner must not promote placeholder authorities");
    assert!(error.contains("xfstests selection status is `selection-authority-declared`"));
    assert!(error.contains("crash-cut catalog status is `catalog-authority-declared`"));
}

#[test]
fn tier1_parse_accepts_live_preflight_mode() {
    let root = temp_root("preflight-live-parse");
    write_tier1_authority_fixture(&root);

    let invocation = parse_tier1_args(&root, &["tier1".into(), "--preflight-live".into()])
        .expect("parse preflight-live");

    assert!(invocation.preflight_live);
    assert!(!invocation.dry_run);
    assert!(!invocation.materialize_xfstests);
}

#[test]
fn tier1_parse_accepts_resume_for_live_runs() {
    let root = temp_root("resume-parse");
    write_tier1_authority_fixture(&root);

    let invocation =
        parse_tier1_args(&root, &["tier1".into(), "--resume".into()]).expect("parse resume");

    assert!(invocation.resume);
    assert!(!invocation.dry_run);
    assert!(!invocation.preflight_live);
}

#[test]
fn tier1_parse_accepts_live_start_cut() {
    let root = temp_root("start-cut-parse");
    write_tier1_authority_fixture(&root);

    let invocation = parse_tier1_args(
        &root,
        &[
            "tier1".into(),
            "--run-id".into(),
            "diagnose-cut".into(),
            "--start-cut".into(),
            "crash-cut-0725".into(),
        ],
    )
    .expect("parse start cut");

    assert_eq!(invocation.crash_cut_start, Some(725));
    assert!(
        invocation
            .planned_actions()
            .contains(&"execute deterministic crash cuts from crash-cut-0725".into())
    );
}

#[test]
fn tier1_parse_rejects_start_cut_outside_campaign() {
    let root = temp_root("start-cut-outside-campaign");
    write_tier1_authority_fixture(&root);

    let error = parse_tier1_args(
        &root,
        &[
            "tier1".into(),
            "--start-cut".into(),
            "crash-cut-1000".into(),
        ],
    )
    .expect_err("start cut must be within expanded campaign");

    assert!(error.contains("outside expanded crash cut count 1000"));
}

#[test]
fn tier1_parse_rejects_start_cut_for_non_live_modes() {
    let root = temp_root("start-cut-live-only");
    write_tier1_authority_fixture(&root);

    let dry_run_error = parse_tier1_args(
        &root,
        &[
            "tier1".into(),
            "--dry-run".into(),
            "--start-cut".into(),
            "725".into(),
        ],
    )
    .expect_err("start cut is live-only");
    assert!(dry_run_error.contains("--start-cut is only valid for live runs"));

    let preflight_error = parse_tier1_args(
        &root,
        &[
            "tier1".into(),
            "--preflight-live".into(),
            "--start-cut".into(),
            "725".into(),
        ],
    )
    .expect_err("start cut is live-only");
    assert!(preflight_error.contains("--start-cut is only valid for live runs"));
}

#[test]
fn tier1_crash_cut_start_uses_later_of_resume_prefix_and_requested_cut() {
    assert_eq!(campaign_start_index(0, None), 0);
    assert_eq!(campaign_start_index(0, Some(725)), 725);
    assert_eq!(campaign_start_index(724, Some(725)), 725);
    assert_eq!(campaign_start_index(800, Some(725)), 800);
}

#[test]
fn tier1_crash_cut_resume_restores_completed_start_cut_slice() {
    let root = temp_root("crash-cut-start-slice-resume");
    let mut run = RunWorkspace::create(&root, "crash-run").unwrap();
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_text(
        &root.join("tools/ext4/tier1/crash-workload.scn"),
        "# workload\n",
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-replay.scn"),
        "# replay\n",
    );
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/crash-workload.scn",
            "replay_script":"tools/ext4/tier1/crash-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    let catalog = CrashCutCatalog::load_with_root(catalog_path, &root).unwrap();
    let manifest = run.working_dir().join("crash-campaign-plan.json");
    write_text(&manifest, "campaign-plan\n");
    let campaign_plan_sha256 = sha256_file(&manifest).unwrap();
    write_complete_restored_cut(
        &run,
        "crash-cut-0725",
        "D10",
        "tx.ext4.crash.phase.D10",
        &campaign_plan_sha256,
    );

    let (next_idx, families, outcomes) =
        restore_completed_crash_cut_state_from(&mut run, &catalog, &campaign_plan_sha256, 725)
            .unwrap();

    assert_eq!(campaign_start_index(next_idx, Some(725)), 726);
    assert_eq!(families, vec!["D10"]);
    assert_eq!(outcomes[0].cut_id, "crash-cut-0725");
}

#[test]
fn tier1_parse_rejects_preflight_report_without_live_preflight() {
    let root = temp_root("preflight-report-without-live");
    write_tier1_authority_fixture(&root);

    let error = parse_tier1_args(
        &root,
        &[
            "tier1".into(),
            "--preflight-report".into(),
            "target/ext4/tier1/preflight/report.json".into(),
        ],
    )
    .expect_err("preflight report only applies to live preflight");

    assert!(error.contains("--preflight-report requires --preflight-live"));
}

#[test]
fn tier1_parse_rejects_materialize_xfstests_without_live_preflight() {
    let root = temp_root("materialize-without-live");
    write_tier1_authority_fixture(&root);

    let error = parse_tier1_args(&root, &["tier1".into(), "--materialize-xfstests".into()])
        .expect_err("materialize only applies to live preflight");

    assert!(error.contains("--materialize-xfstests requires --preflight-live"));
}

#[test]
fn tier1_parse_rejects_dry_run_with_live_preflight() {
    let root = temp_root("preflight-live-exclusive");
    write_tier1_authority_fixture(&root);

    let error = parse_tier1_args(
        &root,
        &[
            "tier1".into(),
            "--dry-run".into(),
            "--preflight-live".into(),
        ],
    )
    .expect_err("dry-run and live preflight are mutually exclusive");

    assert!(error.contains("only one of --dry-run or --preflight-live"));
}

#[test]
fn tier1_live_preflight_reports_later_blockers_with_placeholder_authorities() {
    let root = temp_root("preflight-live-placeholder");
    write_tier1_authority_fixture(&root);
    let authorities = Tier1Authorities::load(&root).expect("load authority fixture");

    let error = run_live_preflight(&root, "preflight-placeholder", None, false, &authorities)
        .expect_err("placeholder preflight must fail");

    assert!(error.contains("xfstests selection status is `selection-authority-declared`"));
    assert!(error.contains("crash-cut catalog status is `catalog-authority-declared`"));
    assert!(error.contains("pinned xfstests source is missing"));
}

#[test]
fn tier1_live_preflight_writes_durable_blocker_report() {
    let root = temp_root("preflight-live-report");
    write_tier1_authority_fixture(&root);
    let authorities = Tier1Authorities::load(&root).expect("load authority fixture");
    let report = root.join("target/ext4/tier1/preflight/report.json");

    let error = run_live_preflight(&root, "report-run", Some(&report), false, &authorities)
        .expect_err("placeholder preflight must fail");

    assert!(error.contains("pinned xfstests source is missing"));
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(value["schema"], "tx.ext4.tier1_live_preflight.v1");
    assert_eq!(value["run_id"], "report-run");
    assert_eq!(value["result"]["ready"], false);
    assert_eq!(value["result"]["acceptance_receipt_generated"], false);
    assert_eq!(
        value["authorities"]["xfstests_selection"]["status"],
        "selection-authority-declared"
    );
    assert!(
        value["result"]["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|blocker| blocker
                .as_str()
                .unwrap()
                .contains("pinned xfstests source is missing"))
    );
}

#[test]
fn tier1_live_preflight_materialize_verifies_existing_pinned_source() {
    let root = temp_root("preflight-materialize-existing");
    write_tier1_authority_fixture(&root);
    let xfstests = root.join("external/xfstests");
    fs::create_dir_all(&xfstests).unwrap();
    write_text(&xfstests.join("check"), "#!/bin/sh\nexit 0\n");
    write_text(&xfstests.join("tests/generic/001"), "#!/bin/sh\nexit 0\n");
    run_git(&xfstests, &["init"]);
    run_git(&xfstests, &["add", "check", "tests/generic/001"]);
    run_git(
        &xfstests,
        &[
            "commit",
            "-m",
            "pin check",
            "--author",
            "Tx Test <tx@example.invalid>",
        ],
    );
    let revision = git_output(&xfstests, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let check_sha256 = sha256_file(&xfstests.join("check")).unwrap();
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        &format!(
            r#"{{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"selection-authority-declared",
          "tier":"tier1",
          "source_lock":{{
            "path":"external/xfstests",
            "revision":"{revision}",
            "check_sha256":"{check_sha256}"
          }},
          "selected":[{{"case_id":"generic/001"}}]
        }}"#
        ),
    );
    let authorities = Tier1Authorities::load(&root).expect("load authority fixture");
    let report = root.join("target/ext4/tier1/preflight/materialized.json");

    let error = run_live_preflight(&root, "materialized-run", Some(&report), true, &authorities)
        .expect_err("placeholder authorities still block");

    assert!(!error.contains("pinned xfstests source is missing"));
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(value["preflight"]["materialize_xfstests_requested"], true);
    assert_eq!(value["preflight"]["xfstests_source_prepared"], true);
    assert_eq!(value["preflight"]["xfstests_selected_cases_verified"], true);
}

#[test]
fn tier1_live_preflight_reports_existing_pinned_source_without_materialize() {
    let root = temp_root("preflight-existing-source-report");
    write_tier1_authority_fixture(&root);
    let xfstests = root.join("external/xfstests");
    fs::create_dir_all(&xfstests).unwrap();
    write_text(&xfstests.join("check"), "#!/bin/sh\nexit 0\n");
    write_text(&xfstests.join("tests/generic/001"), "#!/bin/sh\nexit 0\n");
    run_git(&xfstests, &["init"]);
    run_git(&xfstests, &["add", "check", "tests/generic/001"]);
    run_git(
        &xfstests,
        &[
            "commit",
            "-m",
            "pin check",
            "--author",
            "Tx Test <tx@example.invalid>",
        ],
    );
    let revision = git_output(&xfstests, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let check_sha256 = sha256_file(&xfstests.join("check")).unwrap();
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        &format!(
            r#"{{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"selection-authority-declared",
          "tier":"tier1",
          "source_lock":{{
            "path":"external/xfstests",
            "revision":"{revision}",
            "check_sha256":"{check_sha256}"
          }},
          "selected":[{{"case_id":"generic/001"}}]
        }}"#
        ),
    );
    let authorities = Tier1Authorities::load(&root).expect("load authority fixture");
    let report = root.join("target/ext4/tier1/preflight/existing-source.json");

    let error = run_live_preflight(
        &root,
        "existing-source-run",
        Some(&report),
        false,
        &authorities,
    )
    .expect_err("placeholder authorities still block");

    assert!(!error.contains("pinned xfstests source is missing"));
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(value["preflight"]["materialize_xfstests_requested"], false);
    assert_eq!(value["preflight"]["xfstests_source_prepared"], true);
    assert_eq!(value["preflight"]["xfstests_selected_cases_verified"], true);
}

#[test]
fn tier1_live_preflight_rejects_missing_selected_xfstests_case() {
    let root = temp_root("preflight-missing-selected-case");
    write_tier1_authority_fixture(&root);
    let xfstests = root.join("external/xfstests");
    fs::create_dir_all(&xfstests).unwrap();
    write_text(&xfstests.join("check"), "#!/bin/sh\nexit 0\n");
    run_git(&xfstests, &["init"]);
    run_git(&xfstests, &["add", "check"]);
    run_git(
        &xfstests,
        &[
            "commit",
            "-m",
            "pin check",
            "--author",
            "Tx Test <tx@example.invalid>",
        ],
    );
    let revision = git_output(&xfstests, &["rev-parse", "HEAD"])
        .trim()
        .to_string();
    let check_sha256 = sha256_file(&xfstests.join("check")).unwrap();
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        &format!(
            r#"{{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"selection-authority-declared",
          "tier":"tier1",
          "source_lock":{{
            "path":"external/xfstests",
            "revision":"{revision}",
            "check_sha256":"{check_sha256}"
          }},
          "selected":[{{"case_id":"generic/001"}}]
        }}"#
        ),
    );
    let authorities = Tier1Authorities::load(&root).expect("load authority fixture");

    let error = run_live_preflight(&root, "missing-case-run", None, false, &authorities)
        .expect_err("missing selected case must block preflight");

    assert!(error.contains("selected xfstests case generic/001 is missing"));
}

#[test]
fn tier1_live_storage_preflight_blocks_when_capacity_is_below_campaign_floor() {
    let mut blockers = Vec::new();

    let estimate = collect_storage_capacity_preflight_for_test(
        13 * 1024 * 1024 * 1024,
        1000,
        64 * 1024 * 1024,
        false,
        &mut blockers,
    );

    assert_eq!(estimate.available_bytes, 13 * 1024 * 1024 * 1024);
    assert!(estimate.required_bytes > 100 * 1024 * 1024 * 1024);
    assert_eq!(blockers.len(), 1);
    assert!(blockers[0].contains("insufficient free space for live Tier 1 crash campaign"));
    assert!(blockers[0].contains("requires at least"));
}

#[test]
fn tier1_live_storage_preflight_accepts_cow_clone_capacity() {
    let mut blockers = Vec::new();

    let estimate = collect_storage_capacity_preflight_for_test(
        13 * 1024 * 1024 * 1024,
        1000,
        64 * 1024 * 1024,
        true,
        &mut blockers,
    );

    assert_eq!(estimate.cow_clone_supported, true);
    assert!(estimate.required_bytes < 13 * 1024 * 1024 * 1024);
    assert_eq!(blockers, Vec::<String>::new());
}

#[test]
fn tier1_live_rejects_acceptance_ready_crash_catalog_without_campaign_plan() {
    let root = temp_root("missing-crash-campaign-plan");
    write_json(
        &root.join("tools/ext4/tier1/capability-ledger.json"),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        r#"{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"acceptance-ready",
          "tier":"tier1",
          "source_lock":{
            "path":"external/xfstests",
            "revision":"acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509",
            "check_sha256":"104d9351e1b2d47f7992af650e0fed0054be0cecfd8fddd43e076e175ba80642"
          },
          "selected":[{"case_id":"generic/001"}]
        }"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/crash-cuts.json"),
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    write_text(
        &root.join("tools/shell-tests/ext4-tier1.scn"),
        "# ext4 tier1\n",
    );

    let error = Tier1Authorities::load(&root)
        .unwrap()
        .ensure_live_acceptance_ready()
        .expect_err("acceptance-ready crash catalog must be executable");
    assert!(error.contains("crash-cut catalog is acceptance-ready but missing campaign plan"));
}

#[test]
fn tier1_live_rejects_acceptance_ready_xfstests_without_readiness_evidence() {
    let root = temp_root("missing-xfstests-readiness");
    write_json(
        &root.join("tools/ext4/tier1/capability-ledger.json"),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        r#"{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"acceptance-ready",
          "tier":"tier1",
          "source_lock":{
            "path":"external/xfstests",
            "revision":"acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509",
            "check_sha256":"104d9351e1b2d47f7992af650e0fed0054be0cecfd8fddd43e076e175ba80642"
          },
          "selected":[{"case_id":"generic/001"}]
        }"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/crash-cuts.json"),
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"catalog-authority-declared",
          "expanded_cut_count":1000,
          "families":[
            {"id":"D0"},{"id":"D1"},{"id":"D2"},{"id":"D3"},{"id":"D4"},
            {"id":"D5"},{"id":"D6"},{"id":"D7"},{"id":"D8"},{"id":"D9"},
            {"id":"D10"},{"id":"D11"},{"id":"D12"}
          ]
        }"#,
    );
    write_text(
        &root.join("tools/shell-tests/ext4-tier1.scn"),
        "# ext4 tier1\n",
    );

    let error = Tier1Authorities::load(&root)
        .unwrap()
        .ensure_live_acceptance_ready()
        .expect_err("status-only xfstests readiness must fail");
    assert!(
        error.contains("xfstests selection is acceptance-ready but missing readiness_evidence")
    );
}

#[test]
fn tier1_live_rejects_acceptance_ready_crash_catalog_without_readiness_evidence() {
    let root = temp_root("missing-crash-readiness");
    let cases = vec!["generic/001".to_string()];
    write_json(
        &root.join("tools/ext4/tier1/capability-ledger.json"),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        &format!(
            r#"{{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"acceptance-ready",
          "tier":"tier1",
          "source_lock":{{
            "path":"external/xfstests",
            "revision":"acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509",
            "check_sha256":"104d9351e1b2d47f7992af650e0fed0054be0cecfd8fddd43e076e175ba80642"
          }},
          "selected":[{{"case_id":"generic/001"}}],
          "readiness_evidence":{{
            "source_revision":"acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509",
            "check_sha256":"104d9351e1b2d47f7992af650e0fed0054be0cecfd8fddd43e076e175ba80642",
            "selected_count":1,
            "selected_cases_sha256":"{}",
            "selection_policy":"tier1-controlled-production-v1"
          }}
        }}"#,
            xfstests_selected_cases_sha256(&cases)
        ),
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-workload.scn"),
        "# workload\n",
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-replay.scn"),
        "# replay\n",
    );
    write_json(
        &root.join("tools/ext4/tier1/crash-cuts.json"),
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/crash-workload.scn",
            "replay_script":"tools/ext4/tier1/crash-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    write_text(
        &root.join("tools/shell-tests/ext4-tier1.scn"),
        "# ext4 tier1\n",
    );

    let error = Tier1Authorities::load(&root)
        .unwrap()
        .ensure_live_acceptance_ready()
        .expect_err("status-only crash readiness must fail");
    assert!(error.contains("crash-cut catalog is acceptance-ready but missing readiness_evidence"));
}

#[test]
fn tier1_rejects_crash_campaign_with_missing_script_artifact() {
    let root = temp_root("missing-crash-script");
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/missing-workload.scn",
            "replay_script":"tools/ext4/tier1/missing-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );

    let error = CrashCutCatalog::load_with_root(catalog_path, &root)
        .expect_err("campaign scripts must exist before acceptance-ready use");
    assert!(error.contains("missing campaign.workload_script artifact"));
}

#[test]
fn tier1_rejects_crash_campaign_without_family_phase_markers() {
    let root = temp_root("missing-crash-phase-marker");
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_text(
        &root.join("tools/ext4/tier1/crash-workload.scn"),
        "# workload\n",
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-replay.scn"),
        "# replay\n",
    );
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/crash-workload.scn",
            "replay_script":"tools/ext4/tier1/crash-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0"},{"id":"D1"},{"id":"D2"},{"id":"D3"},{"id":"D4"},
            {"id":"D5"},{"id":"D6"},{"id":"D7"},{"id":"D8"},{"id":"D9"},
            {"id":"D10"},{"id":"D11"},{"id":"D12"}
          ]
        }"#,
    );

    let error = CrashCutCatalog::load_with_root(catalog_path, &root)
        .expect_err("phase-marker kill policy needs per-family markers");
    assert!(error.contains("missing phase_marker"));
}

#[test]
fn tier1_shell_matrix_passes_role_images_in_device_order() {
    let root = temp_root("scratch-matrix");
    let scenario = root.join("tools/shell-tests/ext4-tier1.scn");
    let test = root.join("target/ext4/tier1/run/test.img");
    let scratch = root.join("target/ext4/tier1/run/scratch.img");
    let workload = root.join("target/ext4/tier1/run/workload.img");
    let serial_log = root.join("target/ext4/tier1/run/guest-matrix-serial.log");
    let args = tier1_shell_test_args(
        TxTarget::Rv64Qemu,
        &scenario,
        &test,
        &scratch,
        &workload,
        &serial_log,
    );
    let rendered = args.join(" ");

    assert!(rendered.contains("--target rv64-qemu"));
    assert!(rendered.contains("--profile busybox"));
    assert!(rendered.contains("--serial-log"));
    assert!(rendered.contains(&serial_log.display().to_string()));
    assert!(rendered.contains("--extra-rv64-ext4"));
    assert!(rendered.contains(&test.display().to_string()));
    assert!(rendered.contains(&scratch.display().to_string()));
    assert!(rendered.contains(&workload.display().to_string()));
    let role_paths: Vec<String> = args
        .iter()
        .enumerate()
        .filter_map(|(idx, arg)| {
            if arg == "--extra-rv64-ext4" {
                args.get(idx + 1).cloned()
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        role_paths,
        vec![
            test.display().to_string(),
            scratch.display().to_string(),
            workload.display().to_string()
        ]
    );
}

#[test]
fn tier1_crash_cut_shell_matrix_uses_boot_and_cut_images_with_phase_marker() {
    let boot = PathBuf::from("/tmp/tx/boot.img");
    let cut = PathBuf::from("/tmp/tx/crash-cut-0007.img");
    let script = PathBuf::from("/tmp/tx/tools/ext4/tier1/crash-workload.scn");
    let serial = PathBuf::from("/tmp/tx/crash-cut-0007-replay.serial.log");
    let args = crash_cut_shell_test_args(
        &boot,
        &cut,
        &script,
        Some("tx.ext4.crash.phase.D7"),
        Some(&serial),
    );

    let mut extra_images = Vec::new();
    let mut stop_marker = None;
    let mut serial_log = None;
    for (idx, arg) in args.iter().enumerate() {
        if arg == "--extra-rv64-ext4" {
            extra_images.push(args[idx + 1].clone());
        }
        if arg == "--stop-after-needle" {
            stop_marker = args.get(idx + 1).cloned();
        }
        if arg == "--serial-log" {
            serial_log = args.get(idx + 1).cloned();
        }
    }

    assert_eq!(
        extra_images,
        vec![boot.display().to_string(), cut.display().to_string()]
    );
    assert_eq!(stop_marker.as_deref(), Some("tx.ext4.crash.phase.D7"));
    assert_eq!(serial_log.as_deref(), Some(serial.to_str().unwrap()));
}

#[test]
fn tier1_replay_probe_retries_from_retained_crash_image_after_timeout() {
    let root = temp_root("replay-probe-retry");
    let boot_image = root.join("boot.img");
    let crash_image = root.join("crash.img");
    let replay_image = root.join("replay.img");
    let replay_script = root.join("tools/ext4/tier1/crash-replay.scn");
    let replay_serial_log = root.join("replay-serial.log");
    write_text(&boot_image, "boot-image\n");
    write_text(&crash_image, "clean-crash-image\n");
    write_text(&replay_image, "mutated-after-failed-probe\n");
    write_text(&replay_script, "# replay\n");
    let attempts: RefCell<Vec<Vec<String>>> = RefCell::new(Vec::new());

    let result = run_replay_probe_with_retries_for_test(
        &root,
        &boot_image,
        &replay_image,
        &crash_image,
        &replay_script,
        &replay_serial_log,
        |_, args| {
            let attempt = attempts.borrow().len() + 1;
            let serial = shell_arg_value(&args, "--serial-log");
            write_text(
                &PathBuf::from(&serial),
                &format!("replay attempt {attempt}\n"),
            );
            attempts.borrow_mut().push(args);
            if attempt == 1 {
                Err("timed out after 10000 ms waiting for \"crash-replay-inspect:0\"".into())
            } else {
                Ok(())
            }
        },
    )
    .expect("second replay attempt should pass");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.attempts, 2);
    assert_eq!(attempts.borrow().len(), 2);
    assert_eq!(
        fs::read_to_string(&replay_image).unwrap(),
        "clean-crash-image\n"
    );
    assert!(root.join("replay-serial-attempt-1.log").is_file());
    assert_eq!(
        fs::read_to_string(&replay_serial_log).unwrap(),
        "replay attempt 2\n"
    );
    assert_eq!(
        shell_arg_value(&attempts.borrow()[1], "--serial-log"),
        replay_serial_log.display().to_string()
    );
}

#[test]
fn tier1_fault_job_request_binds_repository_executor_inputs() {
    let mut fixture = fault_request_fixture("fault-job-request");

    let job = write_fault_job_request(
        &fixture.root,
        &mut fixture.run,
        &fixture.campaign_manifest,
        "crash-cut-0007",
        &fixture.family,
        fixture.family.phase_marker.as_deref().unwrap(),
        &fixture.campaign,
        &fixture.test_image,
        &fixture.scratch_image,
        &fixture.workload_image,
    )
    .expect("write request");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&job.request_path).unwrap()).unwrap();

    assert_eq!(value["schema"], "tx.ext4.fault_job_request.v1");
    assert_eq!(
        value["campaign_plan_sha256"],
        sha256_file(&fixture.campaign_manifest).unwrap()
    );
    assert_eq!(value["job"]["case"], "D7");
    assert_eq!(value["job"]["cut"], "crash-cut-0007");
    assert_eq!(value["qemu"]["cut_marker"], "tx.ext4.crash.phase.D7");
    assert_eq!(
        value["qemu"]["script"],
        fixture
            .root
            .join("tools/ext4/tier1/crash-workload.scn")
            .display()
            .to_string()
    );
    assert_eq!(
        value["role_images"]["test"],
        fixture.test_image.display().to_string()
    );
    assert_eq!(
        value["role_images"]["scratch"],
        fixture.scratch_image.display().to_string()
    );
    assert_eq!(
        value["role_images"]["workload"],
        fixture.workload_image.display().to_string()
    );
    assert_eq!(
        value["job"]["checks"][0]["args"][1],
        job.replay_image.display().to_string()
    );
    assert_eq!(value["job"]["replay_matrix"][0]["id"], "linux-rw-replay");
    assert_eq!(
        value["job"]["replay_matrix"][0]["image"],
        job.linux_replay_image.display().to_string()
    );
    assert_eq!(
        value["job"]["replay_matrix"][1]["args"][1],
        job.linux_replay_image.display().to_string()
    );
    assert_eq!(value["job"]["replay_matrix"][2]["id"], "tx-remount");
    assert_eq!(
        value["job"]["replay_matrix"][2]["image"],
        job.tx_remount_image.display().to_string()
    );
    assert_eq!(
        value["job"]["semantic_oracles"][0]["id"],
        "debugfs-file-hash-namespace"
    );
    assert_eq!(
        value["job"]["semantic_oracles"][0]["image"],
        job.semantic_oracle_image.display().to_string()
    );
    assert_eq!(
        value["job"]["semantic_oracles"][0]["expected"]["present"]["/"],
        serde_json::json!({})
    );
    assert!(
        fixture
            .run
            .working_dir()
            .join("crash-cuts/crash-cut-0007/job-request.json")
            .is_file()
    );
}

#[test]
fn tier1_fault_job_request_does_not_register_future_artifacts_before_executor() {
    let root;
    {
        let mut fixture = fault_request_fixture("fault-job-request-fail-before-executor");
        root = fixture.root.clone();

        write_fault_job_request(
            &fixture.root,
            &mut fixture.run,
            &fixture.campaign_manifest,
            "crash-cut-0007",
            &fixture.family,
            fixture.family.phase_marker.as_deref().unwrap(),
            &fixture.campaign,
            &fixture.test_image,
            &fixture.scratch_image,
            &fixture.workload_image,
        )
        .expect("write request");
        fixture.run.mark_failed_for_test("before-executor");
    }

    let manifest_path = root.join("target/ext4/tier1/crash-run/artifacts.json");
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let names = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| artifact["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["crash-cut-0007-job-request"]);
    for artifact in manifest["artifacts"].as_array().unwrap() {
        assert!(PathBuf::from(artifact["path"].as_str().unwrap()).is_file());
    }
}

#[test]
fn tier1_fault_job_result_requires_hard_kill_and_e2fsck_exit() {
    let root = temp_root("fault-job-result");
    let result = root.join("result.json");
    let crash = root.join("crash.img");
    let replay = root.join("replay.img");
    let linux_replay = root.join("linux-rw-replay.img");
    let tx_remount = root.join("tx-remount.img");
    let semantic_oracle = root.join("semantic-oracle.img");
    let serial = root.join("serial.log");
    let log = root.join("e2fsck-fn.log");
    let linux_log = root.join("linux-rw-replay.log");
    let linux_e2fsck_log = root.join("linux-post-replay-e2fsck.log");
    let tx_remount_log = root.join("tx-remount.log");
    let semantic_log = root.join("semantic-oracle.log");
    write_text(&crash, "crash-image\n");
    write_text(&replay, "replay-image\n");
    write_text(&linux_replay, "linux-replay-image\n");
    write_text(&tx_remount, "tx-remount-image\n");
    write_text(&semantic_oracle, "semantic-oracle-image\n");
    write_text(&serial, "boot...\ntx.ext4.crash.phase.D7\n");
    write_text(&log, "clean\n");
    write_text(&linux_log, "linux ok\n");
    write_text(&linux_e2fsck_log, "linux e2fsck ok\n");
    write_text(&tx_remount_log, "tx remount ok\n");
    write_text(&semantic_log, "semantic ok\n");
    let log_sha256 = sha256_file(&log).unwrap();
    let linux_log_sha256 = sha256_file(&linux_log).unwrap();
    let linux_e2fsck_log_sha256 = sha256_file(&linux_e2fsck_log).unwrap();
    let tx_remount_log_sha256 = sha256_file(&tx_remount_log).unwrap();
    let semantic_log_sha256 = sha256_file(&semantic_log).unwrap();
    let linux_replay_sha256 = sha256_file(&linux_replay).unwrap();
    let tx_remount_sha256 = sha256_file(&tx_remount).unwrap();
    let semantic_sha256 = sha256_file(&semantic_oracle).unwrap();
    write_json(
        &result,
        &format!(
            r#"{{
          "schema":"tx.ext4.fault_job_result.v1",
          "campaign_plan_sha256":"{plan_sha}",
          "case":"D7",
          "cut":"crash-cut-0007",
          "hard_kill_observed":true,
          "replay_attempted":true,
          "e2fsck_exit":0,
          "e2fsck_checks":[
            {{
              "tool":"e2fsck",
              "args":["-fn","{replay}"],
              "log":"{log}",
              "log_sha256":"{log_sha256}",
              "exit_code":0
            }}
          ],
          "replay_matrix":[
            {{
              "id":"linux-rw-replay",
              "image":"{linux_replay}",
              "log":"{linux_log}",
              "log_sha256":"{linux_log_sha256}",
              "image_sha256":"{linux_replay_sha256}",
              "exit_code":0
            }},
            {{
              "id":"linux-post-replay-e2fsck",
              "image":"{linux_replay}",
              "args":["-fn","{linux_replay}"],
              "log":"{linux_e2fsck_log}",
              "log_sha256":"{linux_e2fsck_log_sha256}",
              "image_sha256":"{linux_replay_sha256}",
              "exit_code":0
            }},
            {{
              "id":"tx-remount",
              "image":"{tx_remount}",
              "log":"{tx_remount_log}",
              "log_sha256":"{tx_remount_log_sha256}",
              "image_sha256":"{tx_remount_sha256}",
              "exit_code":0
            }}
          ],
          "semantic_oracles":[
            {{
              "id":"debugfs-file-hash-namespace",
              "image":"{semantic_oracle}",
              "log":"{semantic_log}",
              "expected":{{"present":{{"/":{{}}}}}},
              "log_sha256":"{semantic_log_sha256}",
              "image_sha256":"{semantic_sha256}",
              "exit_code":0
            }}
          ]
        }}"#,
            plan_sha = "a".repeat(64),
            replay = replay.display(),
            log = log.display(),
            linux_replay = linux_replay.display(),
            linux_log = linux_log.display(),
            linux_e2fsck_log = linux_e2fsck_log.display(),
            tx_remount = tx_remount.display(),
            tx_remount_log = tx_remount_log.display(),
            semantic_oracle = semantic_oracle.display(),
            semantic_log = semantic_log.display(),
        ),
    );
    let parsed = parse_fault_job_result(
        &result,
        "D7",
        "crash-cut-0007",
        &"a".repeat(64),
        &crash,
        &replay,
        &serial,
        "tx.ext4.crash.phase.D7",
        &log,
    )
    .expect("parse clean result");
    assert_eq!(parsed.e2fsck_exit_code, 0);

    write_json(
        &result,
        &format!(
            r#"{{
          "schema":"tx.ext4.fault_job_result.v1",
          "campaign_plan_sha256":"{plan_sha}",
          "case":"D7",
          "cut":"crash-cut-0007",
          "hard_kill_observed":false,
          "replay_attempted":true,
          "e2fsck_exit":0,
          "e2fsck_checks":[
            {{
              "tool":"e2fsck",
              "args":["-fn","{replay}"],
              "log":"{log}",
              "log_sha256":"{log_sha256}",
              "exit_code":0
            }}
          ]
        }}"#,
            plan_sha = "a".repeat(64),
            replay = replay.display(),
            log = log.display(),
        ),
    );
    let error = parse_fault_job_result(
        &result,
        "D7",
        "crash-cut-0007",
        &"a".repeat(64),
        &crash,
        &replay,
        &serial,
        "tx.ext4.crash.phase.D7",
        &log,
    )
    .expect_err("hard kill is required");
    assert!(error.contains("hard_kill_observed is not true"));
}

#[test]
fn tier1_fault_job_result_requires_replay_matrix_and_semantic_oracle() {
    let root = temp_root("fault-job-result-missing-matrix");
    let result = root.join("result.json");
    let crash = root.join("crash.img");
    let replay = root.join("replay.img");
    let serial = root.join("serial.log");
    let log = root.join("e2fsck-fn.log");
    write_text(&crash, "crash-image\n");
    write_text(&replay, "replay-image\n");
    write_text(&serial, "boot...\ntx.ext4.crash.phase.D7\n");
    write_text(&log, "clean\n");
    let log_sha256 = sha256_file(&log).unwrap();
    write_json(
        &result,
        &format!(
            r#"{{
          "schema":"tx.ext4.fault_job_result.v1",
          "campaign_plan_sha256":"{plan_sha}",
          "case":"D7",
          "cut":"crash-cut-0007",
          "hard_kill_observed":true,
          "replay_attempted":true,
          "e2fsck_exit":0,
          "e2fsck_checks":[
            {{
              "tool":"e2fsck",
              "args":["-fn","{replay}"],
              "log":"{log}",
              "log_sha256":"{log_sha256}",
              "exit_code":0
            }}
          ]
        }}"#,
            plan_sha = "a".repeat(64),
            replay = replay.display(),
            log = log.display(),
        ),
    );

    let error = parse_fault_job_result(
        &result,
        "D7",
        "crash-cut-0007",
        &"a".repeat(64),
        &crash,
        &replay,
        &serial,
        "tx.ext4.crash.phase.D7",
        &log,
    )
    .expect_err("matrix evidence is required");
    assert!(error.contains("missing replay_matrix"));
}

#[test]
fn tier1_fault_job_result_rejects_unbound_manifest() {
    let root = temp_root("fault-job-result-unbound");
    let result = root.join("result.json");
    write_json(
        &result,
        r#"{
          "schema":"tx.ext4.fault_job_result.v1",
          "campaign_plan_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "hard_kill_observed":true,
          "e2fsck_exit":0
        }"#,
    );

    let error = parse_fault_job_result(
        &result,
        "D7",
        "crash-cut-0007",
        &"a".repeat(64),
        &root.join("crash.img"),
        &root.join("replay.img"),
        &root.join("serial.log"),
        "tx.ext4.crash.phase.D7",
        &root.join("e2fsck-fn.log"),
    )
    .expect_err("result must bind to a cut");
    assert!(error.contains("missing case"));
}

#[test]
fn tier1_live_plan_runs_g0_lints_before_building_candidate() {
    let root = temp_root("g0-plan");
    write_json(
        &root.join("tools/ext4/tier1/capability-ledger.json"),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        r#"{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"selection-authority-declared",
          "tier":"tier1",
          "source_lock":{
            "path":"external/xfstests",
            "revision":"acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509",
            "check_sha256":"104d9351e1b2d47f7992af650e0fed0054be0cecfd8fddd43e076e175ba80642"
          },
          "selected":[{"case_id":"generic/001"}]
        }"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/crash-cuts.json"),
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"catalog-authority-declared",
          "expanded_cut_count":1000,
          "families":[
            {"id":"D0"},{"id":"D1"},{"id":"D2"},{"id":"D3"},{"id":"D4"},
            {"id":"D5"},{"id":"D6"},{"id":"D7"},{"id":"D8"},{"id":"D9"},
            {"id":"D10"},{"id":"D11"},{"id":"D12"}
          ]
        }"#,
    );
    write_text(
        &root.join("tools/shell-tests/ext4-tier1.scn"),
        "# ext4 tier1\n",
    );

    let invocation =
        parse_tier1_args(&root, &["tier1".into(), "--dry-run".into()]).expect("dry run invocation");
    let actions = invocation.planned_actions();
    let g0_position = actions
        .iter()
        .position(|action| action == "run G0 ext4 ownership and durability lints")
        .expect("G0 lint action must be part of Tier 1 plan");
    let build_position = actions
        .iter()
        .position(|action| action == "build candidate")
        .expect("build action must be part of Tier 1 plan");
    assert!(
        g0_position < build_position,
        "G0 lints must run before expensive product evidence"
    );
}

#[test]
fn tier1_rejects_xfstests_selection_without_source_lock() {
    let root = temp_root("missing-source-lock");
    write_json(
        &root.join("tools/ext4/tier1/capability-ledger.json"),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        r#"{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"acceptance-ready",
          "tier":"tier1",
          "selected":[{"case_id":"generic/001"}]
        }"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/crash-cuts.json"),
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "families":[
            {"id":"D0"},{"id":"D1"},{"id":"D2"},{"id":"D3"},{"id":"D4"},
            {"id":"D5"},{"id":"D6"},{"id":"D7"},{"id":"D8"},{"id":"D9"},
            {"id":"D10"},{"id":"D11"},{"id":"D12"}
          ]
        }"#,
    );
    write_text(
        &root.join("tools/shell-tests/ext4-tier1.scn"),
        "# ext4 tier1\n",
    );

    let error = parse_tier1_args(&root, &["tier1".into(), "--dry-run".into()])
        .expect_err("source_lock is mandatory");
    assert!(error.contains("missing source_lock"));
}

#[test]
fn tier1_xfstests_source_lock_checks_revision_and_check_hash() {
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let root = temp_root("xfstests-source-lock");
    let xfstests = root.join("external/xfstests");
    fs::create_dir_all(&xfstests).unwrap();
    write_text(&xfstests.join("check"), "#!/bin/sh\nexit 0\n");
    run_git(&xfstests, &["init"]);
    run_git(&xfstests, &["add", "check"]);
    run_git(
        &xfstests,
        &[
            "-c",
            "user.name=tx",
            "-c",
            "user.email=tx@example.invalid",
            "commit",
            "-m",
            "pin check",
        ],
    );
    let revision = git_output(&xfstests, &["rev-parse", "HEAD"]);
    let check_sha256 = sha256_file(&xfstests.join("check")).unwrap();
    let good_lock = XfstestsSourceLock {
        path: PathBuf::from("external/xfstests"),
        revision: revision.trim().into(),
        check_sha256: check_sha256.clone(),
    };

    verify_xfstests_source_lock(&xfstests, &good_lock).expect("matching source lock");
    let bad_lock = XfstestsSourceLock {
        path: PathBuf::from("external/xfstests"),
        revision: revision.trim().into(),
        check_sha256: "0".repeat(64),
    };
    let error = verify_xfstests_source_lock(&xfstests, &bad_lock).expect_err("check hash mismatch");
    assert!(error.contains("xfstests check sha256 mismatch"));
}

#[test]
fn tier1_xfstests_source_lock_reports_local_status_without_network() {
    let root = temp_root("xfstests-local-status");
    let lock = XfstestsSourceLock {
        path: PathBuf::from("external/xfstests"),
        revision: "a".repeat(40),
        check_sha256: "b".repeat(64),
    };
    let missing = lock.local_status(&root);
    assert!(missing.contains("missing at"));
    assert!(missing.contains("live runner will clone pinned source"));

    let xfstests = root.join("external/xfstests");
    fs::create_dir_all(&xfstests).unwrap();
    let incomplete = lock.local_status(&root);
    assert!(incomplete.contains("incomplete at"));
    assert!(incomplete.contains("missing check script"));
}

#[test]
fn tier1_crash_cut_campaign_refuses_synthetic_completion() {
    let root = temp_root("crash-cut-synthetic");
    let mut run = RunWorkspace::create(&root, "crash-run").unwrap();
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "families":[
            {"id":"D0"},{"id":"D1"},{"id":"D2"},{"id":"D3"},{"id":"D4"},
            {"id":"D5"},{"id":"D6"},{"id":"D7"},{"id":"D8"},{"id":"D9"},
            {"id":"D10"},{"id":"D11"},{"id":"D12"}
          ]
        }"#,
    );
    let catalog = CrashCutCatalog::load_with_root(catalog_path, &root).unwrap();
    run.record_authority_inputs(&super::receipt::authority_input_summary(
        "0".repeat(64),
        catalog.sha256().to_string(),
        "0".repeat(64),
        "0".repeat(64),
        0,
        1000,
    ))
    .unwrap();

    let error =
        run_crash_cut_campaign(&mut run, &catalog).expect_err("crash cuts must not be synthesized");
    assert!(error.contains("refusing to synthesize completed=1000"));
}

#[test]
fn tier1_crash_cut_campaign_writes_deterministic_manifest_before_executor_error() {
    let root = temp_root("crash-cut-manifest");
    let mut run = RunWorkspace::create(&root, "crash-run").unwrap();
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_text(
        &root.join("tools/ext4/tier1/crash-workload.scn"),
        "# workload\n",
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-replay.scn"),
        "# replay\n",
    );
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/crash-workload.scn",
            "replay_script":"tools/ext4/tier1/crash-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    let catalog = CrashCutCatalog::load_with_root(catalog_path, &root).unwrap();

    let error =
        run_crash_cut_campaign(&mut run, &catalog).expect_err("executor remains fail-closed");
    assert!(error.contains("deterministic crash-cut executor is not implemented"));
    assert!(error.contains("crash-cut-outcomes.json"));
    let manifest = run.working_dir().join("crash-campaign-plan.json");
    let value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    assert_eq!(value["schema"], "tx.ext4.crash_cut_execution_manifest.v1");
    assert_eq!(value["expanded_cut_count"], 1000);
    assert_eq!(
        value["workload_script_sha256"],
        sha256_file(&root.join("tools/ext4/tier1/crash-workload.scn")).unwrap()
    );
    assert_eq!(
        value["replay_script_sha256"],
        sha256_file(&root.join("tools/ext4/tier1/crash-replay.scn")).unwrap()
    );
    let cuts = value["cuts"].as_array().unwrap();
    assert_eq!(cuts.len(), 1000);
    assert_eq!(cuts[0]["id"], "crash-cut-0000");
    assert_eq!(cuts[0]["family"], "D0");
    assert_eq!(cuts[0]["phase_marker"], "tx.ext4.crash.phase.D0");
    assert_eq!(cuts[13]["family"], "D0");
    assert_eq!(
        value["families"][0]["phase_marker"],
        "tx.ext4.crash.phase.D0"
    );
    assert_eq!(cuts[999]["immutable_image"], "crash-cut-0999.img");
}

#[test]
fn tier1_crash_cut_resume_discards_result_without_replay_serial() {
    let root = temp_root("crash-cut-resume-missing-replay-serial");
    let mut run = RunWorkspace::create(&root, "crash-run").unwrap();
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_text(
        &root.join("tools/ext4/tier1/crash-workload.scn"),
        "# workload\n",
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-replay.scn"),
        "# replay\n",
    );
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/crash-workload.scn",
            "replay_script":"tools/ext4/tier1/crash-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    let catalog = CrashCutCatalog::load_with_root(catalog_path, &root).unwrap();
    let stale_cut = run.working_dir().join("crash-cuts").join("crash-cut-0000");
    fs::create_dir_all(&stale_cut).unwrap();
    write_json(
        &stale_cut.join("result.json"),
        r#"{"schema":"tx.ext4.fault_job_result.v1"}"#,
    );

    let (next_idx, families, outcomes) =
        restore_completed_crash_cut_state(&mut run, &catalog, &"a".repeat(64)).unwrap();

    assert_eq!(next_idx, 0);
    assert!(families.is_empty());
    assert!(outcomes.is_empty());
    assert!(!stale_cut.exists());
}

#[test]
fn tier1_crash_cut_resume_discards_result_without_replay_success_marker() {
    let root = temp_root("crash-cut-resume-failed-replay-serial");
    let mut run = RunWorkspace::create(&root, "crash-run").unwrap();
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_text(
        &root.join("tools/ext4/tier1/crash-workload.scn"),
        "# workload\n",
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-replay.scn"),
        "# replay\n",
    );
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/crash-workload.scn",
            "replay_script":"tools/ext4/tier1/crash-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    let catalog = CrashCutCatalog::load_with_root(catalog_path, &root).unwrap();
    let campaign_plan_sha256 = "a".repeat(64);
    write_complete_restored_cut(
        &run,
        "crash-cut-0000",
        "D0",
        "tx.ext4.crash.phase.D0",
        &campaign_plan_sha256,
    );
    let stale_cut = run.working_dir().join("crash-cuts").join("crash-cut-0000");
    write_text(
        &stale_cut.join("replay-serial.log"),
        "crash-replay-mount:0\n/ # \n",
    );

    let (next_idx, families, outcomes) =
        restore_completed_crash_cut_state(&mut run, &catalog, &campaign_plan_sha256).unwrap();

    assert_eq!(next_idx, 0);
    assert!(families.is_empty());
    assert!(outcomes.is_empty());
    assert!(!stale_cut.exists());
}

#[test]
fn tier1_crash_cut_resume_restores_large_image_hashes_from_executor_plan() {
    let root = temp_root("crash-cut-resume-executor-plan-hashes");
    let mut run = RunWorkspace::create(&root, "crash-run").unwrap();
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_text(
        &root.join("tools/ext4/tier1/crash-workload.scn"),
        "# workload\n",
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-replay.scn"),
        "# replay\n",
    );
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/crash-workload.scn",
            "replay_script":"tools/ext4/tier1/crash-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    let catalog = CrashCutCatalog::load_with_root(catalog_path, &root).unwrap();
    let campaign_plan_sha256 = "a".repeat(64);
    write_complete_restored_cut(
        &run,
        "crash-cut-0000",
        "D0",
        "tx.ext4.crash.phase.D0",
        &campaign_plan_sha256,
    );
    let job_dir = run.working_dir().join("crash-cuts/crash-cut-0000");
    let crash_image = job_dir.join("crash.img");
    let replay_image = job_dir.join("replay.img");
    let crash_sha256 = sha256_file(&crash_image).unwrap();
    let replay_sha256 = sha256_file(&replay_image).unwrap();
    let replay_recovery_log = job_dir.join("replay-recovery.log");
    write_text(&replay_recovery_log, "replay recovery ok\n");
    write_json(
        &job_dir.join("executor-plan.json"),
        &serde_json::to_string_pretty(&serde_json::json!({
            "schema": "tx.ext4.fault_qemu_executor_plan.v1",
            "preserved_images": {
                "crash": crash_image.display().to_string(),
                "replay": replay_image.display().to_string(),
                "source": job_dir.join("roles/scratch.img").display().to_string(),
                "status": "copied-after-runner-termination"
            },
            "staged_role_image_digests": {
                "scratch": crash_sha256
            },
            "replay_image_recovery": {
                "image": replay_image.display().to_string(),
                "image_sha256": replay_sha256,
                "log": replay_recovery_log.display().to_string(),
                "log_sha256": sha256_file(&replay_recovery_log).unwrap(),
                "exit_code": 0,
                "status": "ok"
            }
        }))
        .unwrap(),
    );

    let (_next_idx, _families, outcomes) =
        restore_completed_crash_cut_state(&mut run, &catalog, &campaign_plan_sha256).unwrap();

    assert_eq!(outcomes[0].immutable_image_sha256, replay_sha256);
    assert_eq!(
        run.cached_artifact_sha256(&crash_image),
        Some(crash_sha256.as_str())
    );
    assert_eq!(
        run.cached_artifact_sha256(&replay_image),
        Some(replay_sha256.as_str())
    );
}

#[test]
fn tier1_crash_cut_campaign_consumes_executor_outcome_manifest() {
    let root = temp_root("crash-cut-outcome-consume");
    let mut run = RunWorkspace::create(&root, "crash-run").unwrap();
    let catalog_path = root.join("tools/ext4/tier1/crash-cuts.json");
    write_text(
        &root.join("tools/ext4/tier1/crash-workload.scn"),
        "# workload\n",
    );
    write_text(
        &root.join("tools/ext4/tier1/crash-replay.scn"),
        "# replay\n",
    );
    write_json(
        &catalog_path,
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"acceptance-ready",
          "expanded_cut_count":1000,
          "campaign":{
            "workload_script":"tools/ext4/tier1/crash-workload.scn",
            "replay_script":"tools/ext4/tier1/crash-replay.scn",
            "kill_policy":"deterministic-phase-marker-v1",
            "e2fsck_mode":"immutable-copy"
          },
          "families":[
            {"id":"D0","phase_marker":"tx.ext4.crash.phase.D0"},
            {"id":"D1","phase_marker":"tx.ext4.crash.phase.D1"},
            {"id":"D2","phase_marker":"tx.ext4.crash.phase.D2"},
            {"id":"D3","phase_marker":"tx.ext4.crash.phase.D3"},
            {"id":"D4","phase_marker":"tx.ext4.crash.phase.D4"},
            {"id":"D5","phase_marker":"tx.ext4.crash.phase.D5"},
            {"id":"D6","phase_marker":"tx.ext4.crash.phase.D6"},
            {"id":"D7","phase_marker":"tx.ext4.crash.phase.D7"},
            {"id":"D8","phase_marker":"tx.ext4.crash.phase.D8"},
            {"id":"D9","phase_marker":"tx.ext4.crash.phase.D9"},
            {"id":"D10","phase_marker":"tx.ext4.crash.phase.D10"},
            {"id":"D11","phase_marker":"tx.ext4.crash.phase.D11"},
            {"id":"D12","phase_marker":"tx.ext4.crash.phase.D12"}
          ]
        }"#,
    );
    write_json(
        &run.working_dir().join("crash-cut-outcomes.json"),
        r#"{
          "schema":"tx.ext4.crash_cut_outcome_manifest.v1",
          "completed":2,
          "required":1000,
          "families":["D0","D1"],
          "outcomes":[
            {
              "cut_id":"crash-cut-0000",
              "immutable_image_sha256":"1111111111111111111111111111111111111111111111111111111111111111",
              "replay_serial_sha256":"3333333333333333333333333333333333333333333333333333333333333333",
              "e2fsck_exit_code":0,
              "replay_exit_code":0
            },
            {
              "cut_id":"crash-cut-0001",
              "immutable_image_sha256":"2222222222222222222222222222222222222222222222222222222222222222",
              "replay_serial_sha256":"4444444444444444444444444444444444444444444444444444444444444444",
              "e2fsck_exit_code":0,
              "replay_exit_code":0
            }
          ]
        }"#,
    );
    let catalog = CrashCutCatalog::load_with_root(catalog_path, &root).unwrap();

    let evidence =
        run_crash_cut_campaign(&mut run, &catalog).expect("clean executor outcomes are accepted");
    assert_eq!(evidence.summary.completed, 2);
    assert_eq!(evidence.summary.required, 1000);
    assert_eq!(evidence.immutable_images.len(), 2);
    assert_eq!(evidence.immutable_images[0].role, "crash-cut-0000");
    assert!(run.working_dir().join("crash-campaign-plan.json").is_file());
}

#[test]
fn tier1_crash_cut_evidence_requires_e2fsck_image_per_completed_cut() {
    let summary = super::receipt::CrashCuts {
        completed: 2,
        required: 1000,
        families: vec!["D0".into()],
    };
    let one_image = vec![super::receipt::E2fsckImageResult {
        role: "crash-cut-0000".into(),
        image_sha256: "1".repeat(64),
        exit_code: 0,
    }];
    let error = CrashCutCampaignEvidence::new(summary.clone(), one_image)
        .expect_err("completed crash cuts need matching e2fsck image records");
    assert!(error.contains("crash-cut e2fsck coverage mismatch"));

    let two_images = vec![
        super::receipt::E2fsckImageResult {
            role: "crash-cut-0000".into(),
            image_sha256: "1".repeat(64),
            exit_code: 0,
        },
        super::receipt::E2fsckImageResult {
            role: "crash-cut-0001".into(),
            image_sha256: "2".repeat(64),
            exit_code: 0,
        },
    ];
    CrashCutCampaignEvidence::new(summary, two_images).expect("matching crash-cut e2fsck evidence");
}

#[test]
fn tier1_crash_cut_evidence_requires_clean_per_cut_outcomes() {
    let summary = super::receipt::CrashCuts {
        completed: 2,
        required: 1000,
        families: vec!["D0".into(), "D1".into()],
    };
    let missing = vec![CrashCutOutcome {
        cut_id: "crash-cut-0000".into(),
        immutable_image_sha256: "1".repeat(64),
        replay_serial_sha256: "3".repeat(64),
        e2fsck_exit_code: 0,
        replay_exit_code: 0,
    }];
    let error = CrashCutCampaignEvidence::from_outcomes(summary.clone(), missing)
        .expect_err("completed cuts need matching outcome rows");
    assert!(error.contains("crash-cut outcome coverage mismatch"));

    let dirty_replay = vec![
        CrashCutOutcome {
            cut_id: "crash-cut-0000".into(),
            immutable_image_sha256: "1".repeat(64),
            replay_serial_sha256: "3".repeat(64),
            e2fsck_exit_code: 0,
            replay_exit_code: 0,
        },
        CrashCutOutcome {
            cut_id: "crash-cut-0001".into(),
            immutable_image_sha256: "2".repeat(64),
            replay_serial_sha256: "4".repeat(64),
            e2fsck_exit_code: 0,
            replay_exit_code: 1,
        },
    ];
    let error = CrashCutCampaignEvidence::from_outcomes(summary.clone(), dirty_replay)
        .expect_err("replay failures must block crash evidence");
    assert!(error.contains("replay failed for crash-cut-0001"));

    let clean = vec![
        CrashCutOutcome {
            cut_id: "crash-cut-0000".into(),
            immutable_image_sha256: "1".repeat(64),
            replay_serial_sha256: "3".repeat(64),
            e2fsck_exit_code: 0,
            replay_exit_code: 0,
        },
        CrashCutOutcome {
            cut_id: "crash-cut-0001".into(),
            immutable_image_sha256: "2".repeat(64),
            replay_serial_sha256: "4".repeat(64),
            e2fsck_exit_code: 0,
            replay_exit_code: 0,
        },
    ];
    let evidence =
        CrashCutCampaignEvidence::from_outcomes(summary, clean).expect("clean outcomes aggregate");
    assert_eq!(evidence.immutable_images.len(), 2);
    assert_eq!(evidence.immutable_images[0].role, "crash-cut-0000");
    assert_eq!(evidence.immutable_images[1].role, "crash-cut-0001");
}

#[test]
fn tier1_crash_cut_outcome_manifest_parses_into_clean_evidence() {
    let root = temp_root("crash-outcome-manifest");
    let path = root.join("crash-cut-outcomes.json");
    write_json(
        &path,
        r#"{
          "schema":"tx.ext4.crash_cut_outcome_manifest.v1",
          "completed":2,
          "required":1000,
          "families":["D0","D1"],
          "outcomes":[
            {
              "cut_id":"crash-cut-0000",
              "immutable_image_sha256":"1111111111111111111111111111111111111111111111111111111111111111",
              "replay_serial_sha256":"3333333333333333333333333333333333333333333333333333333333333333",
              "e2fsck_exit_code":0,
              "replay_exit_code":0
            },
            {
              "cut_id":"crash-cut-0001",
              "immutable_image_sha256":"2222222222222222222222222222222222222222222222222222222222222222",
              "replay_serial_sha256":"4444444444444444444444444444444444444444444444444444444444444444",
              "e2fsck_exit_code":0,
              "replay_exit_code":0
            }
          ]
        }"#,
    );

    let evidence =
        CrashCutCampaignEvidence::from_outcome_manifest(&path).expect("clean outcome manifest");
    assert_eq!(evidence.summary.completed, 2);
    assert_eq!(evidence.summary.required, 1000);
    assert_eq!(evidence.summary.families, vec!["D0", "D1"]);
    assert_eq!(evidence.immutable_images.len(), 2);
    assert_eq!(evidence.immutable_images[0].role, "crash-cut-0000");
}

#[test]
fn tier1_crash_cut_outcome_manifest_writes_completed_before_required() {
    let root = temp_root("crash-outcome-manifest-field-order");
    let run = RunWorkspace::create(&root, "crash-run").unwrap();
    let path = write_crash_cut_outcome_manifest(
        &run,
        1000,
        116,
        vec!["D0".into()],
        vec![CrashCutOutcome {
            cut_id: "crash-cut-0884".into(),
            immutable_image_sha256: "1".repeat(64),
            replay_serial_sha256: "2".repeat(64),
            e2fsck_exit_code: 0,
            replay_exit_code: 0,
        }],
    )
    .expect("write outcome manifest");
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();

    assert_eq!(manifest["completed"], 116);
    assert_eq!(manifest["required"], 1000);
}

#[test]
fn tier1_crash_cut_outcome_manifest_rejects_dirty_or_incomplete_rows() {
    let root = temp_root("dirty-crash-outcome-manifest");
    let path = root.join("crash-cut-outcomes.json");
    write_json(
        &path,
        r#"{
          "schema":"tx.ext4.crash_cut_outcome_manifest.v1",
          "completed":2,
          "required":1000,
          "families":["D0"],
          "outcomes":[
            {
              "cut_id":"crash-cut-0000",
              "immutable_image_sha256":"1111111111111111111111111111111111111111111111111111111111111111",
              "replay_serial_sha256":"3333333333333333333333333333333333333333333333333333333333333333",
              "e2fsck_exit_code":0,
              "replay_exit_code":0
            }
          ]
        }"#,
    );
    let error = CrashCutCampaignEvidence::from_outcome_manifest(&path)
        .expect_err("missing outcome rows must fail closed");
    assert!(error.contains("crash-cut outcome coverage mismatch"));

    write_json(
        &path,
        r#"{
          "schema":"tx.ext4.crash_cut_outcome_manifest.v1",
          "completed":1,
          "required":1000,
          "families":["D0"],
          "outcomes":[
            {
              "cut_id":"crash-cut-0000",
              "immutable_image_sha256":"1111111111111111111111111111111111111111111111111111111111111111",
              "replay_serial_sha256":"3333333333333333333333333333333333333333333333333333333333333333",
              "e2fsck_exit_code":4,
              "replay_exit_code":0
            }
          ]
        }"#,
    );
    let error = CrashCutCampaignEvidence::from_outcome_manifest(&path)
        .expect_err("dirty e2fsck must fail closed");
    assert!(error.contains("e2fsck failed for crash-cut-0000"));
}

#[test]
fn tier1_crash_cut_outcome_manifest_requires_replay_serial_evidence() {
    let root = temp_root("crash-outcome-replay-serial");
    let path = root.join("crash-cut-outcomes.json");
    write_json(
        &path,
        r#"{
          "schema":"tx.ext4.crash_cut_outcome_manifest.v1",
          "completed":1,
          "required":1000,
          "families":["D0"],
          "outcomes":[
            {
              "cut_id":"crash-cut-0000",
              "immutable_image_sha256":"1111111111111111111111111111111111111111111111111111111111111111",
              "e2fsck_exit_code":0,
              "replay_exit_code":0
            }
          ]
        }"#,
    );

    let error = CrashCutCampaignEvidence::from_outcome_manifest(&path)
        .expect_err("replay serial evidence must be present");
    assert!(error.contains("missing outcome.replay_serial_sha256"));
}

#[test]
fn tier1_xfstests_parser_requires_passed_all_summary() {
    let summary = parse_xfstests_summary(
        "FSTYP -- ext4\ngeneric/013 1s\nRan: generic/013\nPassed all 1 tests\n",
        1,
    )
    .expect("passed all summary");
    assert_eq!(summary.passed, 1);
    assert_eq!(summary.not_run, 0);
    assert_eq!(summary.failed, 0);

    let missing = parse_xfstests_summary("Ran: generic/013\n", 1)
        .expect_err("missing summary must fail closed");
    assert!(missing.contains("missing `Passed all N tests`"));
}

#[test]
fn tier1_xfstests_parser_rejects_notrun_and_count_mismatch() {
    let notrun = parse_xfstests_summary(
        "generic/013 -- not run: requires scratch\nNot run: generic/013\n",
        1,
    )
    .expect_err("not-run must fail closed");
    assert!(notrun.contains("not-run cases"));

    let mismatch = parse_xfstests_summary(
        "generic/013 1s\ngeneric/035 1s\nRan: generic/013 generic/035\nPassed all 2 tests\n",
        1,
    )
    .expect_err("count mismatch must fail closed");
    assert!(mismatch.contains("passed count mismatch"));
}

fn temp_root(suffix: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time moved forward")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "tx-xtask-ext4-{suffix}-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create temp root");
    root
}

struct FaultRequestFixture {
    root: PathBuf,
    run: RunWorkspace,
    campaign_manifest: PathBuf,
    family: CrashCutFamily,
    campaign: CrashCutCampaignPlan,
    test_image: PathBuf,
    scratch_image: PathBuf,
    workload_image: PathBuf,
}

fn fault_request_fixture(suffix: &str) -> FaultRequestFixture {
    let root = temp_root(suffix);
    let run = RunWorkspace::create(&root, "crash-run").unwrap();
    let campaign_manifest = run.working_dir().join("crash-campaign-plan.json");
    write_text(&campaign_manifest, "campaign-plan\n");
    let test_image = root.join("test.img");
    let scratch_image = root.join("scratch.img");
    let workload_image = root.join("workload.img");
    for (path, contents) in [
        (&test_image, "test-image\n"),
        (&scratch_image, "scratch-image\n"),
        (&workload_image, "workload-image\n"),
    ] {
        write_text(path, contents);
    }
    FaultRequestFixture {
        root,
        run,
        campaign_manifest,
        family: CrashCutFamily {
            id: "D7".into(),
            phase_marker: Some("tx.ext4.crash.phase.D7".into()),
        },
        campaign: CrashCutCampaignPlan {
            workload_script: PathBuf::from("tools/ext4/tier1/crash-workload.scn"),
            workload_script_sha256: "a".repeat(64),
            replay_script: PathBuf::from("tools/ext4/tier1/crash-replay.scn"),
            replay_script_sha256: "b".repeat(64),
            kill_policy: "deterministic-phase-marker-v1".into(),
            e2fsck_mode: "immutable-copy".into(),
        },
        test_image,
        scratch_image,
        workload_image,
    }
}

fn write_tier1_authority_fixture(root: &PathBuf) {
    write_json(
        &root.join("tools/ext4/tier1/capability-ledger.json"),
        r#"{"schema":"tx.ext4.capability_ledger.v1"}"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/xfstests-selection.json"),
        r#"{
          "schema":"tx.ext4.xfstests_selection_ledger.v1",
          "status":"selection-authority-declared",
          "tier":"tier1",
          "source_lock":{
            "path":"external/xfstests",
            "revision":"acb6d4cb84205a8e3f19ca470cfcf7bf6d93a509",
            "check_sha256":"104d9351e1b2d47f7992af650e0fed0054be0cecfd8fddd43e076e175ba80642"
          },
          "selected":[{"case_id":"generic/001"}]
        }"#,
    );
    write_json(
        &root.join("tools/ext4/tier1/crash-cuts.json"),
        r#"{
          "schema":"tx.ext4.crash_cut_catalog.v1",
          "status":"catalog-authority-declared",
          "expanded_cut_count":1000,
          "families":[
            {"id":"D0"},{"id":"D1"},{"id":"D2"},{"id":"D3"},{"id":"D4"},
            {"id":"D5"},{"id":"D6"},{"id":"D7"},{"id":"D8"},{"id":"D9"},
            {"id":"D10"},{"id":"D11"},{"id":"D12"}
          ]
        }"#,
    );
    write_text(
        &root.join("tools/shell-tests/ext4-tier1.scn"),
        "# ext4 tier1\n",
    );
}

fn write_json(path: &PathBuf, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, text).expect("write json fixture");
}

fn write_text(path: &PathBuf, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, text).expect("write text fixture");
}

fn shell_arg_value(args: &[String], flag: &str) -> String {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|idx| args.get(idx + 1))
        .cloned()
        .unwrap_or_else(|| panic!("missing {flag} in args: {args:?}"))
}

fn write_complete_restored_cut(
    run: &RunWorkspace,
    cut_id: &str,
    case_id: &str,
    phase_marker: &str,
    campaign_plan_sha256: &str,
) {
    let job_dir = run.working_dir().join("crash-cuts").join(cut_id);
    fs::create_dir_all(&job_dir).unwrap();
    let crash_image = job_dir.join("crash.img");
    let replay_image = job_dir.join("replay.img");
    let serial_log = job_dir.join("serial.log");
    let replay_serial_log = job_dir.join("replay-serial.log");
    let e2fsck_log = job_dir.join("e2fsck-fn.log");
    let linux_rw_log = job_dir.join("linux-rw-replay.log");
    let linux_post_log = job_dir.join("linux-post-replay-e2fsck.log");
    let tx_remount_log = job_dir.join("tx-remount.log");
    let semantic_log = job_dir.join("semantic-oracle.log");

    write_text(&crash_image, "crash-image\n");
    write_text(&replay_image, "replay-image\n");
    write_text(&serial_log, &format!("{phase_marker}\n"));
    write_text(
        &replay_serial_log,
        "crash-replay-mount:0\ncrash-replay-inspect:0\n",
    );
    write_text(&e2fsck_log, "e2fsck-ok\n");
    write_text(&linux_rw_log, "linux-rw-ok\n");
    write_text(&linux_post_log, "linux-post-ok\n");
    write_text(&tx_remount_log, "tx-remount-ok\n");
    write_text(&semantic_log, "semantic-ok\n");

    let replay_arg = replay_image.display().to_string();
    let linux_image = job_dir.join("linux-rw-replay.img");
    let tx_image = job_dir.join("tx-remount.img");
    let semantic_image = job_dir.join("semantic-oracle.img");
    let result = serde_json::json!({
        "schema": "tx.ext4.fault_job_result.v1",
        "campaign_plan_sha256": campaign_plan_sha256,
        "case": case_id,
        "cut": cut_id,
        "hard_kill_observed": true,
        "replay_attempted": true,
        "e2fsck_exit": 0,
        "e2fsck_checks": [
            {
                "tool": "e2fsck",
                "args": ["-fn", replay_arg],
                "log": e2fsck_log.display().to_string(),
                "log_sha256": sha256_file(&e2fsck_log).unwrap(),
                "exit_code": 0
            }
        ],
        "replay_matrix": [
            {
                "id": "linux-rw-replay",
                "image": linux_image.display().to_string(),
                "image_sha256": "1".repeat(64),
                "log": linux_rw_log.display().to_string(),
                "log_sha256": sha256_file(&linux_rw_log).unwrap(),
                "exit_code": 0
            },
            {
                "id": "linux-post-replay-e2fsck",
                "image": linux_image.display().to_string(),
                "image_sha256": "2".repeat(64),
                "args": ["-fn", linux_image.display().to_string()],
                "log": linux_post_log.display().to_string(),
                "log_sha256": sha256_file(&linux_post_log).unwrap(),
                "exit_code": 0
            },
            {
                "id": "tx-remount",
                "image": tx_image.display().to_string(),
                "image_sha256": "3".repeat(64),
                "log": tx_remount_log.display().to_string(),
                "log_sha256": sha256_file(&tx_remount_log).unwrap(),
                "exit_code": 0
            }
        ],
        "semantic_oracles": [
            {
                "id": "debugfs-file-hash-namespace",
                "image": semantic_image.display().to_string(),
                "image_sha256": "4".repeat(64),
                "log": semantic_log.display().to_string(),
                "log_sha256": sha256_file(&semantic_log).unwrap(),
                "exit_code": 0,
                "expected": {
                    "present": {
                        "/": {}
                    }
                }
            }
        ]
    });
    fs::write(
        job_dir.join("result.json"),
        serde_json::to_string_pretty(&result).unwrap(),
    )
    .unwrap();
}

fn run_git(cwd: &PathBuf, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed with {status}");
}

fn git_output(cwd: &PathBuf, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8(output.stdout).expect("git utf8")
}
