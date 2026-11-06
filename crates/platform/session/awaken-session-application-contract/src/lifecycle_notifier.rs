//! The wake-up hint over the durable Session lifecycle outbox.

/// Best-effort low-latency hint that Coordinator's durable lifecycle outbox may
/// have advanced. Implementations must read facts from the repository; losing a
/// notification is safe because startup and periodic replay remain authoritative.
pub trait LifecycleFactNotifier: Send + Sync {
    fn notify(&self);
}
