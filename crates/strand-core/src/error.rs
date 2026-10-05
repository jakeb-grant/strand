//! Errors are values: every read, write and handler returns one of these
//! instead of panicking.

use std::fmt;
use std::sync::Arc;

use crate::NodeId;

/// Everything that can go wrong in the reactive core.
///
/// Errors are cheap to clone and comparable, so a memo can hold one as its
/// value and the equality cut-off still works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The handle points at a node that has been disposed (a stale
    /// generational id). Reads of unmounted state land here.
    Disposed(NodeId),
    /// A node exists but holds a different type than the handle expects.
    /// Only reachable through type-erased ids.
    TypeMismatch(NodeId),
    /// A runtime dependency cycle; the path names every node on it and ends
    /// where it started.
    Cycle(CyclePath),
    /// A write happened while a derived value was being computed. Derived
    /// values (`let`, props, memos) are pure.
    WriteInDerived {
        /// The cell that was written.
        cell: NodeId,
        /// The memo that was computing.
        memo: NodeId,
    },
    /// `flush` or `tick` was called from inside a flush.
    Reentrant,
    /// A handler was cancelled before it finished (its owner unmounted).
    Cancelled,
    /// A keyed-collection operation failed.
    Keyed(crate::keyed::KeyedError),
    /// A handler or async computation failed with its own message.
    Failed(Arc<str>),
}

impl Error {
    /// A handler failure carrying a message.
    pub fn failed(msg: impl Into<Arc<str>>) -> Self {
        Self::Failed(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disposed(id) => write!(f, "read of disposed node {id:?}"),
            Self::TypeMismatch(id) => write!(f, "node {id:?} holds a different type"),
            Self::Cycle(path) => write!(f, "dependency cycle: {path}"),
            Self::WriteInDerived { cell, memo } => {
                write!(
                    f,
                    "write to {cell:?} while computing derived value {memo:?}"
                )
            }
            Self::Reentrant => f.write_str("flush called from inside a flush"),
            Self::Cancelled => f.write_str("handler cancelled"),
            Self::Keyed(e) => write!(f, "{e}"),
            Self::Failed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::keyed::KeyedError> for Error {
    fn from(e: crate::keyed::KeyedError) -> Self {
        Self::Keyed(e)
    }
}

/// The nodes on a cycle, in dependency order, first node repeated at the end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CyclePath {
    /// Node ids along the cycle.
    pub nodes: Vec<NodeId>,
    /// Debug names (from [`crate::Runtime::set_name`]) or `#<index>` for
    /// unnamed nodes, parallel to `nodes`.
    pub names: Vec<Arc<str>>,
}

impl fmt::Display for CyclePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, name) in self.names.iter().enumerate() {
            if i > 0 {
                f.write_str(" -> ")?;
            }
            f.write_str(name)?;
        }
        Ok(())
    }
}
