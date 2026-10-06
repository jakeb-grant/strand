//! The [`Service`] trait (`#[service(name = "…")]` implements it), typed
//! calls ([`FromCall`]) and how a service starts ([`Start`]).

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use strand_core::{Error, Runtime};

use crate::cx::Cx;
use crate::data::{Data, DataError};
use crate::store::Store;

/// A service: a [`Store`] with a name, its schema text and a body that
/// runs on a services thread.
///
/// `#[service(name = "cpu", schema = CPU_SCHEMA)]` on the store struct
/// implements it, running the struct's `async fn run(cx: Cx<Self>) ->
/// Result<(), ServiceError>` on the shared current-thread tokio runtime
/// (or, with `thread`, a blocking `fn run(cx)` on a thread of its own).
pub trait Service: Store {
    /// The global name (`battery`).
    const NAME: &'static str;
    /// Its actions (`notifications.clear()`, `ws.focus()`), typed from
    /// the call (`#[derive(Call)]`; [`NoCall`] for none).
    type Action: FromCall + Send + 'static;
    /// Its async methods (`apps.search(q) -> Async<[Hit]>`), answered
    /// with a [`Reply`](crate::Reply) ([`NoCall`] for none).
    type Call: FromCall + Send + 'static;
    /// Its declarations in the schema language (the service, and the
    /// records only it hands out): `Schema::extend` input, replacing the
    /// builtin schema's provisional stubs of the same names.
    fn schema() -> &'static str;
    /// Its `fn` methods (`workspaces.on(screen)`): pure functions of its
    /// state, computed on the logic thread from the cells (tracked
    /// reads), so they never wait on the service. `None`: no such method.
    fn call(
        _cells: &Self::Cells,
        _rt: &Runtime,
        _method: &str,
        _args: &[Data],
    ) -> Option<Result<Data, Error>> {
        None
    }
    /// Its body, given its context.
    fn start(cx: Cx<Self>) -> Start;
}

/// Why a service's body ended early.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceError(pub String);

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ServiceError {}

impl From<String> for ServiceError {
    fn from(s: String) -> Self {
        ServiceError(s)
    }
}

impl From<&str> for ServiceError {
    fn from(s: &str) -> Self {
        ServiceError(s.to_string())
    }
}

impl From<std::io::Error> for ServiceError {
    fn from(e: std::io::Error) -> Self {
        ServiceError(e.to_string())
    }
}

impl From<zbus::Error> for ServiceError {
    fn from(e: zbus::Error) -> Self {
        ServiceError(e.to_string())
    }
}

impl From<zbus::fdo::Error> for ServiceError {
    fn from(e: zbus::fdo::Error) -> Self {
        ServiceError(e.to_string())
    }
}

impl From<DataError> for ServiceError {
    fn from(e: DataError) -> Self {
        ServiceError(e.0)
    }
}

/// A service body's future (it runs on the shared runtime's thread and
/// need not be `Send`).
pub type LocalFuture = Pin<Box<dyn Future<Output = Result<(), ServiceError>>>>;

/// Where and how a service runs.
pub enum Start {
    /// On the shared tokio current-thread runtime: the closure (sent to
    /// that thread) builds the body's future there.
    Shared(Box<dyn FnOnce() -> LocalFuture + Send>),
    /// On a thread of its own (`!Send` libraries: PipeWire, the Wayland
    /// toplevel protocols), speaking the same patch and message protocol.
    Thread(Box<dyn FnOnce() -> Result<(), ServiceError> + Send>),
}

impl fmt::Debug for Start {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Start::Shared(_) => "Start::Shared",
            Start::Thread(_) => "Start::Thread",
        })
    }
}

impl Start {
    /// A body on the shared runtime.
    pub fn shared<F, Fut>(f: F) -> Start
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), ServiceError>> + 'static,
    {
        Start::Shared(Box::new(move || Box::pin(f())))
    }

    /// A body on its own thread.
    pub fn thread<F>(f: F) -> Start
    where
        F: FnOnce() -> Result<(), ServiceError> + Send + 'static,
    {
        Start::Thread(Box::new(f))
    }
}

/// An action or async method typed from its call: name (`play_pause`),
/// the item it was called on (`ws.focus()`) and its arguments
/// (`#[derive(Call)]`).
pub trait FromCall: Sized {
    /// Every call name it takes.
    const NAMES: &'static [&'static str];
    fn from_call(name: &str, item: Option<&Data>, args: &[Data]) -> Result<Self, DataError>;
    /// The record types whose items it is called on (`Workspace` for
    /// `ws.focus()`), so the language side routes item calls here.
    fn item_records() -> Vec<String> {
        Vec::new()
    }
}

/// No actions, or no async methods.
#[derive(Debug)]
pub enum NoCall {}

impl FromCall for NoCall {
    const NAMES: &'static [&'static str] = &[];

    fn from_call(name: &str, _: Option<&Data>, _: &[Data]) -> Result<Self, DataError> {
        Err(DataError::new(format!("no call `{name}`")))
    }
}
