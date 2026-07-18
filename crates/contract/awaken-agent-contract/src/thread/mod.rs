//! The committed-thread aggregate: the durable substance of one conversation
//! (its messages, runs, state, awaiting ticket, audit log), split by direction —
//! [`read`] reads committed truth back, [`commit`] makes new truth durable.

pub mod commit;
pub mod read;
