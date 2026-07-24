use std::path::Path;

#[test]
fn kernel_observe_layers_have_explicit_directories() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for layer in ["l0_schema", "l1_probe_api", "l2_producer", "l3_wire"] {
        assert!(
            src.join(layer).is_dir(),
            "missing kernel observe layer directory: {layer}"
        );
    }
    for host_layer in ["l4_readers", "l5_canonical", "l6_views"] {
        assert!(
            !src.join(host_layer).exists(),
            "host observe layer {host_layer} must not live in tx-observe"
        );
    }
}

#[test]
fn kernel_observe_legacy_top_level_files_stay_removed() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for old_path in ["encode.rs", "generated", "hart_local.rs", "macros.rs"] {
        assert!(
            !src.join(old_path).exists(),
            "legacy kernel observe path must stay under an L0-L3 directory: {old_path}"
        );
    }
}
