use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::Result;

pub(crate) const RV64_REFERENCE_JSON: &str =
    "xtask/data/syscalls/riscv/64/rv64/linux-6.17-table.json";
pub(crate) const RV64_REFERENCE_URL: &str =
    "https://syscalls.mebeim.net/db/riscv/64/rv64/latest/table.json";
pub(crate) const LINUX_REF_SUBMODULE: &str = "external/linux-rv-6.17";

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ReferenceTable {
    pub(crate) kernel: ReferenceKernel,
    pub(crate) syscalls: Vec<ReferenceSyscall>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ReferenceKernel {
    pub(crate) version: String,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ReferenceSyscall {
    pub(crate) file: String,
    pub(crate) line: u64,
    pub(crate) name: String,
    pub(crate) number: u64,
    #[serde(default)]
    pub(crate) signature: Vec<String>,
    pub(crate) symbol: String,
}

#[derive(Clone, Debug)]
pub(crate) struct LocalNr {
    pub(crate) name: String,
    pub(crate) nr: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct NumberMismatch {
    pub(crate) name: String,
    pub(crate) local: u64,
    pub(crate) reference: u64,
    pub(crate) reference_file: String,
    pub(crate) reference_line: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ExtraLocal {
    pub(crate) name: String,
    pub(crate) nr: u64,
}

pub(crate) fn load_reference(root: &Path) -> Result<ReferenceTable> {
    let path = root.join(RV64_REFERENCE_JSON);
    let text = fs::read_to_string(&path).map_err(|e| format!("{RV64_REFERENCE_JSON}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{RV64_REFERENCE_JSON}: {e}"))
}

pub(crate) fn reference_by_local_name(
    table: &ReferenceTable,
) -> BTreeMap<String, &ReferenceSyscall> {
    table
        .syscalls
        .iter()
        .flat_map(|s| {
            let mut names = vec![name_to_local_key(&s.name)];
            names.extend(reference_aliases(&s.name).into_iter().map(str::to_string));
            names.into_iter().map(move |name| (name, s))
        })
        .collect()
}

pub(crate) fn number_mismatches(
    locals: &[LocalNr],
    reference: &ReferenceTable,
) -> Vec<NumberMismatch> {
    let by_name = reference_by_local_name(reference);
    locals
        .iter()
        .filter_map(|local| {
            let expected = by_name.get(&local.name)?;
            (local.nr != expected.number).then(|| NumberMismatch {
                name: local.name.clone(),
                local: local.nr,
                reference: expected.number,
                reference_file: expected.file.clone(),
                reference_line: expected.line,
            })
        })
        .collect()
}

pub(crate) fn extra_locals(locals: &[LocalNr], reference: &ReferenceTable) -> Vec<ExtraLocal> {
    let by_name = reference_by_local_name(reference);
    locals
        .iter()
        .filter(|local| !by_name.contains_key(&canonical_local_key(&local.name)))
        .map(|local| ExtraLocal {
            name: local.name.clone(),
            nr: local.nr,
        })
        .collect()
}

pub(crate) fn true_missing<'a>(
    locals: &[LocalNr],
    reference: &'a ReferenceTable,
) -> Vec<&'a ReferenceSyscall> {
    let local_names: BTreeSet<String> = locals
        .iter()
        .map(|n| canonical_local_key(&n.name))
        .collect();
    reference
        .syscalls
        .iter()
        .filter(|sys| !local_names.contains(&name_to_local_key(&sys.name)))
        .collect()
}

pub(crate) fn name_to_local_key(name: &str) -> String {
    canonical_local_key(&name.to_ascii_uppercase())
}

fn canonical_local_key(name: &str) -> String {
    match name {
        // Linux's RV64 table uses implementation names for these generic ABI
        // slots; tx-shims keeps the public syscall spelling used by callers.
        "FSTAT" => "NEWFSTAT",
        "UNAME" => "NEWUNAME",
        "UMOUNT2" => "UMOUNT",
        other => other,
    }
    .to_string()
}

fn reference_aliases(name: &str) -> Vec<&'static str> {
    match name {
        "newfstat" => vec!["FSTAT"],
        "newuname" => vec!["UNAME"],
        "umount" => vec!["UMOUNT2"],
        _ => Vec::new(),
    }
}
