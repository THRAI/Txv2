use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::util::{optional_option_value, resolve_path};
use crate::Result;

pub(crate) fn observe_schema(root: &Path, args: Vec<String>) -> Result<()> {
    let Some(subcmd) = args.first() else {
        return Err("observe-schema command needs check\n\
             usage:\n\
             \tcargo xtask observe-schema check [--schema schema/txobserve.toml]\n\
             \tcargo xtask observe-schema codegen [--schema schema/txobserve.toml] [--output crates/tx-observe/src/l0_schema/schema_catalog.rs] [--host-output tools/tx-observe-host-catalog.json] [--check]"
            .into());
    };
    match subcmd.as_str() {
        "check" => observe_schema_check(root, &args[1..]),
        "codegen" => observe_schema_codegen(root, &args[1..]),
        other => Err(format!(
            "unknown observe-schema subcommand '{other}'; expected check or codegen"
        )),
    }
}

fn schema_path_from_args(root: &Path, args: &[String]) -> std::path::PathBuf {
    optional_option_value(args, "--schema")
        .map(PathBufLike::from)
        .map(|path| resolve_path(root, path.0))
        .unwrap_or_else(|| root.join("schema/txobserve.toml"))
}

fn read_schema_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|err| format!("failed to read {}: {err}", path.display()))
}

fn observe_schema_check(root: &Path, args: &[String]) -> Result<()> {
    let schema_path = schema_path_from_args(root, args);
    let schema = read_schema_file(&schema_path)?;
    let record_rs_path = root.join("crates/tx-observe-types/src/record.rs");
    let payload_rs_path = root.join("crates/tx-observe-types/src/payload.rs");
    let types_lib_rs_path = root.join("crates/tx-observe-types/src/lib.rs");
    let cargo_toml_path = root.join("Cargo.toml");
    let analyzer_path = root.join("tools/tx-observe-analyze.py");
    let perfetto_writer_path = root.join("tools/tx-trace-daemon/src/l6_views/perfetto/writer.rs");
    let tx_observe_runtime_path = root.join("crates/tx-observe/src/l2_producer/runtime.rs");
    let record_rs = fs::read_to_string(&record_rs_path)
        .map_err(|err| format!("failed to read {}: {err}", record_rs_path.display()))?;
    let payload_rs = fs::read_to_string(&payload_rs_path)
        .map_err(|err| format!("failed to read {}: {err}", payload_rs_path.display()))?;
    let types_lib_rs = fs::read_to_string(&types_lib_rs_path)
        .map_err(|err| format!("failed to read {}: {err}", types_lib_rs_path.display()))?;
    let cargo_toml = fs::read_to_string(&cargo_toml_path)
        .map_err(|err| format!("failed to read {}: {err}", cargo_toml_path.display()))?;
    let analyzer_py = fs::read_to_string(&analyzer_path)
        .map_err(|err| format!("failed to read {}: {err}", analyzer_path.display()))?;
    let perfetto_writer = fs::read_to_string(&perfetto_writer_path)
        .map_err(|err| format!("failed to read {}: {err}", perfetto_writer_path.display()))?;
    let tx_observe_runtime = fs::read_to_string(&tx_observe_runtime_path).map_err(|err| {
        format!(
            "failed to read {}: {err}",
            tx_observe_runtime_path.display()
        )
    })?;
    check_observe_host_layout(root)?;

    let report = check_schema_text(
        &schema,
        SchemaRustSources {
            record_rs: &record_rs,
            payload_rs: &payload_rs,
            types_lib_rs: &types_lib_rs,
        },
        &cargo_toml,
        &analyzer_py,
        &perfetto_writer,
        &tx_observe_runtime,
    )?;
    println!(
        "observe-schema check: ok (levels={} record_kinds={} payloads={} payload_structs={} cfgs={} projections={} tracks={} hart_emitter_methods={})",
        report.levels,
        report.record_kinds,
        report.payloads,
        report.payload_structs,
        report.cfgs,
        report.projections,
        report.tracks,
        report.hart_emitter_methods
    );
    Ok(())
}

fn observe_schema_codegen(root: &Path, args: &[String]) -> Result<()> {
    let schema_path = schema_path_from_args(root, args);
    let schema_text = read_schema_file(&schema_path)?;
    let schema: ObserveSchema =
        toml::from_str(&schema_text).map_err(|err| format!("schema TOML parse failed: {err}"))?;
    let generated = render_schema_catalog(&schema)?;
    let generated_host = render_host_catalog(&schema)?;
    let output_path = optional_option_value(args, "--output")
        .map(PathBufLike::from)
        .map(|path| resolve_path(root, path.0))
        .or_else(|| {
            schema
                .kernel
                .as_ref()
                .and_then(|kernel| kernel.generated_catalog.as_ref())
                .map(|path| resolve_path(root, path.into()))
        })
        .unwrap_or_else(|| root.join("crates/tx-observe/src/l0_schema/schema_catalog.rs"));
    let host_output_path = optional_option_value(args, "--host-output")
        .map(PathBufLike::from)
        .map(|path| resolve_path(root, path.0))
        .unwrap_or_else(|| root.join("tools/tx-observe-host-catalog.json"));

    if args.iter().any(|arg| arg == "--check") {
        let current = read_schema_file(&output_path)?;
        let current_host = read_schema_file(&host_output_path)?;
        if current == generated && current_host == generated_host {
            println!(
                "observe-schema codegen --check: ok ({}, {})",
                output_path.display(),
                host_output_path.display()
            );
            return Ok(());
        }
        if current_host != generated_host {
            return Err(format!(
                "observe-schema generated host catalog is stale: run `cargo xtask observe-schema codegen` to update {}",
                host_output_path.display()
            ));
        }
        return Err(format!(
            "observe-schema generated catalog is stale: run `cargo xtask observe-schema codegen` to update {}",
            output_path.display()
        ));
    }

    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::write(&output_path, generated)
        .map_err(|err| format!("failed to write {}: {err}", output_path.display()))?;
    if let Some(parent) = host_output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create {}: {err}", parent.display()))?;
    }
    fs::write(&host_output_path, generated_host)
        .map_err(|err| format!("failed to write {}: {err}", host_output_path.display()))?;
    println!(
        "observe-schema codegen: wrote {} and {}",
        output_path.display(),
        host_output_path.display()
    );
    Ok(())
}

struct PathBufLike(std::path::PathBuf);

impl From<String> for PathBufLike {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Deserialize)]
struct ObserveSchema {
    abi: AbiSchema,
    payloads: Vec<PayloadEntry>,
    controls: ControlsSchema,
    host: HostSchema,
    tracks: Option<TracksSchema>,
    names: Option<NamesSchema>,
    kernel: Option<KernelSchema>,
    #[serde(default)]
    event_families: Vec<EventFamilyEntry>,
}

#[derive(Debug, Deserialize)]
struct AbiSchema {
    levels: Vec<NamedDiscriminant>,
    record_kinds: Vec<NamedDiscriminant>,
}

#[derive(Debug, Deserialize)]
struct NamedDiscriminant {
    rust: String,
    value: u16,
}

#[derive(Debug, Deserialize)]
struct PayloadEntry {
    id: String,
    rust_tag: String,
    tag: u16,
    #[serde(rename = "struct")]
    #[serde(default)]
    rust_struct: String,
    #[serde(default)]
    size_bytes: u16,
    #[serde(default)]
    fields: Vec<PayloadFieldEntry>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Deserialize)]
struct PayloadFieldEntry {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Debug, Deserialize)]
struct ControlsSchema {
    cfgs: Vec<CfgEntry>,
    #[serde(default)]
    groups: Vec<ControlGroupEntry>,
}

#[derive(Debug, Deserialize)]
struct CfgEntry {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ControlGroupEntry {
    id: String,
    title: Option<String>,
    #[serde(default)]
    default: bool,
    #[serde(default)]
    requires_cfg: Vec<String>,
    #[serde(default)]
    local_cfgs: Vec<String>,
    #[serde(default)]
    events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct HostSchema {
    #[serde(default)]
    inputs: Vec<HostInputEntry>,
    projections: Vec<ProjectionEntry>,
}

#[derive(Debug, Deserialize)]
struct HostInputEntry {
    id: String,
    extension: Option<String>,
    reader_target: String,
}

#[derive(Debug, Deserialize)]
struct ProjectionEntry {
    id: String,
    kind: String,
    file: Option<String>,
    target_type: Option<String>,
    current_source: Option<String>,
    coverage_default: Option<String>,
    #[serde(default)]
    columns: Vec<ColumnEntry>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Deserialize)]
struct ColumnEntry {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Debug, Deserialize)]
struct EventFamilyEntry {
    id: String,
    #[serde(default)]
    levels: Vec<String>,
    #[serde(default)]
    payloads: Vec<String>,
    control_group: Option<String>,
    #[serde(default)]
    projection: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct TracksSchema {
    #[serde(default)]
    explicit: Vec<ExplicitTrackEntry>,
}

#[derive(Debug, Deserialize)]
struct ExplicitTrackEntry {
    #[allow(dead_code)]
    id: String,
    #[serde(rename = "const")]
    const_name: String,
    track_id_hex: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct NamesSchema {
    #[serde(default)]
    families: Vec<NameFamilyEntry>,
}

#[derive(Debug, Deserialize)]
struct NameFamilyEntry {
    pattern: String,
}

#[derive(Debug, Deserialize)]
struct KernelSchema {
    generated_catalog: Option<String>,
    #[serde(default)]
    hart_emitter_methods: Vec<HartEmitterMethodEntry>,
    #[serde(default)]
    producer_boundary_rules: Vec<ProducerBoundaryRuleEntry>,
}

#[derive(Debug, Deserialize)]
struct HartEmitterMethodEntry {
    name: String,
    surface: String,
    record_kind: String,
    #[serde(default)]
    levels: Vec<String>,
    #[serde(default)]
    payloads: Vec<String>,
    event_family: Option<String>,
    producer_allowed: bool,
}

#[derive(Debug, Deserialize)]
struct ProducerBoundaryRuleEntry {
    name: String,
    replacement: String,
    #[serde(default)]
    needles: Vec<String>,
}

#[derive(Debug, Eq, PartialEq)]
struct CheckReport {
    levels: usize,
    record_kinds: usize,
    payloads: usize,
    payload_structs: usize,
    cfgs: usize,
    projections: usize,
    tracks: usize,
    hart_emitter_methods: usize,
}

struct SchemaRustSources<'a> {
    record_rs: &'a str,
    payload_rs: &'a str,
    types_lib_rs: &'a str,
}

fn check_schema_text(
    schema: &str,
    rust_sources: SchemaRustSources<'_>,
    cargo_toml: &str,
    analyzer_py: &str,
    perfetto_writer: &str,
    tx_observe_lib_rs: &str,
) -> Result<CheckReport> {
    let schema: ObserveSchema =
        toml::from_str(schema).map_err(|err| format!("schema TOML parse failed: {err}"))?;

    let level_schema = discriminants_from_schema(
        schema
            .abi
            .levels
            .iter()
            .map(|entry| (&entry.rust, entry.value)),
        "TxTraceLevel",
    )?;
    let kind_schema = discriminants_from_schema(
        schema
            .abi
            .record_kinds
            .iter()
            .map(|entry| (&entry.rust, entry.value)),
        "TxTraceKind",
    )?;
    let payload_schema = discriminants_from_schema(
        schema
            .payloads
            .iter()
            .map(|entry| (&entry.rust_tag, entry.tag)),
        "TxPayloadTag",
    )?;
    let cfg_schema: BTreeSet<String> = schema
        .controls
        .cfgs
        .iter()
        .map(|entry| entry.name.clone())
        .collect();

    let levels = parse_rust_enum_discriminants(rust_sources.record_rs, "TxTraceLevel")?;
    let record_kinds = parse_rust_enum_discriminants(rust_sources.record_rs, "TxTraceKind")?;
    let payloads = parse_rust_enum_discriminants(rust_sources.payload_rs, "TxPayloadTag")?;
    let payload_structs = parse_payload_struct_fields(rust_sources.payload_rs)?;
    let payload_sizes = parse_payload_size_assertions(rust_sources.types_lib_rs)?;
    let cfgs = parse_workspace_cfgs(cargo_toml)?;
    let schema_projections = projection_schemas_from_schema(&schema.host.projections)?;
    let track_consts = parse_explicit_track_consts(rust_sources.payload_rs)?;
    let writer_track_names = parse_explicit_track_names(perfetto_writer)?;
    let hart_emitter_methods = parse_hart_emitter_public_methods(tx_observe_lib_rs)?;
    check_event_family_refs(&schema)?;
    check_analyzer_host_boundary(analyzer_py)?;
    check_explicit_tracks(&schema, &track_consts, &writer_track_names)?;
    check_hart_emitter_methods(&schema, &hart_emitter_methods)?;

    compare_maps("TxTraceLevel", &levels, &level_schema)?;
    compare_maps("TxTraceKind", &record_kinds, &kind_schema)?;
    compare_maps("TxPayloadTag", &payloads, &payload_schema)?;
    compare_payload_structs(&schema.payloads, &payload_structs, &payload_sizes)?;
    compare_sets("cfg", &cfgs, &cfg_schema)?;

    Ok(CheckReport {
        levels: level_schema.len(),
        record_kinds: kind_schema.len(),
        payloads: payload_schema.len(),
        payload_structs: schema
            .payloads
            .iter()
            .filter(|payload| requires_payload_struct(payload))
            .count(),
        cfgs: cfg_schema.len(),
        projections: schema_projections.len(),
        tracks: schema
            .tracks
            .as_ref()
            .map(|tracks| tracks.explicit.len())
            .unwrap_or(0),
        hart_emitter_methods: schema
            .kernel
            .as_ref()
            .map(|kernel| kernel.hart_emitter_methods.len())
            .unwrap_or(0),
    })
}

fn discriminants_from_schema<'a>(
    entries: impl Iterator<Item = (&'a String, u16)>,
    enum_name: &str,
) -> Result<BTreeMap<String, u16>> {
    let prefix = format!("{enum_name}::");
    let mut out = BTreeMap::new();
    for (rust, value) in entries {
        let Some(name) = rust.strip_prefix(&prefix) else {
            return Err(format!(
                "schema entry '{rust}' does not use {prefix}<Variant>"
            ));
        };
        if out.insert(name.to_string(), value).is_some() {
            return Err(format!("duplicate schema entry {rust}"));
        }
    }
    Ok(out)
}

fn parse_rust_enum_discriminants(text: &str, enum_name: &str) -> Result<BTreeMap<String, u16>> {
    let body = rust_enum_body(text, enum_name)?;
    let mut out = BTreeMap::new();
    for raw_line in body.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with("#[") {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim().trim_end_matches(',');
        let value_text = value.trim().trim_end_matches(',');
        let value = value_text.parse::<u16>().map_err(|err| {
            format!("{enum_name}::{name} has non-u16 discriminant '{value_text}': {err}")
        })?;
        if out.insert(name.to_string(), value).is_some() {
            return Err(format!("duplicate Rust enum entry {enum_name}::{name}"));
        }
    }
    Ok(out)
}

fn parse_payload_struct_fields(text: &str) -> Result<BTreeMap<String, Vec<PayloadFieldEntry>>> {
    let mut out = BTreeMap::new();
    let mut rest = text;
    while let Some(idx) = rest.find("pub struct Payload") {
        rest = &rest[idx + "pub struct ".len()..];
        let name_end = rest
            .find([' ', '{'])
            .ok_or("unterminated payload struct name")?;
        let name = rest[..name_end].to_string();
        let open = rest[name_end..]
            .find('{')
            .map(|offset| name_end + offset)
            .ok_or_else(|| format!("missing '{{' for {name}"))?;
        let close = matching_delimiter(rest, open, '{', '}')?;
        let body = &rest[open + 1..close];
        let mut fields = Vec::new();
        for raw_line in body.lines() {
            let line = raw_line.split("//").next().unwrap_or("").trim();
            if !line.starts_with("pub ") {
                continue;
            }
            let field = line.trim_start_matches("pub ").trim_end_matches(',');
            let Some((field_name, field_ty)) = field.split_once(':') else {
                return Err(format!("{name} field line is not `pub name: type`: {line}"));
            };
            fields.push(PayloadFieldEntry {
                name: field_name.trim().to_string(),
                ty: field_ty.trim().to_string(),
            });
        }
        if out.insert(name.clone(), fields).is_some() {
            return Err(format!("duplicate payload struct {name}"));
        }
        rest = &rest[close + 1..];
    }
    Ok(out)
}

fn parse_payload_size_assertions(text: &str) -> Result<BTreeMap<String, u16>> {
    let mut out = BTreeMap::new();
    let mut rest = text;
    while let Some(idx) = rest.find("assert!(size_of::<Payload") {
        rest = &rest[idx + "assert!(size_of::<".len()..];
        let Some(name_end) = rest.find(">() ==") else {
            let Some(next) = rest.find(';') else {
                return Err("malformed payload size assertion".into());
            };
            rest = &rest[next + 1..];
            continue;
        };
        let name = rest[..name_end].to_string();
        rest = &rest[name_end + ">() ==".len()..];
        let value_text: String = rest
            .trim_start()
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        let value = value_text
            .parse::<u16>()
            .map_err(|err| format!("{name} size assertion is not u16: {err}"))?;
        out.entry(name).or_insert(value);
    }
    Ok(out)
}

fn parse_explicit_track_consts(text: &str) -> Result<BTreeMap<String, String>> {
    if !text.contains("ALLOC_TRACK_") {
        return Ok(BTreeMap::new());
    }
    let prefix = parse_hex_const(text, "EXPLICIT_TRACK_ID_PREFIX")?;
    let mut out = BTreeMap::new();
    for raw_line in text.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        if !line.starts_with("pub const ALLOC_TRACK_") {
            continue;
        }
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        let const_name = left
            .trim_start_matches("pub const ")
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let Some(offset_text) = right.split('|').nth(1) else {
            return Err(format!(
                "{const_name} does not use EXPLICIT_TRACK_ID_PREFIX | offset"
            ));
        };
        let offset = parse_hex_u64(offset_text.trim().trim_end_matches(';'))?;
        out.insert(const_name, format!("0x{:016x}", prefix | offset));
    }
    Ok(out)
}

fn parse_hex_const(text: &str, const_name: &str) -> Result<u64> {
    for raw_line in text.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        if !line.starts_with(&format!("pub const {const_name}:")) {
            continue;
        }
        let Some((_, value)) = line.split_once('=') else {
            break;
        };
        return parse_hex_u64(value.trim().trim_end_matches(';'));
    }
    Err(format!("missing const {const_name}"))
}

fn parse_hex_u64(value: &str) -> Result<u64> {
    let cleaned = value.trim().trim_start_matches("0x").replace('_', "");
    u64::from_str_radix(&cleaned, 16).map_err(|err| format!("invalid hex '{value}': {err}"))
}

fn parse_explicit_track_names(text: &str) -> Result<BTreeMap<String, String>> {
    let body = rust_function_body(text, "explicit_track_descriptor")?;
    let mut out = BTreeMap::new();
    for raw_line in body.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        if !line.starts_with("ALLOC_TRACK_") {
            continue;
        }
        let Some((const_name, value)) = line.split_once("=>") else {
            continue;
        };
        let Some(first_quote) = value.find('"') else {
            continue;
        };
        let rest = &value[first_quote + 1..];
        let Some(end_quote) = rest.find('"') else {
            return Err(format!("unterminated track name for {}", const_name.trim()));
        };
        out.insert(const_name.trim().to_string(), rest[..end_quote].to_string());
    }
    Ok(out)
}

fn rust_function_body<'a>(text: &'a str, fn_name: &str) -> Result<&'a str> {
    let needle = format!("fn {fn_name}");
    let Some(start) = text.find(&needle) else {
        return Err(format!("missing Rust function {fn_name}"));
    };
    let after = &text[start..];
    let Some(open_rel) = after.find('{') else {
        return Err(format!("missing '{{' for Rust function {fn_name}"));
    };
    let body_start = start + open_rel + 1;
    let mut depth = 1usize;
    for (offset, ch) in text[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&text[body_start..body_start + offset]);
                }
            }
            _ => {}
        }
    }
    Err(format!("missing closing '}}' for Rust function {fn_name}"))
}

fn check_explicit_tracks(
    schema: &ObserveSchema,
    track_consts: &BTreeMap<String, String>,
    writer_names: &BTreeMap<String, String>,
) -> Result<()> {
    let schema_tracks = schema
        .tracks
        .as_ref()
        .map(|tracks| tracks.explicit.as_slice())
        .unwrap_or(&[]);
    let schema_by_const: BTreeMap<_, _> = schema_tracks
        .iter()
        .map(|track| (track.const_name.clone(), track))
        .collect();
    let schema_keys: BTreeSet<_> = schema_by_const.keys().cloned().collect();
    let const_keys: BTreeSet<_> = track_consts.keys().cloned().collect();
    let writer_keys: BTreeSet<_> = writer_names.keys().cloned().collect();
    let mut mismatches = Vec::new();
    for missing in const_keys.difference(&schema_keys) {
        mismatches.push(format!("missing schema track {missing}"));
    }
    for extra in schema_keys.difference(&const_keys) {
        mismatches.push(format!("extra schema track {extra}"));
    }
    for missing in const_keys.difference(&writer_keys) {
        mismatches.push(format!("missing Perfetto writer track {missing}"));
    }
    for extra in writer_keys.difference(&const_keys) {
        mismatches.push(format!("extra Perfetto writer track {extra}"));
    }
    for const_name in const_keys.intersection(&schema_keys) {
        let schema_track = schema_by_const[const_name];
        let rust_track_id = &track_consts[const_name];
        if &schema_track.track_id_hex.to_ascii_lowercase() != rust_track_id {
            mismatches.push(format!(
                "{const_name} track_id: rust={rust_track_id} schema={}",
                schema_track.track_id_hex
            ));
        }
        if let Some(writer_name) = writer_names.get(const_name) {
            if &schema_track.name != writer_name {
                mismatches.push(format!(
                    "{const_name} name: writer={writer_name} schema={}",
                    schema_track.name
                ));
            }
        }
    }

    let name_patterns: Vec<_> = schema
        .names
        .as_ref()
        .map(|names| {
            names
                .families
                .iter()
                .map(|family| family.pattern.as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for track in schema_tracks {
        if !name_patterns
            .iter()
            .any(|pattern| name_matches_pattern(&track.name, pattern))
        {
            mismatches.push(format!(
                "{} name '{}' is not covered by names.families",
                track.const_name, track.name
            ));
        }
    }

    if mismatches.is_empty() {
        return Ok(());
    }
    Err(format!("explicit track mismatch: {mismatches:?}"))
}

fn parse_hart_emitter_public_methods(text: &str) -> Result<BTreeSet<String>> {
    let needle = "impl HartEmitter";
    let Some(start) = text.find(needle) else {
        return Err("missing impl HartEmitter".into());
    };
    let after = &text[start..];
    let Some(open_rel) = after.find('{') else {
        return Err("missing '{' for impl HartEmitter".into());
    };
    let open = start + open_rel;
    let close = matching_delimiter(text, open, '{', '}')?;
    let body = &text[open + 1..close];
    let mut methods = BTreeSet::new();
    for raw_line in body.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        let Some(rest) = line.strip_prefix("pub fn ") else {
            continue;
        };
        let Some(name_end) = rest.find('(') else {
            return Err(format!(
                "HartEmitter method line missing '(' after name: {line}"
            ));
        };
        let name = rest[..name_end].trim();
        if name.is_empty() {
            return Err(format!("HartEmitter method line has empty name: {line}"));
        }
        methods.insert(name.to_string());
    }
    Ok(methods)
}

fn check_hart_emitter_methods(
    schema: &ObserveSchema,
    rust_methods: &BTreeSet<String>,
) -> Result<()> {
    let Some(kernel) = &schema.kernel else {
        return Ok(());
    };
    let schema_methods: BTreeSet<_> = kernel
        .hart_emitter_methods
        .iter()
        .map(|method| method.name.clone())
        .collect();
    let missing: Vec<_> = rust_methods.difference(&schema_methods).cloned().collect();
    let extra: Vec<_> = schema_methods.difference(rust_methods).cloned().collect();
    let mut mismatches = Vec::new();
    if !missing.is_empty() || !extra.is_empty() {
        mismatches.push(format!("missing={missing:?} extra={extra:?}"));
    }

    let level_ids: BTreeSet<_> = schema
        .abi
        .levels
        .iter()
        .filter_map(|level| level.rust.strip_prefix("TxTraceLevel::"))
        .map(camel_to_snake)
        .collect();
    let payload_ids: BTreeSet<_> = schema
        .payloads
        .iter()
        .map(|payload| payload.id.as_str())
        .collect();
    let record_kind_ids: BTreeSet<_> = schema
        .abi
        .record_kinds
        .iter()
        .filter_map(|kind| kind.rust.strip_prefix("TxTraceKind::"))
        .map(camel_to_snake)
        .collect();
    let event_family_ids: BTreeSet<_> = schema
        .event_families
        .iter()
        .map(|family| family.id.as_str())
        .collect();

    for method in &kernel.hart_emitter_methods {
        if method.surface != "raw_facade" && method.surface != "typed_producer" {
            mismatches.push(format!(
                "{} surface '{}' is not raw_facade or typed_producer",
                method.name, method.surface
            ));
        }
        if !record_kind_ids.contains(method.record_kind.as_str()) {
            mismatches.push(format!(
                "{} record_kind -> {}",
                method.name, method.record_kind
            ));
        }
        for level in &method.levels {
            if !level_ids.contains(level.as_str()) {
                mismatches.push(format!("{} level -> {}", method.name, level));
            }
        }
        for payload in &method.payloads {
            if !payload_ids.contains(payload.as_str()) {
                mismatches.push(format!("{} payload -> {}", method.name, payload));
            }
        }
        if let Some(event_family) = &method.event_family {
            if !event_family_ids.contains(event_family.as_str()) {
                mismatches.push(format!("{} event_family -> {}", method.name, event_family));
            }
        }
        if method.surface == "raw_facade" && method.producer_allowed {
            mismatches.push(format!(
                "{} raw_facade method cannot be producer_allowed",
                method.name
            ));
        }
    }

    if mismatches.is_empty() {
        return Ok(());
    }
    Err(format!("HartEmitter method mismatch: {mismatches:?}"))
}

fn render_schema_catalog(schema: &ObserveSchema) -> Result<String> {
    let Some(kernel) = &schema.kernel else {
        return Err("schema has no [kernel] catalog section".into());
    };
    let mut out = String::new();
    out.push_str("// @generated by `cargo xtask observe-schema codegen`; do not edit by hand.\n");
    out.push_str("// Source of truth: schema/txobserve.toml\n\n");
    out.push_str("#[derive(Copy, Clone, Debug, Eq, PartialEq)]\n");
    out.push_str("pub struct HartEmitterMethodCatalog {\n");
    out.push_str("    pub name: &'static str,\n");
    out.push_str("    pub surface: &'static str,\n");
    out.push_str("    pub record_kind: &'static str,\n");
    out.push_str("    pub levels: &'static [&'static str],\n");
    out.push_str("    pub payloads: &'static [&'static str],\n");
    out.push_str("    pub event_family: Option<&'static str>,\n");
    out.push_str("    pub producer_allowed: bool,\n");
    out.push_str("}\n\n");
    out.push_str("pub const HART_EMITTER_METHODS: &[HartEmitterMethodCatalog] = &[\n");
    for method in &kernel.hart_emitter_methods {
        out.push_str("    HartEmitterMethodCatalog {\n");
        out.push_str(&format!(
            "        name: \"{}\",\n",
            rust_escape(&method.name)
        ));
        out.push_str(&format!(
            "        surface: \"{}\",\n",
            rust_escape(&method.surface)
        ));
        out.push_str(&format!(
            "        record_kind: \"{}\",\n",
            rust_escape(&method.record_kind)
        ));
        out.push_str(&format!(
            "        levels: {},\n",
            render_string_slice(&method.levels)
        ));
        out.push_str(&format!(
            "        payloads: {},\n",
            render_string_slice(&method.payloads)
        ));
        let event_family = method
            .event_family
            .as_ref()
            .map(|value| format!("Some(\"{}\")", rust_escape(value)))
            .unwrap_or_else(|| "None".to_string());
        out.push_str(&format!("        event_family: {event_family},\n"));
        out.push_str(&format!(
            "        producer_allowed: {},\n",
            method.producer_allowed
        ));
        out.push_str("    },\n");
    }
    out.push_str("];\n\n");

    out.push_str("#[derive(Copy, Clone, Debug, Eq, PartialEq)]\n");
    out.push_str("pub struct ProducerBoundaryRuleCatalog {\n");
    out.push_str("    pub name: &'static str,\n");
    out.push_str("    pub replacement: &'static str,\n");
    out.push_str("    pub needles: &'static [&'static str],\n");
    out.push_str("}\n\n");
    out.push_str("pub const PRODUCER_BOUNDARY_RULES: &[ProducerBoundaryRuleCatalog] = &[\n");
    for rule in &kernel.producer_boundary_rules {
        out.push_str("    ProducerBoundaryRuleCatalog {\n");
        out.push_str(&format!("        name: \"{}\",\n", rust_escape(&rule.name)));
        out.push_str(&format!(
            "        replacement: \"{}\",\n",
            rust_escape(&rule.replacement)
        ));
        out.push_str(&format!(
            "        needles: {},\n",
            render_string_slice(&rule.needles)
        ));
        out.push_str("    },\n");
    }
    out.push_str("];\n");
    Ok(out)
}

fn render_host_catalog(schema: &ObserveSchema) -> Result<String> {
    let cfgs = schema
        .controls
        .cfgs
        .iter()
        .map(|cfg| serde_json::json!({ "name": cfg.name }))
        .collect::<Vec<_>>();
    let control_groups = schema
        .controls
        .groups
        .iter()
        .map(|group| {
            serde_json::json!({
                "id": group.id,
                "title": group.title,
                "default": group.default,
                "requires_cfg": group.requires_cfg,
                "local_cfgs": group.local_cfgs,
                "events": group.events,
            })
        })
        .collect::<Vec<_>>();
    let inputs = schema
        .host
        .inputs
        .iter()
        .map(|input| {
            serde_json::json!({
                "id": input.id,
                "extension": input.extension,
                "reader_target": input.reader_target,
            })
        })
        .collect::<Vec<_>>();
    let projections = schema
        .host
        .projections
        .iter()
        .map(|projection| {
            let columns = projection
                .columns
                .iter()
                .map(|column| {
                    serde_json::json!({
                        "name": column.name,
                        "type": column.ty,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "id": projection.id,
                "kind": projection.kind,
                "file": projection.file,
                "target_type": projection.target_type,
                "current_source": projection.current_source,
                "coverage_default": projection.coverage_default,
                "columns": columns,
            })
        })
        .collect::<Vec<_>>();
    let event_families = schema
        .event_families
        .iter()
        .map(|family| {
            serde_json::json!({
                "id": family.id,
                "levels": family.levels,
                "payloads": family.payloads,
                "control_group": family.control_group,
                "projection": family.projection,
            })
        })
        .collect::<Vec<_>>();
    let value = serde_json::json!({
        "schema": "tx-observe-host-catalog-v0",
        "source": "schema/txobserve.toml",
        "cfgs": cfgs,
        "control_groups": control_groups,
        "inputs": inputs,
        "projections": projections,
        "event_families": event_families,
    });
    serde_json::to_string_pretty(&value)
        .map(|mut text| {
            text.push('\n');
            text
        })
        .map_err(|err| format!("failed to render host catalog JSON: {err}"))
}

fn render_string_slice(values: &[String]) -> String {
    if values.is_empty() {
        return "&[]".to_string();
    }
    let inline_items = values
        .iter()
        .map(|value| format!("\"{}\"", rust_escape(value)))
        .collect::<Vec<_>>()
        .join(", ");
    let inline = format!("&[{inline_items}]");
    if inline.len() <= 72 {
        return inline;
    }
    if values.iter().all(|value| value.len() <= 16) {
        return format!("&[\n            {inline_items},\n        ]");
    }
    let mut out = String::from("&[\n");
    for value in values {
        out.push_str(&format!("            \"{}\",\n", rust_escape(value)));
    }
    out.push_str("        ]");
    out
}

fn rust_escape(value: &str) -> String {
    value.escape_default().collect()
}

fn name_matches_pattern(name: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix(".*") {
        name.starts_with(&format!("{prefix}."))
    } else {
        name == pattern
    }
}

fn compare_payload_structs(
    schema_payloads: &[PayloadEntry],
    rust_structs: &BTreeMap<String, Vec<PayloadFieldEntry>>,
    rust_sizes: &BTreeMap<String, u16>,
) -> Result<()> {
    let mut mismatches = Vec::new();
    let schema_struct_names: BTreeSet<_> = schema_payloads
        .iter()
        .filter(|payload| requires_payload_struct(payload))
        .map(|payload| payload.rust_struct.as_str())
        .collect();

    for payload in schema_payloads {
        if !requires_payload_struct(payload) {
            if payload.size_bytes != 0 && payload.rust_struct == "None" {
                mismatches.push(format!(
                    "{} uses struct=None but size_bytes={}",
                    payload.id, payload.size_bytes
                ));
            }
            continue;
        }

        match rust_structs.get(&payload.rust_struct) {
            Some(fields) if fields == &payload.fields => {}
            Some(fields) => mismatches.push(format!(
                "{} fields: rust={fields:?} schema={:?}",
                payload.rust_struct, payload.fields
            )),
            None => mismatches.push(format!("missing Rust struct {}", payload.rust_struct)),
        }

        match rust_sizes.get(&payload.rust_struct) {
            Some(size) if *size == payload.size_bytes => {}
            Some(size) => mismatches.push(format!(
                "{} size: rust={} schema={}",
                payload.rust_struct, size, payload.size_bytes
            )),
            None => mismatches.push(format!(
                "missing size assertion for {}",
                payload.rust_struct
            )),
        }
    }

    let rust_payload_struct_names: BTreeSet<_> = rust_structs
        .keys()
        .filter(|name| name.starts_with("Payload"))
        .map(|name| name.as_str())
        .collect();
    for extra in rust_payload_struct_names.difference(&schema_struct_names) {
        mismatches.push(format!("Rust payload struct not in schema: {extra}"));
    }

    if mismatches.is_empty() {
        return Ok(());
    }
    Err(format!("payload struct mismatch: {mismatches:?}"))
}

fn requires_payload_struct(payload: &PayloadEntry) -> bool {
    !payload.rust_struct.is_empty()
        && payload.rust_struct != "None"
        && payload.rust_struct != "reserved"
}

fn rust_enum_body<'a>(text: &'a str, enum_name: &str) -> Result<&'a str> {
    let needle = format!("pub enum {enum_name}");
    let Some(start) = text.find(&needle) else {
        return Err(format!("missing Rust enum {enum_name}"));
    };
    let after = &text[start..];
    let Some(open_rel) = after.find('{') else {
        return Err(format!("missing '{{' for Rust enum {enum_name}"));
    };
    let body_start = start + open_rel + 1;
    let mut depth = 1usize;
    for (offset, ch) in text[body_start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(&text[body_start..body_start + offset]);
                }
            }
            _ => {}
        }
    }
    Err(format!("missing closing '}}' for Rust enum {enum_name}"))
}

fn parse_workspace_cfgs(text: &str) -> Result<BTreeSet<String>> {
    let mut cfgs = BTreeSet::new();
    let mut rest = text;
    while let Some(idx) = rest.find("cfg(") {
        rest = &rest[idx + "cfg(".len()..];
        let Some(end) = rest.find(')') else {
            return Err("unterminated cfg(...) in Cargo.toml".into());
        };
        cfgs.insert(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    Ok(cfgs)
}

fn projection_schemas_from_schema(
    projections: &[ProjectionEntry],
) -> Result<BTreeMap<String, Vec<ColumnEntry>>> {
    let mut out = BTreeMap::new();
    for projection in projections {
        if projection.columns.is_empty() {
            continue;
        }
        let key = match projection.kind.as_str() {
            "derived_table" => projection.file.clone().ok_or_else(|| {
                format!(
                    "projection '{}' is derived_table but has no file",
                    projection.id
                )
            })?,
            "sql_view" => projection.id.clone(),
            _ => continue,
        };
        if out
            .insert(key.clone(), projection.columns.clone())
            .is_some()
        {
            return Err(format!("duplicate projection schema entry '{key}'"));
        }
    }
    Ok(out)
}

fn check_event_family_refs(schema: &ObserveSchema) -> Result<()> {
    let level_ids: BTreeSet<_> = schema
        .abi
        .levels
        .iter()
        .filter_map(|level| level.rust.strip_prefix("TxTraceLevel::"))
        .map(camel_to_snake)
        .collect();
    let payload_ids: BTreeSet<_> = schema
        .payloads
        .iter()
        .map(|payload| payload.id.as_str())
        .collect();
    let control_group_ids: BTreeSet<_> = schema
        .controls
        .groups
        .iter()
        .map(|group| group.id.as_str())
        .chain([
            "always_on_when_observe_enabled",
            "always_on_when_callsite_enabled",
        ])
        .collect();
    let projection_ids: BTreeSet<_> = schema
        .host
        .projections
        .iter()
        .map(|projection| projection.id.as_str())
        .collect();
    let mut missing = Vec::new();
    for family in &schema.event_families {
        for level in &family.levels {
            if !level_ids.contains(level.as_str()) {
                missing.push(format!("{} level -> {}", family.id, level));
            }
        }
        for payload in &family.payloads {
            if !payload_ids.contains(payload.as_str()) {
                missing.push(format!("{} payload -> {}", family.id, payload));
            }
        }
        if let Some(control_group) = &family.control_group {
            if !control_group_ids.contains(control_group.as_str()) {
                missing.push(format!("{} control_group -> {}", family.id, control_group));
            }
        }
        for projection in &family.projection {
            if !projection_ids.contains(projection.as_str()) {
                missing.push(format!("{} projection -> {}", family.id, projection));
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "event family reference mismatch: missing={missing:?}"
    ))
}

fn camel_to_snake(value: &str) -> String {
    let mut out = String::new();
    for (idx, ch) in value.chars().enumerate() {
        if ch.is_ascii_uppercase() {
            if idx > 0 {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn check_analyzer_host_boundary(text: &str) -> Result<()> {
    if !text.contains("ANALYZER_DECODER_VERSION") {
        return Ok(());
    }
    let forbidden = [
        "PARQUET_SCHEMAS",
        "RECORD_SQL_SCHEMA",
        "REPAIR_SQL_SCHEMA",
        "NAME_SQL_SCHEMA",
        "local_projection_schemas",
        "_PROJECTION_SCHEMA_CATALOG",
        "def load_host_catalog(",
        "def validate_host_catalog(",
        "def projection_schema_catalog(",
        "def parquet_schema_catalog(",
        "def parquet_select_sql(",
        "def typed_select_sql(",
    ];
    let present: Vec<_> = forbidden
        .iter()
        .copied()
        .filter(|needle| text.contains(needle))
        .collect();
    if !present.is_empty() {
        return Err(format!(
            "analyzer host boundary mismatch: local schema constants must not be defined: {present:?}"
        ));
    }
    let forbidden_raw_readers = [
        "import mmap",
        "import struct",
        "RECORD_STRUCT =",
        "def _u32(",
        "def _u64(",
        "def decode_payload_bytes(",
        "def decode_record_bytes(",
        "def load_txtrace_stream(",
        "def load_rawrecords_stream(",
    ];
    let present_raw_readers: Vec<_> = forbidden_raw_readers
        .iter()
        .copied()
        .filter(|needle| text.contains(needle))
        .collect();
    if !present_raw_readers.is_empty() {
        return Err(format!(
            "analyzer host boundary mismatch: raw reader/decode ownership must stay under tools/tx_observe_host: {present_raw_readers:?}"
        ));
    }
    let has_local_boundary_types = text.contains("class TraceIntegrity:")
        && text.contains("class TraceEventStream:")
        && text.contains("class ProjectionInput:");
    let has_host_package_facade = text.contains("from tx_observe_host import");
    if !has_local_boundary_types && !has_host_package_facade {
        return Err(
            "analyzer host boundary mismatch: boundary types must be local test fixtures or imported from tx_observe_host"
                .into(),
        );
    }
    let forbidden_l6_execution = [
        "ANALYZER_DECODER_VERSION =",
        "PARQUET_MANIFEST =",
        "LOCK_TRACK_ID =",
        "DS_METHOD_TRACK_ID =",
        "ALLOC_TRACK_NAMES =",
        "class DerivedTables:",
        "class DerivedCacheResult:",
        "def fnv1a32(",
        "def fmt_ns(",
        "def event_name(",
        "def span_display_name(",
        "def describe(",
        "def build_derived_tables(",
        "def file_sha256(",
        "def derived_cache_path(",
        "def derived_tables_to_json(",
        "def derived_tables_from_json(",
        "def sql_records_rows(",
        "def sql_repairs_rows(",
        "def sql_names_rows(",
        "def write_jsonl(",
        "def require_duckdb(",
        "def parquet_manifest_path(",
        "def parquet_manifest_matches(",
        "def write_parquet_manifest(",
        "def export_derived_tables_parquet(",
        "def run_sql_query(",
        "def run_sql_projection(",
        "def analyze_parquet_summary(",
        "def run_python_file(",
        "def run_python_projection(",
        "def export_projection_parquet(",
        "def run_python_file_with_table_dir(",
        "def load_or_build_derived_tables(",
        "def load_derived_tables_cache(",
        "def fmt_counter_value(",
        "def analyze(",
        "def analyze_projection(",
        "def percentile(",
        "def roundtrip_points(",
        "def analyze_roundtrip(",
        "CLONE_THREAD_PHASES =",
        "CHILD_SUBMIT_PHASES =",
        "TASK_SUBMIT_PHASES =",
        "THREAD_EXIT_PHASES =",
        "def analyze_clone_thread_phases(",
        "def analyze_counter_phase_sequence(",
        "ARG_NAMES =",
        "FUTEX_OPS =",
        "QUEUE_NAMES =",
        "STOP_REASON_NAMES =",
        "MAILBOX_HINT_NAMES =",
        "WAKE_HINT_NAMES =",
        "def decode_task_code(",
        "def decode_task_duration_us(",
        "def counter_rows(",
        "def analyze_futex_ops(",
        "def allocation_rows(",
        "def analyze_allocation_tracks(",
        "def analyze_lock_metrics(",
        "def analyze_lock_service_counters(",
        "def analyze_ds_method_metrics(",
        "def analyze_sched_counters(",
        "def analyze_wake_hint_counters(",
        "VM_POLL_ATTR_PHASE_PAIRS =",
        "VM_WAIT_MARKERS =",
        "def analyze_vm_poll_attribution(",
        "def analyze_futex_table_counters(",
        "def analyze_wait_source_notify(",
        "def analyze_futex_source_correlation(",
        "def analyze_futex_wake_latency(",
    ];
    let present_l6_execution: Vec<_> = forbidden_l6_execution
        .iter()
        .copied()
        .filter(|needle| text.contains(needle))
        .collect();
    if !present_l6_execution.is_empty() {
        return Err(format!(
            "analyzer host boundary mismatch: L6 table/projection execution must stay under tools/tx_observe_host/l6_views: {present_l6_execution:?}"
        ));
    }
    let required = [
        (
            "TraceIntegrity",
            &["class TraceIntegrity:", "TraceIntegrity,"][..],
        ),
        (
            "TraceStreamMarker",
            &["class TraceStreamMarker:", "TraceStreamMarker,"][..],
        ),
        (
            "TraceEventStream",
            &["class TraceEventStream:", "TraceEventStream,"][..],
        ),
        (
            "ProjectionInput",
            &["class ProjectionInput:", "ProjectionInput,"][..],
        ),
        (
            "analyze_projection",
            &["def analyze_projection(", "analyze_projection,"][..],
        ),
        (
            "run_sql_projection",
            &["def run_sql_projection(", "run_sql_projection,"][..],
        ),
        (
            "export_projection_parquet",
            &[
                "def export_projection_parquet(",
                "export_projection_parquet,",
            ][..],
        ),
        (
            "run_python_projection",
            &["def run_python_projection(", "run_python_projection,"][..],
        ),
    ];
    let missing: Vec<_> = required
        .iter()
        .filter_map(|(label, alternatives)| {
            (!alternatives.iter().any(|needle| text.contains(needle))).then_some(*label)
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "analyzer host boundary mismatch: missing={missing:?}"
    ))
}

fn check_observe_host_layout(root: &Path) -> Result<()> {
    let required_dirs = [
        "crates/tx-observe/src/l0_schema",
        "crates/tx-observe/src/l1_probe_api",
        "crates/tx-observe/src/l2_producer",
        "crates/tx-observe/src/l3_wire",
        "tools/tx-trace-daemon/src/l4_readers",
        "tools/tx-trace-daemon/src/l5_canonical",
        "tools/tx-trace-daemon/src/l6_views",
        "tools/tx_observe_host/l4_readers",
        "tools/tx_observe_host/l5_canonical",
        "tools/tx_observe_host/l6_views",
    ];
    let missing: Vec<_> = required_dirs
        .iter()
        .copied()
        .filter(|rel| !root.join(rel).is_dir())
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "observe L0-L6 layout mismatch: missing layer directories {missing:?}"
        ));
    }

    let forbidden_flat_python_layers = [
        "tools/tx_observe_host/l4_readers.py",
        "tools/tx_observe_host/l5_canonical.py",
        "tools/tx_observe_host/l6_views.py",
    ];
    let present: Vec<_> = forbidden_flat_python_layers
        .iter()
        .copied()
        .filter(|rel| root.join(rel).exists())
        .collect();
    if !present.is_empty() {
        return Err(format!(
            "observe L0-L6 layout mismatch: Python host layers must be package directories, not flat files: {present:?}"
        ));
    }

    Ok(())
}

fn matching_delimiter(text: &str, open_index: usize, open: char, close: char) -> Result<usize> {
    let mut depth = 0usize;
    for (offset, ch) in text[open_index..].char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Ok(open_index + offset);
            }
        }
    }
    Err(format!("missing closing '{close}'"))
}

fn compare_maps(
    label: &str,
    rust: &BTreeMap<String, u16>,
    schema: &BTreeMap<String, u16>,
) -> Result<()> {
    let rust_keys: BTreeSet<_> = rust.keys().cloned().collect();
    let schema_keys: BTreeSet<_> = schema.keys().cloned().collect();
    let missing: Vec<_> = rust_keys.difference(&schema_keys).cloned().collect();
    let extra: Vec<_> = schema_keys.difference(&rust_keys).cloned().collect();
    let mismatch: Vec<_> = rust_keys
        .intersection(&schema_keys)
        .filter_map(|name| {
            let rust_value = rust[name];
            let schema_value = schema[name];
            (rust_value != schema_value)
                .then(|| format!("{name}: rust={rust_value} schema={schema_value}"))
        })
        .collect();
    if missing.is_empty() && extra.is_empty() && mismatch.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{label} mismatch: missing={missing:?} extra={extra:?} value_mismatch={mismatch:?}"
    ))
}

fn compare_sets(label: &str, rust: &BTreeSet<String>, schema: &BTreeSet<String>) -> Result<()> {
    let missing: Vec<_> = rust.difference(schema).cloned().collect();
    let extra: Vec<_> = schema.difference(rust).cloned().collect();
    if missing.is_empty() && extra.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{label} mismatch: missing={missing:?} extra={extra:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_host_layout_requires_layer_directories() {
        let root = temp_observe_layout_root();
        create_required_observe_layout(&root);
        check_observe_host_layout(&root).unwrap();

        let flat_layer = root.join("tools/tx_observe_host/l4_readers.py");
        fs::write(&flat_layer, "").unwrap();
        let err = check_observe_host_layout(&root).unwrap_err();
        assert!(
            err.contains("Python host layers must be package directories")
                && err.contains("l4_readers.py"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn check_schema_text_accepts_matching_inventory() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = [
  { name = "ts", type = "UBIGINT" },
  { name = "kind", type = "VARCHAR" },
]

[[host.projections]]
id = "spans"
kind = "derived_table"
file = "spans.parquet"
columns = [
  { name = "span", type = "VARCHAR" },
  { name = "dur", type = "UBIGINT" },
]

[[event_families]]
id = "counter"
levels = ["boundary"]
payloads = ["counter_value"]
control_group = "always_on_when_observe_enabled"
projection = ["records", "spans"]
"#;

        let record_rs = r#"
pub enum TxTraceKind {
    Counter = 13,
}

pub enum TxTraceLevel {
    Boundary = 0,
}
"#;
        let payload_rs = r#"
pub enum TxPayloadTag {
    CounterValue = 31,
}
"#;
        let cargo_toml = r#"
[workspace.lints.rust]
unexpected_cfgs = { level = "warn", check-cfg = ['cfg(tx_lock_metrics)'] }
"#;
        let analyzer_py = minimal_analyzer_host_boundary();

        assert_eq!(
            check_schema_text(
                schema,
                SchemaRustSources {
                    record_rs,
                    payload_rs,
                    types_lib_rs: minimal_types_lib_rs(),
                },
                cargo_toml,
                analyzer_py,
                minimal_perfetto_writer(),
                minimal_hart_emitter_rs()
            )
            .unwrap(),
            CheckReport {
                levels: 1,
                record_kinds: 1,
                payloads: 1,
                payload_structs: 0,
                cfgs: 1,
                projections: 2,
                tracks: 0,
                hart_emitter_methods: 0,
            }
        );
    }

    #[test]
    fn check_schema_text_rejects_mismatched_payload_tag() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 32

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = minimal_analyzer_host_boundary();

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("TxPayloadTag mismatch") && err.contains("CounterValue"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_missing_analyzer_host_boundary() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = r#"
ANALYZER_DECODER_VERSION = "test"
"#;

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("analyzer host boundary mismatch") && err.contains("tx_observe_host"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_analyzer_local_projection_schemas() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = r#"
ANALYZER_DECODER_VERSION = "test"
class TraceIntegrity:
    pass
class TraceStreamMarker:
    pass
class TraceEventStream:
    pass
class ProjectionInput:
    pass
def analyze_projection():
    pass
def run_sql_projection():
    pass
def export_projection_parquet():
    pass
def run_python_projection():
    pass
RECORD_SQL_SCHEMA = []
"#;

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("local schema constants") && err.contains("RECORD_SQL_SCHEMA"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_analyzer_local_raw_decode() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = r#"
ANALYZER_DECODER_VERSION = "test"
from tx_observe_host import TraceIntegrity, TraceEventStream, ProjectionInput
def decode_record_bytes():
    pass
def analyze_projection():
    pass
def run_sql_projection():
    pass
def export_projection_parquet():
    pass
def run_python_projection():
    pass
"#;

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("raw reader/decode ownership") && err.contains("decode_record_bytes"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_analyzer_local_l6_execution() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = r#"
from tx_observe_host import (
    ANALYZER_DECODER_VERSION,
    TraceIntegrity,
    TraceStreamMarker,
    TraceEventStream,
    ProjectionInput,
    run_sql_projection,
    export_projection_parquet,
    run_python_projection,
)
def analyze_projection():
    pass
def build_derived_tables():
    pass
"#;

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("L6 table/projection execution") && err.contains("build_derived_tables"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_missing_event_family_projection_ref() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = [
  { name = "ts", type = "UBIGINT" },
]

[[event_families]]
id = "counter"
projection = ["records", "text_report"]
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = minimal_analyzer_host_boundary();

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("event family reference mismatch")
                && err.contains("counter projection -> text_report"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_missing_event_family_payload_ref() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = [
  { name = "ts", type = "UBIGINT" },
]

[[event_families]]
id = "counter"
levels = ["boundary"]
payloads = ["missing_payload"]
control_group = "always_on_when_observe_enabled"
projection = ["records"]
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = minimal_analyzer_host_boundary();

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("event family reference mismatch")
                && err.contains("counter payload -> missing_payload"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_mismatched_payload_field() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31
struct = "PayloadCounterValue"
size_bytes = 16
fields = [
  { name = "counter_id", type = "u64" },
]

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"
pub enum TxPayloadTag { CounterValue = 31 }
pub struct PayloadCounterValue {
    pub counter_id: u32,
}
"#;
        let types_lib_rs = "assert!(size_of::<PayloadCounterValue>() == 16);";
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = minimal_analyzer_host_boundary();

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs,
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("payload struct mismatch") && err.contains("PayloadCounterValue"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_mismatched_explicit_track() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []

[[names.families]]
pattern = "debug.alloc.*"

[[tracks.explicit]]
id = "zone_slab"
const = "ALLOC_TRACK_ZONE_SLAB"
track_id_hex = "0xd500000000000002"
name = "debug.alloc.zone.slab"
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"
pub enum TxPayloadTag { CounterValue = 31 }
pub const EXPLICIT_TRACK_ID_PREFIX: u64 = 0xD500_0000_0000_0000;
pub const ALLOC_TRACK_ZONE_SLAB: u64 = EXPLICIT_TRACK_ID_PREFIX | 0x0001;
"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = minimal_analyzer_host_boundary();
        let perfetto_writer = r#"
fn explicit_track_descriptor(track_id: u64) -> Option<(&'static str, u8)> {
    let name = match track_id {
        ALLOC_TRACK_ZONE_SLAB => "debug.alloc.zone.slab",
        _ => return None,
    };
    Some((name, 0))
}
"#;

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            perfetto_writer,
            minimal_hart_emitter_rs(),
        )
        .unwrap_err();
        assert!(
            err.contains("explicit track mismatch") && err.contains("ALLOC_TRACK_ZONE_SLAB"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_missing_hart_emitter_method() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []

[[kernel.hart_emitter_methods]]
name = "debug_counter"
surface = "typed_producer"
record_kind = "counter"
levels = ["boundary"]
payloads = ["counter_value"]
event_family = "counter"
producer_allowed = true
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = minimal_analyzer_host_boundary();
        let tx_observe_lib_rs = r#"
pub struct HartEmitter;
impl HartEmitter {
    pub fn counter(&self) {}
}
"#;

        let err = check_schema_text(
            schema,
            SchemaRustSources {
                record_rs,
                payload_rs,
                types_lib_rs: minimal_types_lib_rs(),
            },
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
            tx_observe_lib_rs,
        )
        .unwrap_err();
        assert!(
            err.contains("HartEmitter method mismatch") && err.contains("debug_counter"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn render_schema_catalog_lists_hart_emitter_methods() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = []

[[kernel.hart_emitter_methods]]
name = "debug_counter"
surface = "typed_producer"
record_kind = "counter"
levels = ["boundary"]
payloads = ["counter_value"]
event_family = "counter"
producer_allowed = true
"#;
        let schema: ObserveSchema = toml::from_str(schema).expect("parse schema");
        let catalog = render_schema_catalog(&schema).expect("render catalog");
        assert!(catalog.contains("pub const HART_EMITTER_METHODS"));
        assert!(catalog.contains("name: \"debug_counter\""));
        assert!(catalog.contains("producer_allowed: true"));
    }

    #[test]
    fn render_host_catalog_lists_projection_schemas() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[host.inputs]]
id = "txtrace"
extension = ".txtrace"
reader_target = "TraceReader::TxtraceRegion"

[[host.projections]]
id = "records"
kind = "sql_view"
columns = [
  { name = "ts", type = "UBIGINT" },
]

[[host.projections]]
id = "spans"
kind = "derived_table"
file = "spans.parquet"
columns = [
  { name = "span", type = "VARCHAR" },
]
"#;
        let schema: ObserveSchema = toml::from_str(schema).expect("parse schema");
        let catalog = render_host_catalog(&schema).expect("render host catalog");

        assert!(catalog.contains("\"schema\": \"tx-observe-host-catalog-v0\""));
        assert!(catalog.contains("\"id\": \"txtrace\""));
        assert!(catalog.contains("\"id\": \"records\""));
        assert!(catalog.contains("\"file\": \"spans.parquet\""));
        assert!(catalog.contains("\"columns\""));
    }

    #[test]
    fn render_host_catalog_lists_control_groups_and_event_families() {
        let schema = r#"
[schema]
id = "txobserve"
version = 0

[[abi.levels]]
rust = "TxTraceLevel::Boundary"
value = 0

[[abi.record_kinds]]
rust = "TxTraceKind::Counter"
value = 13

[[payloads]]
id = "counter_value"
rust_tag = "TxPayloadTag::CounterValue"
tag = 31

[[controls.cfgs]]
name = "tx_lock_metrics"

[[controls.groups]]
id = "lock_metrics"
title = "Lock metrics"
default = false
requires_cfg = ["tx_lock_metrics"]
local_cfgs = ["tx_lock_metrics_vm"]
events = ["lock_metric"]

[[host.projections]]
id = "records"
kind = "sql_view"
columns = [
  { name = "ts", type = "UBIGINT" },
]

[[host.projections]]
id = "text_report"
kind = "report"
coverage_default = "event_specific"

[[event_families]]
id = "lock"
levels = ["boundary"]
payloads = ["counter_value"]
control_group = "lock_metrics"
projection = ["records", "text_report"]
"#;
        let schema: ObserveSchema = toml::from_str(schema).expect("parse schema");
        let catalog = render_host_catalog(&schema).expect("render host catalog");

        assert!(catalog.contains("\"cfgs\""));
        assert!(catalog.contains("\"control_groups\""));
        assert!(catalog.contains("\"id\": \"lock_metrics\""));
        assert!(catalog.contains("\"requires_cfg\""));
        assert!(catalog.contains("\"event_families\""));
        assert!(catalog.contains("\"control_group\": \"lock_metrics\""));
        assert!(catalog.contains("\"projection\""));
    }

    fn minimal_types_lib_rs() -> &'static str {
        ""
    }

    fn minimal_perfetto_writer() -> &'static str {
        "fn explicit_track_descriptor(track_id: u64) -> Option<(&'static str, u8)> { None }"
    }

    fn temp_observe_layout_root() -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tx-observe-layout-{}-{unique}", std::process::id()))
    }

    fn create_required_observe_layout(root: &Path) {
        for rel in [
            "crates/tx-observe/src/l0_schema",
            "crates/tx-observe/src/l1_probe_api",
            "crates/tx-observe/src/l2_producer",
            "crates/tx-observe/src/l3_wire",
            "tools/tx-trace-daemon/src/l4_readers",
            "tools/tx-trace-daemon/src/l5_canonical",
            "tools/tx-trace-daemon/src/l6_views",
            "tools/tx_observe_host/l4_readers",
            "tools/tx_observe_host/l5_canonical",
            "tools/tx_observe_host/l6_views",
        ] {
            fs::create_dir_all(root.join(rel)).unwrap();
        }
    }

    fn minimal_analyzer_host_boundary() -> &'static str {
        r#"
from tx_observe_host import (
    ANALYZER_DECODER_VERSION,
    TraceIntegrity,
    TraceStreamMarker,
    TraceEventStream,
    ProjectionInput,
    analyze_projection,
    run_sql_projection,
    export_projection_parquet,
    run_python_projection,
)
"#
    }

    fn minimal_hart_emitter_rs() -> &'static str {
        "pub struct HartEmitter;\nimpl HartEmitter {}"
    }
}
