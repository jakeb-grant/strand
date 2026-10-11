//! `strand call <service.action> [args]` (S-ipc): a service-level action
//! (decisions.md m5-owner Q13). Not served yet.
//!
//! Empty until its stream serves it: the command does not parse yet
//! (`strand_core::ipc::parse`), so a client hears `unknown command`, as
//! from M4. The stream adds its `parse` arm, its handler here and its
//! arm in `Shell::request` together.
