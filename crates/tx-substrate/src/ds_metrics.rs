//! Compile-time gated substrate data-structure method timing.
//!
//! Call sites use local gates such as `tx_ds_metrics_page_allocator` or
//! `tx_ds_metrics_zone`; this module only exists when the global
//! `tx_ds_metrics` cfg is open.

#[cfg(tx_ds_metrics)]
const DS_METRIC_DURATION_NS: tx_observe::EventNameId =
    tx_observe::EventNameId::from_name(b"debug.ds.method.duration_ns");

#[cfg(tx_ds_metrics)]
#[inline(always)]
pub fn measure<R>(method_name: &'static [u8], f: impl FnOnce() -> R) -> R {
    measure_for_zone(method_name, tx_observe::EventNameId::from_raw(0), f)
}

#[cfg(tx_ds_metrics)]
#[inline(always)]
pub fn measure_for_zone<R>(
    method_name: &'static [u8],
    zone: tx_observe::EventNameId,
    f: impl FnOnce() -> R,
) -> R {
    let method = tx_observe::EventNameId::from_name(method_name);
    let start = tx_observe::clock_now_ns();
    let result = f();
    let duration = tx_observe::clock_now_ns().saturating_sub(start);
    if let Some(emitter) = tx_observe::current() {
        emitter.ds_method_metric_for_zone(method, zone, DS_METRIC_DURATION_NS, duration);
    }
    result
}
