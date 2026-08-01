use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Tier1Authorities, parse_tier1_args, run_workspace::RunWorkspace};

#[test]
fn run_workspace_finalizes_once_and_cleans_temporary_state() {
    let root = temp_root("finalize");
    let mut run = RunWorkspace::create(&root, "test-run").unwrap();
    run.record_artifact("scratch", root.join("scratch.img"))
        .unwrap();
    let receipt = run.finalize().unwrap();
    assert!(receipt.exists());
    assert!(!run.temporary_path_for_test().exists());
    assert!(root.join("target/ext4/tier1/test-run").exists());
}

#[test]
fn run_workspace_failure_kills_children_writes_receipt_and_cleans_on_drop() {
    let root = temp_root("failure");
    let temp = root.join("target/ext4/tier1/.failed-run.tmp");
    {
        let mut run = RunWorkspace::create(&root, "failed-run").unwrap();
        let child = run.spawn_test_child("exit 17").unwrap();
        run.record_child(child);
        run.mark_failed_for_test("child-exit");
    }
    assert!(
        root.join("target/ext4/tier1/failed-run/failed-receipt.json")
            .exists()
    );
    assert!(!temp.exists());
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
        r#"{"schema":"tx.ext4.xfstests_selection_ledger.v1","tier":"tier1","selected":["generic/001"]}"#,
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

    parse_tier1_args(&root, &["tier1".into(), "--dry-run".into()])
        .expect("dry-run still resolves placeholder authority hashes");
    let error = Tier1Authorities::load(&root)
        .unwrap()
        .ensure_live_acceptance_ready()
        .expect_err("live runner must not promote placeholder authorities");
    assert!(error.contains("xfstests selection status is `selection-authority-declared`"));
    assert!(error.contains("crash-cut catalog status is `catalog-authority-declared`"));
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
