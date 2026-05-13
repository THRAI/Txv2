use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["zone"],
    reason = "expose substrate zone Cap used by the reactor-submission seam function-pointer types"
)]
pub mod step_engine {
    pub use tx_substrate::zone::Cap;
}
