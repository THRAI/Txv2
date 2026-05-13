//! End-to-end macro expansion tests.
//!
//! Proc-macros can only be invoked from a downstream crate; this
//! integration test is that downstream crate. The tests below compile a
//! few stub modules under `#[platform_adapter]` and assert that the
//! injected `__PLATFORM_ADAPTER` manifest constant is present and
//! well-formed.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "vfs",
    reason = "translate substrate index publication into VFS parent/name binding semantics"
)]
mod minimal_inline_module {
    pub fn _touch() -> u32 {
        7
    }
}

#[platform_adapter(
    platform = "reactor",
    domain = "wait",
    apis = ["wait"],
    reason = "bridge reactor wait sources into VFS-side wakeup semantics"
)]
mod with_apis_list {
    pub fn _touch() -> u32 {
        7
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    reason = "wrap substrate WaitSource registration as pipe-side wakeup semantics"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as pipe-side legacy wakeup verbs"
)]
mod stacked_dual_platform {
    pub fn _touch() -> u32 {
        7
    }
}

#[test]
fn injects_manifest_const_for_minimal_module() {
    let m = minimal_inline_module::__PLATFORM_ADAPTER_SUBSTRATE;
    assert!(m.contains("platform=substrate"));
    assert!(m.contains("domain=vfs"));
    assert!(m.contains("reason=translate substrate index publication"));
    assert!(!m.contains("apis="));
}

#[test]
fn injects_manifest_const_with_apis() {
    let m = with_apis_list::__PLATFORM_ADAPTER_REACTOR;
    assert!(m.contains("platform=reactor"));
    assert!(m.contains("domain=wait"));
    assert!(m.contains("apis=wait"));
    assert!(m.contains("reason=bridge reactor wait sources"));
}

#[test]
fn stacked_attributes_inject_per_platform_constants() {
    // Two `#[platform_adapter(...)]` attributes on the same module
    // expand into two distinct manifest constants, namespaced by
    // platform — so a single semantic adapter (e.g. pipe wait_routing)
    // can wrap both substrate and reactor surfaces without splitting
    // the module by platform.
    let s = stacked_dual_platform::__PLATFORM_ADAPTER_SUBSTRATE;
    let r = stacked_dual_platform::__PLATFORM_ADAPTER_REACTOR;
    assert!(s.contains("platform=substrate") && s.contains("domain=wait_routing"));
    assert!(r.contains("platform=reactor") && r.contains("domain=wait_routing"));
}

#[test]
fn original_module_items_still_visible() {
    assert_eq!(minimal_inline_module::_touch(), 7);
    assert_eq!(with_apis_list::_touch(), 7);
    assert_eq!(stacked_dual_platform::_touch(), 7);
}
