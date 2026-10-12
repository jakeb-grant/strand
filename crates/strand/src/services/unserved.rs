//! [`UnservedHost`]: a service a plugin schema file declares that no
//! linked store serves (docs/architecture.md, "Plugin builder"; the file
//! of a plugin binary that is not the one running). Reads answer at the
//! schema's defaults; writes, calls and actions are refused saying why;
//! and while the config reads it, one notice on the overlay and to
//! `strand watch` says which file declared it.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use strand_compiler::vm::host::{ActionCtx, ActionTarget, PathSeg, ServiceHost};
use strand_compiler::vm::schema_host::SchemaHost;
use strand_compiler::vm::{Value, ValueKey};
use strand_core::{Error, EventQueue, KeyedSignal, NodeId, Runtime};
use strand_services::{ServiceDiagnostic, Services};

/// See the module docs.
pub struct UnservedHost {
    name: String,
    notice: String,
    fallback: Rc<SchemaHost>,
    services: Services,
    readers: Cell<u32>,
}

impl std::fmt::Debug for UnservedHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnservedHost")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// What the notice and every refusal say.
pub fn notice(name: &str, path: &Path) -> String {
    format!(
        "`{name}` is declared by {} but this binary does not serve it; run the plugin binary \
         that wrote it, or remove the file if that plugin is gone",
        path.display()
    )
}

impl UnservedHost {
    pub fn new(name: &str, path: &Path, fallback: Rc<SchemaHost>, services: Services) -> Self {
        UnservedHost {
            name: name.into(),
            notice: notice(name, path),
            fallback,
            services,
            readers: Cell::new(0),
        }
    }

    fn refused(&self) -> Error {
        Error::failed(self.notice.as_str())
    }

    fn report(&self, resolved: bool) {
        self.services.report(ServiceDiagnostic {
            service: self.name.clone(),
            message: self.notice.clone(),
            notice: true,
            resolved,
        });
    }
}

impl ServiceHost for UnservedHost {
    fn read(&self, rt: &Runtime, service: &str, field: &str) -> Result<Value, Error> {
        self.fallback.read(rt, service, field)
    }

    fn sources(&self, rt: &Runtime, service: &str, field: Option<&str>) -> Vec<NodeId> {
        self.fallback.sources(rt, service, field)
    }

    fn read_keyed(
        &self,
        rt: &Runtime,
        service: &str,
        field: &str,
    ) -> Option<KeyedSignal<ValueKey, Value>> {
        self.fallback.read_keyed(rt, service, field)
    }

    fn write(&self, _: &Runtime, _: &str, _: &[PathSeg], _: Value) -> Result<(), Error> {
        Err(self.refused())
    }

    fn write_item(&self, _: &Runtime, _: &Value, _: &[PathSeg], _: Value) -> Result<(), Error> {
        Err(self.refused())
    }

    fn call(&self, _: &Runtime, _: &str, _: &str, _: &[Value]) -> Result<Value, Error> {
        Err(self.refused())
    }

    fn action(
        &self,
        _: &Runtime,
        _: ActionTarget<'_>,
        _: &str,
        _: &[Value],
        _: ActionCtx,
    ) -> Result<(), Error> {
        Err(self.refused())
    }

    fn event(&self, rt: &Runtime, service: &str, event: &str) -> Option<EventQueue<Vec<Value>>> {
        self.fallback.event(rt, service, event)
    }

    fn acquire(&self, _: &Runtime, _: &str) {
        let n = self.readers.get();
        self.readers.set(n + 1);
        if n == 0 {
            self.report(false);
        }
    }

    fn release(&self, _: &Runtime, _: &str) {
        let n = self.readers.get();
        if n == 0 {
            return;
        }
        self.readers.set(n - 1);
        if n == 1 {
            self.report(true);
        }
    }
}
