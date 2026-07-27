use std::path::Path;

#[test]
fn host_observe_layers_have_explicit_directories() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for layer in ["l4_readers", "l5_canonical", "l6_views"] {
        assert!(
            src.join(layer).is_dir(),
            "missing host observe layer directory: {layer}"
        );
    }
}

#[test]
fn host_observe_legacy_top_level_files_stay_removed() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for old_path in ["decode.rs", "emit_json.rs", "perfetto", "replay.rs"] {
        assert!(
            !src.join(old_path).exists(),
            "legacy host observe path must stay under an L4-L6 directory: {old_path}"
        );
    }
}

#[test]
fn host_observe_layers_expose_typed_boundary_names() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let l4 = std::fs::read_to_string(src.join("l4_readers/mod.rs")).unwrap();
    let l5 = std::fs::read_to_string(src.join("l5_canonical/mod.rs")).unwrap();
    let l6 = std::fs::read_to_string(src.join("l6_views/mod.rs")).unwrap();

    for needle in [
        "TraceInputKind",
        "TraceInput",
        "TraceIntegrity",
        "RawRecordFrame",
        "TraceReader",
    ] {
        assert!(l4.contains(needle), "L4 boundary is missing {needle}");
    }
    for needle in [
        "DecodeBatch",
        "TraceEventStream",
        "TraceStreamMarker",
        "TraceDecoder",
    ] {
        assert!(l5.contains(needle), "L5 boundary is missing {needle}");
    }
    for needle in ["ProjectionInput", "TraceTranscoder"] {
        assert!(l6.contains(needle), "L6 boundary is missing {needle}");
    }
}

#[test]
fn host_l4_separates_file_replay_from_live_drain() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let replay = std::fs::read_to_string(src.join("l4_readers/replay.rs")).unwrap();
    let live = std::fs::read_to_string(src.join("l4_readers/live.rs")).unwrap();

    for needle in [
        "decode_file_stream",
        "decode_file_bytes",
        "trace_stats_from_bytes",
    ] {
        assert!(
            replay.contains(needle),
            "L4 replay path is missing {needle}"
        );
        assert!(
            !live.contains(needle),
            "L4 live drain must not own txtrace file replay helper {needle}"
        );
    }
    for needle in ["run_live_guest_mem", "LiveDrainConfig", "MmapOptions"] {
        assert!(live.contains(needle), "L4 live path is missing {needle}");
    }
    assert!(
        !replay.contains("MmapOptions"),
        "guest-memory mmap ownership must stay in l4_readers/live.rs"
    );
}

#[test]
fn host_raw_record_bytes_only_flow_into_l5_decode() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    visit_rs_files(&src, &mut |path| {
        let text = std::fs::read_to_string(path).unwrap();
        if !text.contains("bytes_for_decode(") {
            return;
        }
        let rel = path.strip_prefix(&src).unwrap();
        let rel_text = rel.to_string_lossy();
        if rel_text != "l4_readers/mod.rs" && rel_text != "l5_canonical/decode.rs" {
            offenders.push(rel_text.into_owned());
        }
    });

    assert!(
        offenders.is_empty(),
        "raw record byte access escaped L4/L5 boundary: {offenders:?}"
    );
}

fn visit_rs_files(dir: &Path, f: &mut dyn FnMut(&Path)) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            visit_rs_files(&path, f);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            f(&path);
        }
    }
}
