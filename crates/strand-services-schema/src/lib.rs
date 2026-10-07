//! The schema texts of the builtin services, without their runtime: what
//! `strand-services` serves and what the language extends its builtin
//! schema with (`Schema::extend`). `strand-dev`'s LSP checks, hovers and
//! completes against them without linking tokio, zbus or PipeWire.
//!
//! Each service module of `strand-services` uses its constant here as
//! its `SCHEMA` (architecture.md, "strand-services-schema").

/// `system`: the portal's appearance settings and the host name.
pub const SYSTEM: &str = include_str!("system.schema");
/// `cpu`: processor load (procfs).
pub const CPU: &str = include_str!("cpu.schema");
/// `memory`: memory use (procfs).
pub const MEMORY: &str = include_str!("memory.schema");

/// `battery`: UPower's display device and power sources.
pub const BATTERY: &str = include_str!("battery.schema");

/// Every builtin service's schema text, in registration order.
pub fn schemas() -> Vec<&'static str> {
    vec![SYSTEM, CPU, MEMORY, BATTERY]
}
