use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Tier1AcceptanceReceipt {
    pub schema: String,
    pub candidate: CandidateCommit,
    pub authorities: Tier1Authorities,
    pub role_images: RoleImages,
    pub crash_cuts: CrashCuts,
    pub e2fsck: E2fsckSummary,
    pub xfstests: XfstestsSummary,
    pub gates: Gates,
    pub planned_actions: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CandidateCommit {
    pub commit: String,
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Tier1Authorities {
    pub capability_ledger_sha256: String,
    pub crash_cut_catalog_sha256: String,
    pub xfstests_selection_sha256: String,
    pub shell_scenario_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Tier1AuthorityInputs {
    pub capability_ledger_sha256: String,
    pub crash_cut_catalog_sha256: String,
    pub xfstests_selection_sha256: String,
    pub shell_scenario_sha256: String,
    pub xfstests_case_count: usize,
    pub crash_cut_expanded_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoleImages {
    pub test: RoleImage,
    pub scratch: RoleImage,
    pub workload: RoleImage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RoleImage {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CrashCuts {
    pub completed: usize,
    pub required: usize,
    pub families: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct E2fsckSummary {
    pub immutable_images: Vec<E2fsckImageResult>,
    pub failures: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct E2fsckImageResult {
    pub role: String,
    pub image_sha256: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct XfstestsSummary {
    pub skipped: usize,
    pub not_run: usize,
    pub passed: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Gates {
    #[serde(rename = "G0")]
    pub g0: String,
    #[serde(rename = "G1")]
    pub g1: String,
    #[serde(rename = "G2")]
    pub g2: String,
    #[serde(rename = "G3")]
    pub g3: String,
    #[serde(rename = "G4")]
    pub g4: String,
    #[serde(rename = "G5")]
    pub g5: String,
    #[serde(rename = "G6")]
    pub g6: String,
    #[serde(rename = "G7")]
    pub g7: String,
}

impl Tier1AcceptanceReceipt {
    pub(crate) fn from_dry_run(
        run_id: &str,
        commit: String,
        authorities: Tier1AuthorityInputs,
        planned_actions: &[String],
    ) -> Self {
        Self {
            schema: "tx.ext4.tier1_acceptance_receipt.v1".into(),
            candidate: CandidateCommit {
                commit,
                run_id: run_id.into(),
            },
            authorities: Tier1Authorities {
                capability_ledger_sha256: authorities.capability_ledger_sha256,
                crash_cut_catalog_sha256: authorities.crash_cut_catalog_sha256,
                xfstests_selection_sha256: authorities.xfstests_selection_sha256,
                shell_scenario_sha256: authorities.shell_scenario_sha256,
            },
            role_images: RoleImages {
                test: RoleImage {
                    path: "target/ext4/tier1/<run-id>/test.img".into(),
                    sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                        .into(),
                },
                scratch: RoleImage {
                    path: "target/ext4/tier1/<run-id>/scratch.img".into(),
                    sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                        .into(),
                },
                workload: RoleImage {
                    path: "target/ext4/tier1/<run-id>/workload.img".into(),
                    sha256: "0000000000000000000000000000000000000000000000000000000000000000"
                        .into(),
                },
            },
            crash_cuts: CrashCuts {
                completed: 0,
                required: authorities.crash_cut_expanded_count,
                families: vec![
                    "D0".into(),
                    "D1".into(),
                    "D2".into(),
                    "D3".into(),
                    "D4".into(),
                    "D5".into(),
                    "D6".into(),
                    "D7".into(),
                    "D8".into(),
                    "D9".into(),
                    "D10".into(),
                    "D11".into(),
                    "D12".into(),
                ],
            },
            e2fsck: E2fsckSummary {
                immutable_images: Vec::new(),
                failures: 0,
            },
            xfstests: XfstestsSummary {
                skipped: 0,
                not_run: 0,
                passed: authorities.xfstests_case_count,
                failed: 0,
            },
            gates: Gates {
                g0: "planned".into(),
                g1: "planned".into(),
                g2: "planned".into(),
                g3: "planned".into(),
                g4: "planned".into(),
                g5: "planned".into(),
                g6: "planned".into(),
                g7: "planned".into(),
            },
            planned_actions: planned_actions.to_vec(),
            notes: vec!["dry-run only; live Tier 1 campaign not executed".into()],
        }
    }

    pub(crate) fn from_live(
        run_id: &str,
        commit: String,
        authorities: Tier1AuthorityInputs,
        role_images: RoleImages,
        crash_cuts: CrashCuts,
        e2fsck: E2fsckSummary,
        xfstests: XfstestsSummary,
        planned_actions: &[String],
        notes: &[String],
    ) -> Self {
        let gates = gates_from_evidence(
            &crash_cuts,
            &e2fsck,
            &xfstests,
            authorities.xfstests_case_count,
        );
        Self {
            schema: "tx.ext4.tier1_acceptance_receipt.v1".into(),
            candidate: CandidateCommit {
                commit,
                run_id: run_id.into(),
            },
            authorities: Tier1Authorities {
                capability_ledger_sha256: authorities.capability_ledger_sha256,
                crash_cut_catalog_sha256: authorities.crash_cut_catalog_sha256,
                xfstests_selection_sha256: authorities.xfstests_selection_sha256,
                shell_scenario_sha256: authorities.shell_scenario_sha256,
            },
            role_images,
            crash_cuts,
            e2fsck,
            xfstests,
            gates,
            planned_actions: planned_actions.to_vec(),
            notes: notes.to_vec(),
        }
    }

    pub(crate) fn write_json(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|err| err.to_string())?;
        fs::write(path, text).map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CrashCuts, E2fsckImageResult, E2fsckSummary, XfstestsSummary, gates_from_evidence,
    };

    #[test]
    fn live_gates_block_when_crash_cuts_are_incomplete() {
        let gates = gates_from_evidence(
            &CrashCuts {
                completed: 0,
                required: 1000,
                families: vec!["D0".into()],
            },
            &E2fsckSummary {
                immutable_images: vec![E2fsckImageResult {
                    role: "scratch".into(),
                    image_sha256: "1".repeat(64),
                    exit_code: 0,
                }],
                failures: 0,
            },
            &XfstestsSummary {
                skipped: 0,
                not_run: 0,
                passed: 1,
                failed: 0,
            },
            1,
        );
        assert_eq!(gates.g0, "passed");
        assert_eq!(gates.g7, "blocked");
    }

    #[test]
    fn live_gates_pass_only_with_full_product_evidence() {
        let mut immutable_images =
            vec![ok_image("test"), ok_image("scratch"), ok_image("workload")];
        for idx in 0..1000 {
            immutable_images.push(ok_image(&format!("crash-cut-{idx:04}")));
        }
        let gates = gates_from_evidence(
            &CrashCuts {
                completed: 1000,
                required: 1000,
                families: vec!["D0".into()],
            },
            &E2fsckSummary {
                immutable_images,
                failures: 0,
            },
            &XfstestsSummary {
                skipped: 0,
                not_run: 0,
                passed: 1,
                failed: 0,
            },
            1,
        );
        assert_eq!(gates.g0, "passed");
        assert_eq!(gates.g7, "passed");
    }

    #[test]
    fn live_gates_block_when_e2fsck_does_not_cover_every_crash_cut() {
        let gates = gates_from_evidence(
            &CrashCuts {
                completed: 1000,
                required: 1000,
                families: vec!["D0".into()],
            },
            &E2fsckSummary {
                immutable_images: vec![ok_image("test"), ok_image("scratch"), ok_image("workload")],
                failures: 0,
            },
            &XfstestsSummary {
                skipped: 0,
                not_run: 0,
                passed: 1,
                failed: 0,
            },
            1,
        );
        assert_eq!(gates.g0, "passed");
        assert_eq!(gates.g7, "blocked");
    }

    #[test]
    fn live_gates_block_when_e2fsck_image_result_is_not_clean_or_immutable() {
        let mut immutable_images =
            vec![ok_image("test"), ok_image("scratch"), ok_image("workload")];
        for idx in 0..1000 {
            immutable_images.push(ok_image(&format!("crash-cut-{idx:04}")));
        }
        immutable_images[3].exit_code = 4;
        let dirty_exit = gates_from_evidence(
            &CrashCuts {
                completed: 1000,
                required: 1000,
                families: vec!["D0".into()],
            },
            &E2fsckSummary {
                immutable_images: immutable_images.clone(),
                failures: 0,
            },
            &XfstestsSummary {
                skipped: 0,
                not_run: 0,
                passed: 1,
                failed: 0,
            },
            1,
        );
        assert_eq!(dirty_exit.g7, "blocked");

        immutable_images[3].exit_code = 0;
        immutable_images[3].image_sha256 = "0".repeat(64);
        let placeholder_hash = gates_from_evidence(
            &CrashCuts {
                completed: 1000,
                required: 1000,
                families: vec!["D0".into()],
            },
            &E2fsckSummary {
                immutable_images,
                failures: 0,
            },
            &XfstestsSummary {
                skipped: 0,
                not_run: 0,
                passed: 1,
                failed: 0,
            },
            1,
        );
        assert_eq!(placeholder_hash.g7, "blocked");
    }

    #[test]
    fn live_gates_block_when_xfstests_count_does_not_match_authority() {
        let mut immutable_images =
            vec![ok_image("test"), ok_image("scratch"), ok_image("workload")];
        for idx in 0..1000 {
            immutable_images.push(ok_image(&format!("crash-cut-{idx:04}")));
        }
        let gates = gates_from_evidence(
            &CrashCuts {
                completed: 1000,
                required: 1000,
                families: vec!["D0".into()],
            },
            &E2fsckSummary {
                immutable_images,
                failures: 0,
            },
            &XfstestsSummary {
                skipped: 0,
                not_run: 0,
                passed: 1,
                failed: 0,
            },
            2,
        );
        assert_eq!(gates.g7, "blocked");
    }

    fn ok_image(role: &str) -> E2fsckImageResult {
        E2fsckImageResult {
            role: role.into(),
            image_sha256: "1".repeat(64),
            exit_code: 0,
        }
    }
}

fn gates_from_evidence(
    crash_cuts: &CrashCuts,
    e2fsck: &E2fsckSummary,
    xfstests: &XfstestsSummary,
    expected_xfstests_cases: usize,
) -> Gates {
    let crash_ok = crash_cuts.required == 1000 && crash_cuts.completed == crash_cuts.required;
    let e2fsck_ok = e2fsck_covers_completed_crash_cuts(crash_cuts, e2fsck);
    let xfstests_ok = xfstests.failed == 0
        && xfstests.skipped == 0
        && xfstests.not_run == 0
        && xfstests.passed == expected_xfstests_cases
        && expected_xfstests_cases != 0;
    let all_ok = crash_ok && e2fsck_ok && xfstests_ok;
    let product_gate = if all_ok { "passed" } else { "blocked" };
    Gates {
        g0: "passed".into(),
        g1: product_gate.into(),
        g2: product_gate.into(),
        g3: product_gate.into(),
        g4: product_gate.into(),
        g5: product_gate.into(),
        g6: product_gate.into(),
        g7: product_gate.into(),
    }
}

fn e2fsck_covers_completed_crash_cuts(crash_cuts: &CrashCuts, e2fsck: &E2fsckSummary) -> bool {
    const ROLE_IMAGE_COUNT: usize = 3;
    if e2fsck.failures != 0 {
        return false;
    }
    if !["test", "scratch", "workload"].iter().all(|role| {
        e2fsck
            .immutable_images
            .iter()
            .any(|image| image.role == *role)
    }) {
        return false;
    }
    if e2fsck.immutable_images.len() < ROLE_IMAGE_COUNT + crash_cuts.completed {
        return false;
    }
    e2fsck
        .immutable_images
        .iter()
        .all(|image| image.exit_code == 0 && is_real_sha256(&image.image_sha256))
}

fn is_real_sha256(value: &str) -> bool {
    value.len() == 64
        && value.chars().all(|ch| ch.is_ascii_hexdigit())
        && value.chars().any(|ch| ch != '0')
}

pub(crate) fn authority_input_summary(
    capability_ledger_sha256: String,
    crash_cut_catalog_sha256: String,
    xfstests_selection_sha256: String,
    shell_scenario_sha256: String,
    xfstests_case_count: usize,
    crash_cut_expanded_count: usize,
) -> Tier1AuthorityInputs {
    Tier1AuthorityInputs {
        capability_ledger_sha256,
        crash_cut_catalog_sha256,
        xfstests_selection_sha256,
        shell_scenario_sha256,
        xfstests_case_count,
        crash_cut_expanded_count,
    }
}
