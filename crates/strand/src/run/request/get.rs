//! `strand get <path>` (S-ipc): what `set` writes. Not served yet.
//!
//! Empty until its stream serves it: the command does not parse yet
//! (`strand_core::ipc::parse`), so a client hears `unknown command`, as
//! from M4. The stream adds its `parse` arm, its handler here and its
//! arm in `Shell::request` together.
