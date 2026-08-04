//! Coordinator-owned runtime interfaces over the protocol-neutral SharedHost.

mod durable_ops;

pub use durable_ops::durable_ops_router;
