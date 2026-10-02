//! Infrastructure: durable queues, cluster coordination, metrics and other adapters to the
//! outside world. Use cases depend on these; these never depend on HTTP handlers.

pub mod cluster;
pub mod metrics;
pub mod observe;
pub mod outbox;
pub mod storage;
