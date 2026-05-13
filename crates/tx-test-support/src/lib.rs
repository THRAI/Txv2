#![no_std]

pub mod adapter;
pub use adapter::step_engine::{
    drain_once_unbounded, drain_to_quiescence, init_host, EpochSummary,
};
