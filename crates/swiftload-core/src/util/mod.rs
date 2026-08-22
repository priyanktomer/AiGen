//! Pure, dependency-light building blocks. Everything here is unit-testable without a
//! network, a filesystem, or a clock.

pub mod backoff;
pub mod filename;
pub mod intervals;
pub mod rate;
pub mod redact;
