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
             \tcargo xtask observe-schema check [--schema schema/txobserve.toml]"
            .into());
    };
    match subcmd.as_str() {
        "check" => observe_schema_check(root, &args[1..]),
        other => Err(format!(
            "unknown observe-schema subcommand '{other}'; expected check"
        )),
    }
}

fn observe_schema_check(root: &Path, args: &[String]) -> Result<()> {
    let schema_path = optional_option_value(args, "--schema")
        .map(PathBufLike::from)
        .map(|path| resolve_path(root, path.0))
        .unwrap_or_else(|| root.join("schema/txobserve.toml"));
    let schema = fs::read_to_string(&schema_path)
        .map_err(|err| format!("failed to read {}: {err}", schema_path.display()))?;
    let record_rs_path = root.join("crates/tx-observe-types/src/record.rs");
    let payload_rs_path = root.join("crates/tx-observe-types/src/payload.rs");
    let types_lib_rs_path = root.join("crates/tx-observe-types/src/lib.rs");
    let cargo_toml_path = root.join("Cargo.toml");
    let analyzer_path = root.join("tools/tx-observe-analyze.py");
    let perfetto_writer_path = root.join("tools/tx-trace-daemon/src/perfetto/writer.rs");
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

    let report = check_schema_text(
        &schema,
        &record_rs,
        &payload_rs,
        &types_lib_rs,
        &cargo_toml,
        &analyzer_py,
        &perfetto_writer,
    )?;
    println!(
        "observe-schema check: ok (levels={} record_kinds={} payloads={} payload_structs={} cfgs={} projections={} tracks={})",
        report.levels,
        report.record_kinds,
        report.payloads,
        report.payload_structs,
        report.cfgs,
        report.projections,
        report.tracks
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
}

#[derive(Debug, Deserialize)]
struct HostSchema {
    projections: Vec<ProjectionEntry>,
}

#[derive(Debug, Deserialize)]
struct ProjectionEntry {
    id: String,
    kind: String,
    file: Option<String>,
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

#[derive(Debug, Eq, PartialEq)]
struct CheckReport {
    levels: usize,
    record_kinds: usize,
    payloads: usize,
    payload_structs: usize,
    cfgs: usize,
    projections: usize,
    tracks: usize,
}

fn check_schema_text(
    schema: &str,
    record_rs: &str,
    payload_rs: &str,
    types_lib_rs: &str,
    cargo_toml: &str,
    analyzer_py: &str,
    perfetto_writer: &str,
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

    let levels = parse_rust_enum_discriminants(record_rs, "TxTraceLevel")?;
    let record_kinds = parse_rust_enum_discriminants(record_rs, "TxTraceKind")?;
    let payloads = parse_rust_enum_discriminants(payload_rs, "TxPayloadTag")?;
    let payload_structs = parse_payload_struct_fields(payload_rs)?;
    let payload_sizes = parse_payload_size_assertions(types_lib_rs)?;
    let cfgs = parse_workspace_cfgs(cargo_toml)?;
    let analyzer_projections = parse_analyzer_projection_schemas(analyzer_py)?;
    let schema_projections = projection_schemas_from_schema(&schema.host.projections)?;
    let track_consts = parse_explicit_track_consts(payload_rs)?;
    let writer_track_names = parse_explicit_track_names(perfetto_writer)?;
    check_event_family_refs(&schema)?;
    check_explicit_tracks(&schema, &track_consts, &writer_track_names)?;

    compare_maps("TxTraceLevel", &levels, &level_schema)?;
    compare_maps("TxTraceKind", &record_kinds, &kind_schema)?;
    compare_maps("TxPayloadTag", &payloads, &payload_schema)?;
    compare_payload_structs(&schema.payloads, &payload_structs, &payload_sizes)?;
    compare_sets("cfg", &cfgs, &cfg_schema)?;
    compare_columns(
        "host projection",
        &analyzer_projections,
        &schema_projections,
    )?;

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
            .find(|ch: char| ch == ' ' || ch == '{')
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

fn parse_analyzer_projection_schemas(text: &str) -> Result<BTreeMap<String, Vec<ColumnEntry>>> {
    let mut out = BTreeMap::new();
    let parquet_body = python_assignment_body(text, "PARQUET_SCHEMAS", '{', '}')?;
    let mut rest = parquet_body;
    while let Some(start) = rest.find('"') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('"') else {
            return Err("unterminated PARQUET_SCHEMAS key".into());
        };
        let key = &rest[..end];
        rest = &rest[end + 1..];
        let Some(open) = rest.find('[') else {
            return Err(format!("PARQUET_SCHEMAS['{key}'] missing list"));
        };
        let list_start = open + 1;
        let list_end = matching_delimiter(rest, open, '[', ']')?;
        let body = &rest[list_start..list_end];
        out.insert(key.to_string(), parse_python_column_tuples(body)?);
        rest = &rest[list_end + 1..];
    }

    for (key, assignment) in [
        ("records", "RECORD_SQL_SCHEMA"),
        ("repairs", "REPAIR_SQL_SCHEMA"),
        ("names", "NAME_SQL_SCHEMA"),
    ] {
        let body = python_assignment_body(text, assignment, '[', ']')?;
        let columns = parse_python_column_tuples(body)?;
        if !columns.is_empty() {
            out.insert(key.to_string(), columns);
        }
    }
    Ok(out)
}

fn python_assignment_body<'a>(
    text: &'a str,
    assignment: &str,
    open: char,
    close: char,
) -> Result<&'a str> {
    let needle = format!("{assignment} = ");
    let Some(start) = text.find(&needle) else {
        return Err(format!("missing Python assignment {assignment}"));
    };
    let after = &text[start + needle.len()..];
    let Some(open_rel) = after.find(open) else {
        return Err(format!("missing '{open}' for {assignment}"));
    };
    let body_start = start + needle.len() + open_rel + 1;
    let body_end_rel = matching_delimiter(after, open_rel, open, close)?;
    Ok(&after[open_rel + 1..body_end_rel])
        .map(|_| &text[body_start..start + needle.len() + body_end_rel])
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

fn parse_python_column_tuples(body: &str) -> Result<Vec<ColumnEntry>> {
    let mut columns = Vec::new();
    let mut rest = body;
    while let Some(open) = rest.find("(\"") {
        rest = &rest[open + 2..];
        let Some(name_end) = rest.find('"') else {
            return Err("unterminated Python column name".into());
        };
        let name = rest[..name_end].to_string();
        rest = &rest[name_end + 1..];
        let Some(type_start) = rest.find('"') else {
            return Err(format!("column '{name}' missing type string"));
        };
        rest = &rest[type_start + 1..];
        let Some(type_end) = rest.find('"') else {
            return Err(format!("column '{name}' has unterminated type string"));
        };
        let ty = rest[..type_end].to_string();
        columns.push(ColumnEntry { name, ty });
        rest = &rest[type_end + 1..];
    }
    Ok(columns)
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

fn compare_columns(
    label: &str,
    analyzer: &BTreeMap<String, Vec<ColumnEntry>>,
    schema: &BTreeMap<String, Vec<ColumnEntry>>,
) -> Result<()> {
    let analyzer_keys: BTreeSet<_> = analyzer.keys().cloned().collect();
    let schema_keys: BTreeSet<_> = schema.keys().cloned().collect();
    let missing: Vec<_> = analyzer_keys.difference(&schema_keys).cloned().collect();
    let extra: Vec<_> = schema_keys.difference(&analyzer_keys).cloned().collect();
    let mismatch: Vec<_> = analyzer_keys
        .intersection(&schema_keys)
        .filter_map(|key| {
            let analyzer_cols = &analyzer[key];
            let schema_cols = &schema[key];
            (analyzer_cols != schema_cols)
                .then(|| format!("{key}: analyzer={analyzer_cols:?} schema={schema_cols:?}"))
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
        let analyzer_py = r#"
PARQUET_SCHEMAS = {
    "spans.parquet": [
        ("span", "VARCHAR"),
        ("dur", "UBIGINT"),
    ],
}

RECORD_SQL_SCHEMA = [
    ("ts", "UBIGINT"),
    ("kind", "VARCHAR"),
]

REPAIR_SQL_SCHEMA = []
NAME_SQL_SCHEMA = []
"#;

        assert_eq!(
            check_schema_text(
                schema,
                record_rs,
                payload_rs,
                minimal_types_lib_rs(),
                cargo_toml,
                analyzer_py,
                minimal_perfetto_writer()
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
        let analyzer_py = "PARQUET_SCHEMAS = {}\nRECORD_SQL_SCHEMA = []\nREPAIR_SQL_SCHEMA = []\nNAME_SQL_SCHEMA = []";

        let err = check_schema_text(
            schema,
            record_rs,
            payload_rs,
            minimal_types_lib_rs(),
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
        )
        .unwrap_err();
        assert!(
            err.contains("TxPayloadTag mismatch") && err.contains("CounterValue"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn check_schema_text_rejects_mismatched_projection_column() {
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
  { name = "kind", type = "INTEGER" },
]
"#;
        let record_rs = r#"
pub enum TxTraceKind { Counter = 13 }
pub enum TxTraceLevel { Boundary = 0 }
"#;
        let payload_rs = r#"pub enum TxPayloadTag { CounterValue = 31 }"#;
        let cargo_toml =
            "[workspace.lints.rust]\nunexpected_cfgs = { check-cfg = ['cfg(tx_lock_metrics)'] }";
        let analyzer_py = r#"
PARQUET_SCHEMAS = {}
RECORD_SQL_SCHEMA = [
    ("ts", "UBIGINT"),
    ("kind", "VARCHAR"),
]
REPAIR_SQL_SCHEMA = []
NAME_SQL_SCHEMA = []
"#;

        let err = check_schema_text(
            schema,
            record_rs,
            payload_rs,
            minimal_types_lib_rs(),
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
        )
        .unwrap_err();
        assert!(
            err.contains("host projection mismatch") && err.contains("records"),
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
        let analyzer_py = r#"
PARQUET_SCHEMAS = {}
RECORD_SQL_SCHEMA = [
    ("ts", "UBIGINT"),
]
REPAIR_SQL_SCHEMA = []
NAME_SQL_SCHEMA = []
"#;

        let err = check_schema_text(
            schema,
            record_rs,
            payload_rs,
            minimal_types_lib_rs(),
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
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
        let analyzer_py = r#"
PARQUET_SCHEMAS = {}
RECORD_SQL_SCHEMA = [
    ("ts", "UBIGINT"),
]
REPAIR_SQL_SCHEMA = []
NAME_SQL_SCHEMA = []
"#;

        let err = check_schema_text(
            schema,
            record_rs,
            payload_rs,
            minimal_types_lib_rs(),
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
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
        let analyzer_py = "PARQUET_SCHEMAS = {}\nRECORD_SQL_SCHEMA = []\nREPAIR_SQL_SCHEMA = []\nNAME_SQL_SCHEMA = []";

        let err = check_schema_text(
            schema,
            record_rs,
            payload_rs,
            types_lib_rs,
            cargo_toml,
            analyzer_py,
            minimal_perfetto_writer(),
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
        let analyzer_py = "PARQUET_SCHEMAS = {}\nRECORD_SQL_SCHEMA = []\nREPAIR_SQL_SCHEMA = []\nNAME_SQL_SCHEMA = []";
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
            record_rs,
            payload_rs,
            minimal_types_lib_rs(),
            cargo_toml,
            analyzer_py,
            perfetto_writer,
        )
        .unwrap_err();
        assert!(
            err.contains("explicit track mismatch") && err.contains("ALLOC_TRACK_ZONE_SLAB"),
            "unexpected error: {err}"
        );
    }

    fn minimal_types_lib_rs() -> &'static str {
        ""
    }

    fn minimal_perfetto_writer() -> &'static str {
        "fn explicit_track_descriptor(track_id: u64) -> Option<(&'static str, u8)> { None }"
    }
}
