//! Live reload: a new program against the running one (design.md, "Live
//! reload and real-time config changes").
//!
//! - [`Identity`]: which node of the new program is which node of the old
//!   one (source span through a token diff, then `id:` name, then position
//!   among same-kind siblings; real ambiguity resets with a warning).
//! - [`Hashes`]: Merkle hashes over what each handler and timer reaches,
//!   so a reload restarts only handlers whose code changed.
//! - [`Build`]: a compiled config (program, identity, hashes, sources),
//!   made off-thread by the loader and handed to the logic thread, which
//!   commits it with [`crate::instantiate::Instance::reload`].
//! - [`loader`]: the module set, the largest consistent set of changed
//!   files, the last good tree and its cache.
//!
//! What a reload does to the running instance (state adoption, the edit
//! table, the scene diff) is [`crate::instantiate::Instance::reload`]'s;
//! its report is [`Report`].

mod identity;
pub mod loader;
mod merkle;

use std::sync::Arc;

pub use identity::{Identity, Sid};
pub use merkle::Hashes;

use crate::diagnostic::Diagnostic;
use crate::lower;
use crate::source::SourceMap;

/// The compiler's version, part of the compiled-output cache key.
pub const COMPILER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A compiled config, ready to mount or to reload into a running
/// instance. Plain data (`Send`): the compiler worker builds it.
#[derive(Clone, Debug)]
pub struct Build {
    pub program: Arc<lower::Program>,
    pub identity: Arc<Identity>,
    pub hashes: Arc<Hashes>,
    /// The sources it was compiled from (diagnostics, the overlay).
    pub sources: Arc<SourceMap>,
    /// Warnings (a build with errors is never made).
    pub warnings: Vec<Diagnostic>,
}

impl Build {
    /// Compile `map` as one program. `prev` is the build it replaces
    /// (identities are inherited from it). The diagnostics on failure
    /// include warnings.
    pub fn compile(prev: Option<&Build>, map: SourceMap) -> Result<Build, Vec<Diagnostic>> {
        Self::compile_with(prev, map, crate::schema::Schema::builtin())
    }

    /// [`Build::compile`] against `schema`.
    pub fn compile_with(
        prev: Option<&Build>,
        map: SourceMap,
        schema: &crate::schema::Schema,
    ) -> Result<Build, Vec<Diagnostic>> {
        let compiled = crate::compile_with(&map, schema);
        if compiled.errors() > 0 {
            return Err(compiled.diagnostics);
        }
        Ok(Self::lowered(prev, map, &compiled, schema))
    }

    /// A build from a checked program without errors.
    pub fn lowered(
        prev: Option<&Build>,
        map: SourceMap,
        compiled: &crate::Compiled,
        schema: &crate::schema::Schema,
    ) -> Build {
        let mut program = lower::lower(&compiled.program, schema);
        program.shaders = compiled.shaders.clone();
        let identity = Identity::derive(prev.map(|p| &*p.identity), &map, &program);
        let hashes = Hashes::compute(&identity, &map, &compiled.program, &program);
        Build {
            program: Arc::new(program),
            identity: Arc::new(identity),
            hashes: Arc::new(hashes),
            sources: Arc::new(map),
            warnings: compiled.diagnostics.clone(),
        }
    }

    /// An empty config (nothing mounted): what runs while a config that
    /// never compiled shows its errors.
    pub fn empty() -> Build {
        Build {
            program: Arc::new(lower::Program::default()),
            identity: Arc::new(Identity::default()),
            hashes: Arc::new(Hashes::default()),
            sources: Arc::new(SourceMap::new()),
            warnings: Vec::new(),
        }
    }
}

/// What kind of edit a reload was (design.md's table, "What each edit
/// does"); a reload is usually several.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EditClass {
    /// The token table changed: swapped, colours spring.
    Token,
    /// Props or bindings patched in place, animating from their current
    /// values.
    Prop,
    /// Nodes added (their `enter` plays).
    NodeAdded,
    /// Nodes removed (their `exit` plays).
    NodeRemoved,
    /// A `state` default changed: adopted where the value was never
    /// changed, else kept with a notice.
    StateDefault,
    /// A `state` renamed or retyped: that cell reset, with a warning.
    StateReset,
    /// Handler code changed: restarted, an in-flight `await` cancelled.
    Handler,
    /// A timer's duration or code changed: the countdown rescaled.
    Timer,
    /// A surface's layer, namespace or kind changed: only it recreated.
    Surface,
    /// A custom service declaration changed: only it restarts.
    Service,
    /// The edit touches a shown `lock`: deferred until unlock.
    LockDeferred,
    /// `strand reload --hard`: non-persisted state dropped, every surface
    /// recreated.
    Hard,
}

impl EditClass {
    pub fn name(self) -> &'static str {
        match self {
            EditClass::Token => "token",
            EditClass::Prop => "prop",
            EditClass::NodeAdded => "node-added",
            EditClass::NodeRemoved => "node-removed",
            EditClass::StateDefault => "state-default",
            EditClass::StateReset => "state-reset",
            EditClass::Handler => "handler",
            EditClass::Timer => "timer",
            EditClass::Surface => "surface",
            EditClass::Service => "service",
            EditClass::LockDeferred => "lock-deferred",
            EditClass::Hard => "hard",
        }
    }
}

/// What a reload did to the running instance.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// The edit classes it was, in table order.
    pub classes: Vec<EditClass>,
    /// State cells kept (instance-qualified paths: `Clock.open`,
    /// `TopBar[<monitor>].expanded`).
    pub kept: Vec<String>,
    /// State cells reset, with why (`renamed`, `type changed (bool →
    /// int)`, `@reset`).
    pub reset: Vec<(String, String)>,
    /// Lines for the overlay and `strand watch`: kept over a changed
    /// default (`launcher.query: kept "fir" (default changed) [reset]`),
    /// ambiguous identities, handlers restarted with an `await` in flight.
    /// The first two are also in [`Report::kept_over_default`] and
    /// [`Report::ambiguous`], for readers that want the cells, not prose.
    pub notices: Vec<String>,
    /// Cells kept although their default changed, with their value as
    /// shown (the overlay's `[reset]`, the inspector's kept badge).
    pub kept_over_default: Vec<KeptCell>,
    /// Identity ambiguities that reset something (the warning text).
    pub ambiguous: Vec<String>,
    /// Handlers restarted (their code changed).
    pub restarted: usize,
    /// In-flight handler tasks cancelled by the restart.
    pub cancelled: usize,
}

/// A state cell kept over a changed default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptCell {
    /// The instance-qualified path (`launcher.query`,
    /// `TopBar[<monitor>].expanded`): what `Instance::reset` takes.
    pub path: String,
    /// The kept value as shown (text quoted: `"fir"`).
    pub shown: String,
}

impl KeptCell {
    /// The notice line: `launcher.query: kept "fir" (default changed)
    /// [reset]`.
    pub fn notice(&self) -> String {
        format!(
            "{}: kept {} (default changed) [reset]",
            self.path, self.shown
        )
    }
}

impl Report {
    /// A cell kept over a changed default: recorded and noticed (once).
    pub fn kept_over(&mut self, cell: KeptCell) {
        self.notice(cell.notice());
        if !self.kept_over_default.contains(&cell) {
            self.kept_over_default.push(cell);
        }
    }

    /// Add a notice line (once).
    pub fn notice(&mut self, line: String) {
        if !self.notices.contains(&line) {
            self.notices.push(line);
        }
    }

    /// Add an edit class (once, in table order).
    pub fn class(&mut self, c: EditClass) {
        if let Err(i) = self.classes.binary_search(&c) {
            self.classes.insert(i, c);
        }
    }
}
