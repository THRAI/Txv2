//! Cross-file public API regression fixtures for the time-layering scanner.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn fixture_root(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "tx-time-layering-{name}-{}-{unique}",
        std::process::id()
    ))
}

fn write_source(root: &Path, relative: &str, source: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture source parent"))
        .expect("create fixture source directory");
    fs::write(path, source).expect("write fixture source");
}

#[test]
fn public_api_scanner_resolves_split_inherent_receiver_forms() {
    let root = fixture_root("split-public-receivers");
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/mod.rs",
        "pub struct Reactor;\nmod api;\n",
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/api.rs",
        r#"
            use super::Reactor;
            use super::Reactor as Runtime;

            impl Reactor {
                pub fn imported() -> TimerRegistry { loop {} }
            }

            impl super::Reactor {
                pub fn direct_parent() -> TimerRegistry { loop {} }
            }

            impl Runtime {
                pub fn aliased() -> TimerRegistry { loop {} }
            }
        "#,
    );

    let result = super::lint_invariants_time_layering(&root);
    fs::remove_dir_all(&root).expect("remove split receiver fixture");
    let err = result.expect_err("all split public inherent signatures must be rejected");
    assert!(
        err.contains("time-layering lint found 3 issue"),
        "unexpected split receiver result: {err}"
    );
}

#[test]
fn public_api_scanner_preserves_private_parent_and_short_name_isolation() {
    let root = fixture_root("private-and-isolated-receivers");
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/mod.rs",
        r#"
            struct Hidden;
            pub mod left;
            pub mod right;
            mod hidden_api;
        "#,
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/hidden_api.rs",
        r#"
            use super::Hidden;
            impl Hidden {
                pub fn hidden() -> TimerRegistry { loop {} }
            }
        "#,
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/left.rs",
        "pub struct SameName;\n",
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/right.rs",
        r#"
            struct SameName;
            impl SameName {
                pub fn unrelated_private() -> TimerRegistry { loop {} }
            }
        "#,
    );

    let result = super::lint_invariants_time_layering(&root);
    fs::remove_dir_all(&root).expect("remove private and isolation fixture");
    result.expect("private parent types and unrelated same-name types are not public API");
}

#[test]
fn public_api_scanner_resolves_inline_external_and_qualified_module_chains() {
    let root = fixture_root("qualified-module-chains");
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/mod.rs",
        r#"
            pub struct Reactor;
            pub mod external;
            pub mod inline {
                pub struct Inline;

                impl self::Inline {
                    pub fn inline_self() -> TimerRegistry { loop {} }
                }

                mod private_impl {
                    impl super::Inline {
                        pub fn private_impl_module() -> TimerRegistry { loop {} }
                    }
                }
            }
        "#,
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/external/mod.rs",
        r#"
            pub struct External;
            pub mod child;

            impl self::External {
                pub fn external_self() -> TimerRegistry { loop {} }
            }
        "#,
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/external/child.rs",
        r#"
            use crate::runtime::Reactor as RootReactor;

            impl RootReactor {
                pub fn crate_alias() -> TimerRegistry { loop {} }
            }

            impl super::External {
                pub fn external_parent() -> TimerRegistry { loop {} }
            }

            impl crate::runtime::inline::Inline {
                pub fn crate_qualified_inline() -> TimerRegistry { loop {} }
            }
        "#,
    );

    let result = super::lint_invariants_time_layering(&root);
    fs::remove_dir_all(&root).expect("remove qualified module fixture");
    let err = result.expect_err("all reachable module-chain signatures must be rejected");
    assert!(
        err.contains("time-layering lint found 6 issue"),
        "unexpected qualified module result: {err}"
    );
}

#[test]
fn public_api_scanner_rejects_ambiguous_split_import_aliases() {
    let root = fixture_root("ambiguous-receiver-alias");
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/mod.rs",
        "pub mod left;\npub mod right;\nmod api;\n",
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/left.rs",
        "pub struct Reactor;\n",
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/right.rs",
        "pub struct Reactor;\n",
    );
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/api.rs",
        r#"
            use super::left::Reactor as Runtime;
            use super::right::Reactor as Runtime;

            impl Runtime {
                pub fn forbidden() -> TimerRegistry { loop {} }
            }
        "#,
    );

    let result = super::lint_invariants_time_layering(&root);
    fs::remove_dir_all(&root).expect("remove ambiguous alias fixture");
    let err = result.expect_err("ambiguous local receiver aliases must fail closed");
    assert!(
        err.contains("ambiguous") && err.contains("Runtime"),
        "unexpected ambiguous alias result: {err}"
    );
}

#[test]
fn public_api_scanner_rejects_an_unresolved_inherent_receiver() {
    let root = fixture_root("missing-receiver-import");
    write_source(&root, "crates/tx-reactor/src/runtime/mod.rs", "mod api;\n");
    write_source(
        &root,
        "crates/tx-reactor/src/runtime/api.rs",
        r#"
            impl MissingRuntime {
                pub fn forbidden() -> TimerRegistry { loop {} }
            }
        "#,
    );

    let result = super::lint_invariants_time_layering(&root);
    fs::remove_dir_all(&root).expect("remove missing receiver fixture");
    let err = result.expect_err("unresolved local receiver names must fail closed");
    assert!(
        err.contains("unresolved inherent impl receiver") && err.contains("MissingRuntime"),
        "unexpected missing receiver result: {err}"
    );
}
