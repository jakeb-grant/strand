//! The real services, joined to the language (architecture.md, "Several
//! service crates, one host").
//!
//! - [`schema`]: the builtin schema extended by every service crate this
//!   binary links (`strand_services::schemas()`), its plugin services'
//!   texts and the plugin schema files ([`served`]); `strand check`,
//!   `strand run` (and its compile cache key, the schema's fingerprint)
//!   use it.
//! - [`StoreHost`]: one service store as a `ServiceHost`, converting its
//!   typed cells to and from the VM's `Value` ([`convert`]).
//! - [`Composite`]: one host over every store, routing by service name;
//!   the names nobody serves yet (and the clock and calendar) stay with a
//!   `SchemaHost`.
//! - [`Real`]: what `strand run` builds without `STRAND_MOCK`: the
//!   services registry, the builtin stores and the composite host.

mod adapter;
mod composite;
pub mod convert;
pub mod custom;
mod unserved;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

use strand_compiler::schema::{Schema, plugins};
use strand_compiler::ty::{RecordId, TypeTable};
use strand_compiler::vm::schema_host::SchemaHost;
use strand_core::Runtime;
use strand_services::{Builtin, Buses, Services};

pub use adapter::StoreHost;
pub use composite::{CallTap, Composite, FetchDone};
pub use custom::CustomHost;
pub use unserved::UnservedHost;

use crate::plugin::Registration;

/// The schema `strand check` and `strand run` check against
/// ([`served`]'s).
pub fn schema() -> &'static Schema {
    &served().schema
}

/// The served schema, and what serves it.
#[derive(Debug)]
pub struct Served {
    pub schema: Schema,
    /// The services plugin schema files declare that no linked store
    /// serves, each with its file ([`UnservedHost`]).
    pub unserved: Vec<(String, PathBuf)>,
    /// What was skipped or is stale, for `strand check` to print and
    /// `strand run` to log.
    pub warnings: Vec<String>,
}

/// [`schema_with`] over this binary's plugin services and the plugin
/// schema directory's files, built once per process (a new file is seen
/// after a restart). Its warnings are logged.
pub fn served() -> &'static Served {
    static SERVED: OnceLock<Served> = OnceLock::new();
    SERVED.get_or_init(|| {
        let (files, skipped) = plugins::dir()
            .map(|d| plugins::load(&d))
            .unwrap_or_default();
        let mut s = schema_with(crate::plugin::extras(), &files);
        s.warnings
            .splice(0..0, skipped.iter().map(ToString::to_string));
        for w in &s.warnings {
            log::warn!("{w}");
        }
        s
    })
}

/// The builtin schema with the linked service crates' declarations in
/// place of their provisional stubs, then each of `extras`' texts, then
/// `files` in order, skipping a file named for one of `extras` (code
/// wins over its file) and one that does not apply. A service schema
/// that does not apply (a bug their tests catch:
/// `service_schemas_extend_the_builtin_one`; a plugin's own `run`
/// refuses) is left out with a warning.
#[cold]
#[inline(never)]
pub fn schema_with(extras: &[Registration], files: &[(PathBuf, String)]) -> Served {
    let mut warnings = Vec::new();
    let mut schema = match Schema::builtin_with(&strand_services::schemas()) {
        Ok(s) => s,
        Err((i, errors)) => {
            warnings.push(format!("service schema #{i} does not apply: {errors:?}"));
            Schema::builtin_with(&[]).unwrap_or_default()
        }
    };
    for r in extras {
        if let Err(errors) = schema.extend((r.schema)()) {
            warnings.push(format!(
                "the schema of `{}` does not apply: {errors:?}",
                r.name
            ));
        }
    }
    let linked: Vec<&str> = extras.iter().map(|r| r.name).collect();
    let mut unserved = Vec::new();
    for file in files {
        let (path, text) = file;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if let Some(r) = extras.iter().find(|r| r.name == stem) {
            if plugins::body(text) != (r.schema)() {
                warnings.push(format!(
                    "{}: stale; run `{} --write-schemas`",
                    path.display(),
                    crate::plugin::binary_name()
                ));
            }
            continue;
        }
        let before: Vec<String> = schema.services.keys().cloned().collect();
        let skipped = plugins::apply(&mut schema, std::slice::from_ref(file), &linked);
        warnings.extend(skipped.iter().map(ToString::to_string));
        for name in schema.services.keys() {
            if !before.contains(name) {
                unserved.push((name.clone(), path.clone()));
            }
        }
    }
    Served {
        schema,
        unserved,
        warnings,
    }
}

/// Gives `host` cells at their defaults for `name`, a service `schema`
/// declares that the builtin schema does not (`SchemaHost` holds only
/// the builtin schema's names until told).
fn declare_at_defaults(
    rt: &Runtime,
    host: &SchemaHost,
    types: &TypeTable,
    schema: &Schema,
    name: &str,
    path: &Path,
) {
    use strand_compiler::vm::ServiceHost as _;
    if let Some(&record) = schema.services.get(name) {
        let at_defaults = strand_compiler::lower::CustomService {
            name: name.to_string(),
            record,
            source: strand_compiler::hir::SourceSpec::File {
                path: path.display().to_string(),
            },
            fields: Vec::new(),
            actions: Vec::new(),
        };
        host.declare(rt, &at_defaults, types);
    }
}

/// The records `service`'s values hand out (its own, and those its
/// fields, events and methods reach, transitively) that neither the
/// builtin schema declares nor a store already serves the items of
/// (`claimed`): an [`UnservedHost`] is their items' only owner.
fn handed_out(
    schema: &Schema,
    service: &str,
    claimed: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    use strand_compiler::ty::Ty;
    fn push(ty: &Ty, todo: &mut Vec<RecordId>) {
        match ty {
            Ty::Record(id) => todo.push(*id),
            Ty::List(t, _) | Ty::Optional(t) | Ty::Async(t) => push(t, todo),
            Ty::Tuple(ts) | Ty::Union(ts) => ts.iter().for_each(|t| push(t, todo)),
            _ => {}
        }
    }
    let types = &schema.types;
    let builtin = &Schema::builtin().types;
    let mut seen = std::collections::BTreeSet::new();
    let mut todo: Vec<RecordId> = schema.services.get(service).copied().into_iter().collect();
    let mut out = Vec::new();
    while let Some(id) = todo.pop() {
        let Some(def) = types.records.get(id.0 as usize) else {
            continue;
        };
        if !seen.insert(id.0) {
            continue;
        }
        if builtin.find_record(&def.name).is_none() && !claimed.contains(&def.name) {
            out.push(def.name.clone());
        }
        def.fields.iter().for_each(|f| push(&f.ty, &mut todo));
        for e in &def.events {
            e.params.iter().for_each(|p| push(&p.ty, &mut todo));
        }
        for m in &def.methods {
            m.sigs.iter().for_each(|s| push(&s.ret, &mut todo));
        }
    }
    out
}

/// Under `STRAND_MOCK` the schema host stands in for every service and
/// no [`Real`] starts, so the services `served` adds to the builtin
/// schema (a plugin's own, a file's) get its cells here, read at their
/// defaults as the mock reads every service it does not fill. A file's
/// service is logged as [`Real::start_with`] logs it; the mock raises no
/// service notices (decisions.md m5-plugins, review round 2).
pub fn mock_served(rt: &Runtime, host: &SchemaHost, types: &TypeTable, served: &Served) {
    let builtin = &Schema::builtin().services;
    for name in served.schema.services.keys() {
        if builtin.contains_key(name) {
            continue;
        }
        let file = served.unserved.iter().find(|(n, _)| n == name);
        if let Some((_, path)) = file {
            log::info!(
                "{} declares service `{name}`, which this binary does not serve",
                path.display()
            );
        }
        let path = file.map_or(Path::new(""), |(_, p)| p.as_path());
        declare_at_defaults(rt, host, types, &served.schema, name, path);
    }
}

/// How long `strand set` on a service waits for a stopped service's
/// first read.
pub const SET_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether `path` names a service field (`brightness.level`): the CLI's
/// `set` writes it through the service host.
pub fn is_service_path(path: &str) -> bool {
    let mut parts = path.split('.');
    let (Some(service), Some(_)) = (parts.next(), parts.next()) else {
        return false;
    };
    schema().services.contains_key(service)
}

/// Write `text` to the `rw` service field `path` (`brightness.level`,
/// `audio.sink.volume`) through `host`, read by the field's type as
/// `Instance::set_text` reads a state's: `0.4`, `40%`, `true`, an enum
/// variant, `null`. A number with a sign is a step from the current
/// value (design.md: `strand set audio.sink.volume +5%`), never below 0.
pub fn set_text(
    host: &dyn strand_compiler::vm::ServiceHost,
    rt: &Runtime,
    types: &TypeTable,
    path: &str,
    text: &str,
) -> Result<(), String> {
    set_text_with(schema(), host, rt, types, path, text)
}

/// [`set_text`] with the service and its fields resolved in `s`.
pub fn set_text_with(
    s: &Schema,
    host: &dyn strand_compiler::vm::ServiceHost,
    rt: &Runtime,
    types: &TypeTable,
    path: &str,
    text: &str,
) -> Result<(), String> {
    let service = path.split('.').next().unwrap_or_default();
    let Some(&record) = s.services.get(service) else {
        return Err(format!("nothing is exported as `{path}`"));
    };
    set_field_text(&s.types, record, host, rt, types, path, text)
}

/// The config's own service (`service ppd from dbus …`) that `path`
/// (`ppd.profile`) names a field of, by its record in the program's
/// types: `strand set` writes its `rw` fields as it does a builtin's
/// (decisions.md m5-fixups).
pub fn config_service(prog: &strand_compiler::lower::Program, path: &str) -> Option<RecordId> {
    let mut parts = path.split('.');
    let (Some(service), Some(_)) = (parts.next(), parts.next()) else {
        return None;
    };
    prog.services
        .values()
        .find(|s| s.name == service)
        .map(|s| s.record)
}

/// [`set_text`] of a field of the service whose record is `record` in
/// `decls` (the served schema's types for a builtin or plugin service,
/// the program's for the config's own).
pub fn set_field_text(
    decls: &TypeTable,
    record: RecordId,
    host: &dyn strand_compiler::vm::ServiceHost,
    rt: &Runtime,
    types: &TypeTable,
    path: &str,
    text: &str,
) -> Result<(), String> {
    use strand_compiler::ty::{Prim, Ty};
    use strand_compiler::vm::Value;
    use strand_compiler::vm::host::PathSeg;
    let mut parts = path.split('.');
    let service = parts.next().unwrap_or_default();
    let fields: Vec<&str> = parts.collect();
    if fields.is_empty() {
        return Err(format!("`{path}` names a service, not a field"));
    }
    // The leaf's type, and whether it is writable.
    let mut rec = record;
    let mut leaf = None;
    for (i, f) in fields.iter().enumerate() {
        let def = decls
            .record(rec)
            .fields
            .iter()
            .find(|d| d.name == *f)
            .ok_or_else(|| format!("`{path}` has no field `{f}`"))?;
        if i + 1 == fields.len() {
            leaf = Some(def.clone());
        } else {
            match def.ty.non_null() {
                Ty::Record(r) => rec = *r,
                _ => return Err(format!("`{path}` has no field `{}`", fields[i + 1])),
            }
        }
    }
    let leaf = leaf.ok_or_else(|| format!("`{path}` has no field"))?;
    if !leaf.rw {
        return Err(format!("`{path}` is read-only"));
    }
    let text = text.trim();
    let number = |t: &str| -> Option<f64> {
        match t.strip_suffix('%') {
            Some(p) => p.trim().parse::<f64>().ok().map(|n| n / 100.0),
            None => t.parse::<f64>().ok(),
        }
    };
    let relative = text.starts_with('+') || text.starts_with('-');
    let current = || -> Result<f64, String> {
        let mut v = host
            .read(rt, service, fields[0])
            .map_err(|e| e.to_string())?;
        for f in &fields[1..] {
            v = v.field(types, f).cloned().unwrap_or(Value::Null);
        }
        v.as_f64()
            .ok_or_else(|| format!("`{path}` has no value to step from"))
    };
    let value = match leaf.ty.non_null() {
        _ if text == "null" && matches!(leaf.ty, Ty::Optional(_)) => Value::Null,
        Ty::Prim(Prim::Float | Prim::Percent) => {
            let n = number(text).ok_or_else(|| format!("`{text}` is not a number"))?;
            Value::float(if relative {
                (current()? + n).max(0.0)
            } else {
                n
            })
        }
        Ty::Prim(Prim::Int) => {
            let n: i64 = text
                .parse()
                .map_err(|_| format!("`{text}` is not a whole number"))?;
            Value::int(if relative {
                (current()? as i64 + n).max(0)
            } else {
                n
            })
        }
        Ty::Prim(Prim::Bool) => match text {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(format!("`{text}` is not true or false")),
        },
        Ty::Prim(Prim::Text | Prim::Path) => Value::text(text),
        Ty::Enum(e) => {
            let def = decls.enum_(*e);
            let v = def
                .variant(text)
                .ok_or_else(|| format!("`{text}` is not a {}", def.name))?;
            Value::Enum(*e, v)
        }
        _ => return Err(format!("`{path}` cannot be set from the command line")),
    };
    let segs: Vec<PathSeg> = fields.iter().map(|f| PathSeg::Field((*f).into())).collect();
    host.write(rt, service, &segs, value)
        .map_err(|e| e.to_string())
}

/// The real services of one logic thread.
pub struct Real {
    pub services: Services,
    pub builtin: Builtin,
    /// The host `Instance` gets.
    pub host: Rc<Composite>,
    /// The config's no-code services.
    pub custom: Rc<CustomHost>,
    stores: Vec<Rc<StoreHost>>,
    types: Rc<TypeTable>,
    /// The plugin services, served as the builtins are.
    extras: Vec<Rc<dyn strand_services::DynService>>,
    served: &'static Served,
}

impl std::fmt::Debug for Real {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Real")
            .field("services", &self.services)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl Real {
    /// Register the builtin services on `buses` (nothing starts until a
    /// reader comes), each behind a [`StoreHost`] reading values of
    /// `types`, joined with `fallback` for every other name. `wake` is
    /// called from any thread when a service has sent something: the host
    /// loop then calls [`Services::pump`]. Call it before mounting (no
    /// owner current).
    pub fn start(
        rt: &Runtime,
        types: &TypeTable,
        buses: Buses,
        fallback: Rc<SchemaHost>,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Real {
        let extras = crate::plugin::extras();
        Real::start_with(rt, types, buses, fallback, wake, extras, served())
    }

    /// [`Real::start`] with this binary's plugin services (`extras`,
    /// registered after the builtins and served as they are) and the
    /// schema `served` (the services its files declare that nothing
    /// serves each get an [`UnservedHost`]).
    pub fn start_with(
        rt: &Runtime,
        types: &TypeTable,
        buses: Buses,
        fallback: Rc<SchemaHost>,
        wake: impl Fn() + Send + Sync + 'static,
        extras: &[Registration],
        served: &'static Served,
    ) -> Real {
        let services = Services::new(rt, buses, wake);
        let builtin = Builtin::register(&services, rt);
        let extras: Vec<_> = extras.iter().map(|r| (r.register)(&services, rt)).collect();
        let types = Rc::new(types.clone());
        let mut host = Composite::new(fallback.clone(), types.clone());
        let mut stores = Vec::new();
        let mut claimed = std::collections::BTreeSet::new();
        for svc in builtin.all().into_iter().chain(extras.iter().cloned()) {
            let items = svc.item_records();
            claimed.extend(items.iter().cloned());
            let store = Rc::new(StoreHost::new(rt, svc, types.clone()));
            host.add(store.clone(), &[store.name()], &items);
            stores.push(store);
        }
        for (name, path) in &served.unserved {
            log::info!(
                "{} declares service `{name}`, which this binary does not serve",
                path.display()
            );
            // The fallback answers its reads at the schema's defaults
            // (it holds only the builtin schema's names until told).
            declare_at_defaults(rt, &fallback, &types, &served.schema, name, path);
            let unserved = UnservedHost::new(name, path, fallback.clone(), services.clone());
            // Its item actions and writes are refused too, not simulated
            // by the fallback (review round 3).
            let items = handed_out(&served.schema, name, &claimed);
            claimed.extend(items.iter().cloned());
            host.add(Rc::new(unserved), &[name], &items);
        }
        let custom = Rc::new(CustomHost::new(services.clone(), None));
        host.set_custom(custom.clone());
        Real {
            services,
            builtin,
            host: Rc::new(host),
            custom,
            stores,
            types,
            extras,
            served,
        }
    }

    /// The store serving `name`: a builtin, else a plugin service.
    pub fn served(&self, name: &str) -> Option<Rc<dyn strand_services::DynService>> {
        let builtin = self.builtin.all().into_iter();
        builtin
            .chain(self.extras.iter().cloned())
            .find(|s| s.name() == name)
    }

    /// `strand set brightness.level 0.4` (or `+5%`): an `rw` service
    /// field written from the CLI ([`set_text`]). A service nobody reads
    /// is started for it; for a relative step its first read is waited
    /// for (up to [`SET_WAIT`], that service's only), so the step starts
    /// from its real value. It stops 5 s later.
    pub fn set_text(&self, rt: &Runtime, path: &str, text: &str) -> Result<(), String> {
        let name = path.split('.').next().unwrap_or_default();
        let svc = self.served(name);
        let relative = text.trim().starts_with(['+', '-']);
        if let Some(svc) = &svc {
            svc.acquire(rt);
            if relative {
                self.services.wait_ready_of(rt, Some(name), SET_WAIT);
            }
        }
        let s = &self.served.schema;
        let r = set_text_with(s, &*self.host, rt, &self.types, path, text);
        if let Some(svc) = &svc {
            svc.release(rt);
        }
        r
    }

    /// Stop every service (joining the shared runtime thread) and dispose
    /// the stores' cells.
    pub fn shutdown(&self, rt: &Runtime) {
        self.services.shutdown();
        self.custom.dispose(rt);
        for s in &self.stores {
            s.dispose(rt);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use strand_compiler::SourceMap;
    use strand_compiler::instantiate::{Instance, SceneMirror, Storage};
    use strand_compiler::lower;
    use strand_compiler::vm::{ServiceHost, Value};
    use strand_services::{Cells as _, STOP_GRACE};

    use super::*;

    /// (M6, review r2 major 1) The composite host remembers the press of
    /// the last action a handler ran with one, and when, whatever the
    /// action's fate: the step that hands over the service's answer (the
    /// tray's `opened`) carries it (`run::PressCause`). An action with no
    /// press leaves it.
    #[test]
    fn a_pressed_action_is_remembered_for_its_answer() {
        use strand_compiler::vm::{ActionCtx, ActionTarget, InputCtx};
        let shell = Shell::boot("bar Top { height: 10 }\n");
        let host = shell.real.host.clone();
        let act = |input| {
            let ctx = ActionCtx {
                input,
                invocation: 1,
            };
            let _ = host.action(
                &shell.rt,
                ActionTarget::Service("notifications"),
                "clear",
                &[],
                ctx,
            );
        };
        assert_eq!(host.action_press(), None);
        act(None);
        assert_eq!(host.action_press(), None, "no press");
        let before = Instant::now();
        act(Some(InputCtx {
            surface: 1,
            serial: 9,
        }));
        let (serial, at) = host.action_press().unwrap();
        assert_eq!(serial, 9);
        assert!(at >= before);
        act(None);
        assert_eq!(host.action_press().map(|(s, _)| s), Some(9), "kept");
    }

    /// `strand set` on a service field: absolute and relative values by
    /// the field's type, read-only fields and unknown names refused.
    #[test]
    fn strand_set_writes_service_fields() {
        let rt = Runtime::new();
        let types = schema().types.clone();
        let host = SchemaHost::new(&rt, &types, None);
        host.set(&rt, "brightness.level", Value::float(0.5))
            .unwrap();
        let level =
            |host: &SchemaHost| host.get(&rt, "brightness.level").unwrap().as_f64().unwrap();
        assert!(is_service_path("brightness.level"));
        assert!(!is_service_path("brightness"));
        assert!(!is_service_path("theme.look"));
        set_text(&host, &rt, &types, "brightness.level", "+5%").unwrap();
        assert!((level(&host) - 0.55).abs() < 1e-9, "{}", level(&host));
        set_text(&host, &rt, &types, "brightness.level", "-0.1").unwrap();
        assert!((level(&host) - 0.45).abs() < 1e-9);
        set_text(&host, &rt, &types, "brightness.level", "-100%").unwrap();
        assert_eq!(level(&host), 0.0, "never below 0");
        set_text(&host, &rt, &types, "brightness.level", "40%").unwrap();
        assert!((level(&host) - 0.4).abs() < 1e-9);
        set_text(&host, &rt, &types, "network.wifi", "false").unwrap();
        assert_eq!(host.get(&rt, "network.wifi").unwrap(), Value::Bool(false));
        let err = |p: &str, v: &str| set_text(&host, &rt, &types, p, v).unwrap_err();
        assert!(err("brightness.available", "true").contains("read-only"));
        assert!(err("brightness.level", "loud").contains("not a number"));
        assert!(err("brightness.nope", "1").contains("no field"));
        assert!(err("nothing.level", "1").contains("nothing is exported"));
    }

    /// The compositor services' uses design.md shows (and the reload
    /// event in both handler forms: its `failed` argument may be left
    /// out) check against the served schema, and alike against the bare
    /// builtin one, whose stubs declare the same (decisions.md, wave4-wm
    /// fixes).
    #[test]
    fn the_compositor_services_check_as_design_md_uses_them() {
        let src = "state reloads = 0\nstate broke = false\n\
            on wm.config_reloaded { reloads += 1 }\n\
            on wm.config_reloaded(failed) { broke = failed ?? false }\n\
            bar Top {\n  row {\n    for ws in workspaces.on(screen) { box { when ws.active { opacity: 1 } on click { ws.focus() } } }\n    \
            text windows.focused?.title ?? \"\"\n    \
            text join(\" \", wm.name, reloads, broke)\n  }\n}\n";
        for (which, schema) in [("served", schema()), ("builtin", Schema::builtin())] {
            let (map, _) = strand_compiler::source::SourceMap::single("bar.strand", src);
            let errors: Vec<_> = strand_compiler::compile_with(&map, schema)
                .diagnostics
                .into_iter()
                .filter(|d| d.is_error())
                .collect();
            assert!(errors.is_empty(), "{which}: {errors:#?}");
        }
    }

    /// `win.maximize()` and `win.fullscreen()` are actions next to the
    /// `maximized` and `fullscreen` fields (the same name as a field and
    /// an action): both check, against the served and the builtin schema,
    /// and the shared hover doc names both.
    #[test]
    fn window_state_actions_check_next_to_their_fields() {
        let src = "bar Top {\n  row {\n    for w in windows.all {\n      \
            box { when w.maximized { opacity: 1 } on click { w.maximize() } }\n      \
            box { when w.fullscreen { opacity: 1 } on click { w.fullscreen() } }\n      \
            box { when w.minimized { opacity: 1 } on click { w.minimize() } }\n    }\n    \
            box { on click { windows.focused?.fullscreen() } }\n  }\n}\n";
        for (which, schema) in [("served", schema()), ("builtin", Schema::builtin())] {
            let (map, _) = strand_compiler::source::SourceMap::single("bar.strand", src);
            let errors: Vec<_> = strand_compiler::compile_with(&map, schema)
                .diagnostics
                .into_iter()
                .filter(|d| d.is_error())
                .collect();
            assert!(errors.is_empty(), "{which}: {errors:#?}");
            let doc = schema
                .doc(&strand_compiler::schema::DocKey::Member(
                    "Window".into(),
                    "fullscreen".into(),
                ))
                .unwrap_or_default();
            assert!(
                doc.starts_with("It is fullscreen.") && doc.contains("`win.fullscreen()`"),
                "{which}: {doc:?}"
            );
        }
        // An action is not a value.
        let bad = "bar Top {\n  row {\n    for w in windows.all {\n      \
            box { when w.maximize { opacity: 1 } }\n    }\n  }\n}\n";
        let (map, _) = strand_compiler::source::SourceMap::single("bar.strand", bad);
        assert!(
            strand_compiler::compile_with(&map, schema())
                .diagnostics
                .iter()
                .any(|d| d.is_error()),
            "`w.maximize` without a call"
        );
    }

    /// The schema the binary checks against holds each linked service's
    /// declaration in place of its stub, documented, and each store's
    /// fields and events are exactly its schema record's.
    /// (M6, m6-integration-w1) The compiler's provisional stub of each
    /// service (what a tool holding only `Schema::builtin()` checks a
    /// config against) has the members its service schema declares:
    /// the same fields (name, type, writable), methods and actions, and
    /// events, so a config that uses one (`auth.message`) is not
    /// rejected there.
    #[test]
    fn each_builtin_stub_has_its_service_schemas_members() {
        let full = schema();
        let stub = Schema::builtin();
        let members = |s: &Schema, name: &str| {
            let rec = s.types.record(s.services[name]);
            let fields: Vec<(String, String, bool)> = rec
                .fields
                .iter()
                .map(|f| (f.name.clone(), s.types.show(&f.ty).to_string(), f.rw))
                .collect();
            let methods: Vec<String> = rec.methods.iter().map(|m| m.name.clone()).collect();
            let events: Vec<String> = rec.events.iter().map(|e| e.name.clone()).collect();
            (fields, methods, events)
        };
        let mut drift = Vec::new();
        for name in &stub.provisional {
            if !stub.services.contains_key(name) || !full.services.contains_key(name) {
                continue;
            }
            let (a, b) = (members(stub, name), members(full, name));
            if a != b {
                drift.push(format!("{name}:\n  stub   {a:?}\n  schema {b:?}"));
            }
        }
        assert!(drift.is_empty(), "{}", drift.join("\n"));
    }

    #[test]
    fn service_schemas_extend_the_builtin_one() {
        // Every text applies (a failing one would leave `schema()` on the
        // stubs, with only a log line).
        if let Err((i, errors)) = Schema::builtin_with(&strand_services::schemas()) {
            panic!(
                "service schema #{i} does not apply: {errors:#?}\n{}",
                strand_services::schemas()[i]
            );
        }
        let s = schema();
        assert!(s.undocumented().is_empty(), "{:#?}", s.undocumented());
        assert_ne!(s.fingerprint(), Schema::builtin().fingerprint());
        let rt = Runtime::new();
        let services = Services::new(&rt, Buses::none(), || {});
        for svc in Builtin::register(&services, &rt).all() {
            let name = svc.name();
            assert!(
                !s.provisional.contains(name),
                "{name} is still a stub: {:?}",
                s.provisional
            );
            let id = s.services[name];
            let rec = s.types.record(id);
            let fields: Vec<(String, String, bool)> = rec
                .fields
                .iter()
                .map(|f| (f.name.clone(), s.types.show(&f.ty).to_string(), f.rw))
                .collect();
            let store: Vec<(String, String, bool)> = svc
                .fields()
                .iter()
                .map(|f| (f.name.to_string(), (f.ty)(), f.rw))
                .collect();
            assert_eq!(fields, store, "{name}'s store and schema disagree");
            let events: Vec<&str> = rec.events.iter().map(|e| e.name.as_str()).collect();
            let store_events: Vec<&str> = svc.events().iter().map(|e| e.name).collect();
            assert_eq!(events, store_events, "{name}'s events");
            // The struct's `///` docs are the schema's (hover shows the
            // schema's): the two copies say the same.
            let member_doc = |m: &str| {
                s.doc(&strand_compiler::schema::DocKey::Member(
                    name.to_string(),
                    m.to_string(),
                ))
                .unwrap_or_default()
                .to_string()
            };
            for f in svc.fields() {
                assert_eq!(f.doc, member_doc(f.name), "{name}.{}'s docs", f.name);
            }
            for e in svc.events() {
                assert_eq!(e.doc, member_doc(e.name), "{name}.{}'s docs", e.name);
            }
            // A keyed list is keyed by what its record's schema `key` says.
            for (f, def) in svc.fields().iter().zip(&rec.fields) {
                if !f.keyed {
                    continue;
                }
                let key = match &def.ty {
                    strand_compiler::ty::Ty::List(item, _) => match &**item {
                        strand_compiler::ty::Ty::Record(r) => s.types.record(*r).key.clone(),
                        _ => None,
                    },
                    _ => None,
                };
                assert_eq!(
                    key,
                    f.key.map(|k| vec![k.to_string()]),
                    "{name}.{}'s key",
                    f.name
                );
            }
            // Every field of the provisional stub it replaced, with the
            // same `rw` mark and in the same order, and every event: configs
            // checked before M3 still check. A real service may add fields
            // (battery's `devices`; decisions.md, wave4-a2).
            let builtin = Schema::builtin();
            let stub = builtin.types.record(builtin.services[name]);
            let stub_fields: Vec<(&str, bool)> = stub
                .fields
                .iter()
                .map(|f| (f.name.as_str(), f.rw))
                .collect();
            let real_fields: Vec<(&str, bool)> = rec
                .fields
                .iter()
                .map(|f| (f.name.as_str(), f.rw))
                .filter(|f| stub_fields.iter().any(|s| s.0 == f.0))
                .collect();
            assert_eq!(stub_fields, real_fields, "{name} against its stub");
            let stub_events: Vec<&str> = stub.events.iter().map(|e| e.name.as_str()).collect();
            assert!(
                stub_events.iter().all(|e| events.contains(e)),
                "{name} keeps its stub's events {stub_events:?}"
            );
            assert_calls_match(s, &rt, &*svc);
        }
        // Every service schema text has its store among the builtins, and
        // every builtin has its text: a text without a store would check
        // and hover in the LSP but be served at defaults.
        let mut texts: Vec<String> = strand_services::schemas()
            .iter()
            .map(|t| {
                // The service a text declares: the stub it makes real.
                let builtin = Schema::builtin();
                let mut one = builtin.clone();
                one.extend(t).unwrap();
                let names: Vec<&String> = one
                    .services
                    .keys()
                    .filter(|n| builtin.provisional.contains(*n) && !one.provisional.contains(*n))
                    .collect();
                assert_eq!(names.len(), 1, "one service per text: {names:?}");
                names[0].clone()
            })
            .collect();
        texts.sort();
        let mut stores: Vec<String> = Builtin::register(&services, &rt)
            .all()
            .iter()
            .map(|s| s.name().to_string())
            .collect();
        stores.sort();
        assert_eq!(texts, stores, "schema texts and builtin stores");
        services.shutdown();
    }

    /// A store's actions, async methods and `fn` methods are its schema
    /// record's, by name and arity, and the actions and async methods of
    /// the item records it hands out or takes are those records': the
    /// language routes calls by these names, so a spelling that differs
    /// would type-check and then fail at run time.
    fn assert_calls_match(s: &Schema, rt: &Runtime, svc: &dyn strand_services::DynService) {
        use strand_compiler::ty::Ty;
        let name = svc.name();
        #[derive(Debug, PartialEq)]
        enum Kind {
            Action,
            Async,
            Fn,
        }
        let methods = |r: strand_compiler::ty::RecordId| -> Vec<(Kind, String, usize)> {
            let mut v: Vec<(Kind, String, usize)> = s
                .types
                .record(r)
                .methods
                .iter()
                .filter_map(|m| {
                    let sig = m.sigs.first()?;
                    let kind = if sig.action {
                        Kind::Action
                    } else if matches!(sig.ret, Ty::Async(_)) {
                        Kind::Async
                    } else {
                        Kind::Fn
                    };
                    Some((kind, m.name.clone(), sig.params.len()))
                })
                .collect();
            v.sort_by(|a, b| a.1.cmp(&b.1));
            v
        };
        let of = |sigs: &[strand_services::CallSig], item: Option<&str>| -> Vec<(String, usize)> {
            let mut v: Vec<(String, usize)> = sigs
                .iter()
                .filter(|c| c.item.as_deref() == item)
                .map(|c| (c.name.to_string(), c.arity))
                .collect();
            v.sort();
            v
        };
        let want = |m: &[(Kind, String, usize)], k: Kind| -> Vec<(String, usize)> {
            m.iter()
                .filter(|(kind, _, _)| *kind == k)
                .map(|(_, n, a)| (n.clone(), *a))
                .collect()
        };
        let (actions, calls) = (svc.action_sigs(), svc.method_sigs());
        let own = methods(s.services[name]);
        assert_eq!(
            of(&actions, None),
            want(&own, Kind::Action),
            "{name}'s actions"
        );
        assert_eq!(
            of(&calls, None),
            want(&own, Kind::Async),
            "{name}'s async methods"
        );
        for (f, arity) in want(&own, Kind::Fn) {
            let args = vec![strand_services::Data::Null; arity];
            assert!(
                svc.call(rt, &f, &args).is_some(),
                "{name} does not answer its `fn {f}`"
            );
        }
        for record in svc.item_records() {
            let r = s
                .types
                .find_record(&record)
                .unwrap_or_else(|| panic!("{name} hands out `{record}`, which the schema lacks"));
            let theirs = methods(r);
            assert_eq!(
                of(&actions, Some(&record)),
                want(&theirs, Kind::Action),
                "{name}: the actions of `{record}`"
            );
            assert_eq!(
                of(&calls, Some(&record)),
                want(&theirs, Kind::Async),
                "{name}: the async methods of `{record}`"
            );
        }
        // Every call it takes was compared above.
        let compared = |c: &strand_services::CallSig| match &c.item {
            None => true,
            Some(r) => svc.item_records().contains(r),
        };
        assert!(
            actions.iter().chain(&calls).all(compared),
            "{name}: {actions:?} {calls:?}"
        );
    }

    /// A config mounted against the real composite host, one monitor.
    struct Shell {
        rt: Runtime,
        real: Real,
        inst: Instance,
        scene: SceneMirror,
        now: Duration,
    }

    impl Shell {
        fn boot(src: &str) -> Shell {
            Shell::boot_on(src, Buses::none())
        }

        /// [`Shell::boot`] with the services on `buses`.
        fn boot_on(src: &str, buses: Buses) -> Shell {
            let mut map = SourceMap::new();
            map.add("svc.strand", src.to_string());
            let compiled = strand_compiler::compile_with(&map, schema());
            assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
            let program = Arc::new(lower::lower(&compiled.program, schema()));
            let rt = Runtime::new();
            let fallback = Rc::new(SchemaHost::new(&rt, &program.types, None));
            let screen = fallback.record(
                "Screen",
                &[
                    ("id", Value::text("Mock | DP-1 | Display")),
                    ("name", Value::text("DP-1")),
                ],
            );
            fallback
                .set(&rt, "screens.all", Value::list(vec![screen]))
                .unwrap();
            let real = Real::start(&rt, &program.types, buses, fallback, || {});
            let inst = Instance::new(&rt, program, real.host.clone(), Storage::none());
            let mut shell = Shell {
                rt,
                real,
                inst,
                scene: SceneMirror::new(),
                now: Duration::ZERO,
            };
            shell.tick(Duration::ZERO);
            shell
        }

        /// Pump the services, then advance the logic clock by `by`.
        fn tick(&mut self, by: Duration) {
            self.real.services.pump(&self.rt);
            self.now += by;
            let u = self.inst.tick(self.now);
            assert!(u.errors.is_empty(), "{:?}", u.errors);
            self.scene.apply(&u.diff).unwrap();
        }

        /// Pump and flush until `done`, at most 5 s of real time.
        fn until(&mut self, what: &str, done: impl Fn(&Shell) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done(self) {
                assert!(Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(10));
                self.tick(Duration::ZERO);
            }
        }

        fn set(&mut self, name: &str, v: bool) {
            self.inst
                .set(&format!("svc.{name}"), Value::Bool(v))
                .unwrap();
            self.tick(Duration::ZERO);
        }
    }

    impl Drop for Shell {
        fn drop(&mut self) {
            self.inst.shutdown();
            self.real.shutdown(&self.rt);
        }
    }

    /// (M6, owner-testing gap 20) A builtin service that cannot run (its
    /// system bus does not answer: each body fails and backs off) is no
    /// runtime fault (spec.md 12): `session.lock()`, `network.wifi =
    /// true` in a click handler leave the component running (`tick`
    /// asserts no fault), the action comes back as `session.failed`, the
    /// write is dropped, and `available` says so first. Before, the
    /// action faulted the bar (`on click: 'session' is not running`).
    #[test]
    fn a_service_that_cannot_run_fails_as_values_not_faults() {
        let gone = strand_services::Bus::Address("unix:path=/nonexistent/strand-test-bus".into());
        let src = "state heard = \"\"\n\
                   bar B {\n\
                   \x20 on session.failed(a, e) { heard = heard + a + \";\" }\n\
                   \x20 text (session.available || network.available || bluetooth.available) ? \"up\" : \"down\"\n\
                   \x20 text heard\n\
                   \x20 text network.wifi ? \"on\" : \"off\"\n\
                   \x20 box {\n\
                   \x20   on click {\n\
                   \x20     session.lock()\n\
                   \x20     network.wifi = true\n\
                   \x20   }\n\
                   \x20 }\n\
                   }\n";
        let mut shell = Shell::boot_on(
            src,
            Buses {
                session: gone.clone(),
                system: gone,
            },
        );
        shell.until("the services' failed first runs", |s| {
            let b = &s.real.builtin;
            b.session.starts() > 0
                && b.network.starts() > 0
                && !b.session.running()
                && !b.network.running()
        });
        assert_eq!(shell.scene.texts(), ["down", "", "off"]);
        let boxes = shell.inst.nodes_handling("click");
        assert_eq!(boxes.len(), 1);
        shell.inst.event(boxes[0], "click", Vec::new());
        shell.tick(Duration::ZERO);
        shell.until("the refusal heard", |s| s.scene.texts()[1] == "lock;");
        assert_eq!(shell.scene.texts(), ["down", "lock;", "off"]);
    }

    /// (m6-service-gaps, spec.md 8.1) A program that reads
    /// `clipboard.history` anywhere, here only in a closed popup (through
    /// a `let`), holds the service and the field from its top level, so
    /// copies made while the popup is closed are recorded; one reading
    /// only `clipboard.text` in a closed popup holds nothing, and a
    /// config's own `service clipboard` shadows the rule.
    #[test]
    fn a_closed_popup_reading_clipboard_history_holds_it_from_the_top_level() {
        let dir = tempfile::tempdir().unwrap();
        strand_services::clipboard::configure(Some(strand_services::clipboard::Config {
            target: strand_services::wm::WaylandTarget::Socket(dir.path().join("no-display")),
            state_dir: dir.path().to_path_buf(),
        }));
        let history = <strand_services::clipboard::Clipboard as strand_services::Store>::FIELDS
            .iter()
            .position(|f| f.name == "history")
            .unwrap();
        let shell = Shell::boot(
            "export state open = false\n\
             let recent = clipboard.history\n\
             bar B {\n\
             \x20 text \"x\"\n\
             \x20 popup { open: <-> open; for e in recent { text e.text } }\n\
             }\n",
        );
        let b = shell.real.builtin.clone();
        assert_eq!(b.clipboard.readers(), 1, "held while the popup is closed");
        assert_eq!(b.clipboard.field_readers(history), 1);
        drop(shell);
        let shell = Shell::boot(
            "export state open = false\n\
             bar B {\n\
             \x20 text \"x\"\n\
             \x20 popup { open: <-> open; text clipboard.text }\n\
             }\n",
        );
        let b = shell.real.builtin.clone();
        assert_eq!(b.clipboard.readers(), 0, "the text is read only when shown");
        assert_eq!(b.clipboard.field_readers(history), 0);
        drop(shell);
        let shell = Shell::boot(
            "service clipboard from file \"/nonexistent/c.json\" { history: int }\n\
             export state open = false\n\
             bar B {\n\
             \x20 text \"x\"\n\
             \x20 popup { open: <-> open; text join(\" \", clipboard.history) }\n\
             }\n",
        );
        let b = shell.real.builtin.clone();
        assert_eq!(b.clipboard.readers(), 0, "a config's service shadows it");
        assert_eq!(b.clipboard.field_readers(history), 0);
        drop(shell);
        strand_services::clipboard::configure(None);
    }

    /// Only the services a config reads start (the compiler collects the
    /// service paths each scope reads; a scope acquires those): a config
    /// that reads `cpu` starts it and nothing else, and its binding shows
    /// the store's value, converted, through the composite host.
    #[test]
    fn only_the_services_a_config_reads_start() {
        let mut shell = Shell::boot("bar B { text pct(cpu.usage) }\n");
        let b = shell.real.builtin.clone();
        assert_eq!(b.cpu.starts(), 1);
        assert_eq!(b.cpu.readers(), 1);
        assert_eq!(b.memory.starts(), 0, "never read, never started");
        assert_eq!(b.system.starts(), 0, "never read, never started");
        assert!(b.cpu.running());
        shell.until("cpu's first read", |s| s.real.builtin.cpu.reports() > 0);
        let usage = b.cpu.cells().snapshot(&shell.rt).unwrap().usage;
        let shown = format!("{}%", (usage * 100.0).round());
        assert_eq!(shell.scene.texts(), [shown]);
        // The same value through the host as the VM reads it.
        let v = shell
            .rt
            .untrack(|rt| shell.real.host.read(rt, "cpu", "usage"))
            .unwrap();
        assert_eq!(v, Value::float(usage));
        // Names no service crate serves yet answer from the fallback.
        let v = shell
            .rt
            .untrack(|rt| shell.real.host.read(rt, "battery", "percent"))
            .unwrap();
        assert_eq!(v, Value::float(0.0));
        assert!(
            shell
                .real
                .host
                .sources(&shell.rt, "cpu", Some("usage"))
                .len()
                >= 2,
            "the value memo and the store's cell"
        );
    }

    /// A surface's `open` binding is read by the scope around the
    /// surface, not by its content: design.md's toasts panel (`open:
    /// shown.len > 0` over `notifications.popups`) holds `notifications`
    /// while it is closed, or no notification could ever open it.
    #[test]
    fn a_closed_surface_holds_what_its_open_binding_reads() {
        let shell = Shell::boot(
            "let shown = memory.usage\npanel P {\n  open: shown > 2\n  text \"x\"\n}\n",
        );
        let memory = shell.real.builtin.memory.clone();
        assert_eq!(memory.starts(), 1, "the closed panel's `open` reads it");
        assert_eq!(memory.readers(), 1);
    }

    /// A popup reading `memory` starts it when it opens; closing it stops
    /// it 5 s later on the logic clock, and reopening inside those 5 s
    /// cancels the stop without a restart.
    #[test]
    fn a_popup_starts_its_service_and_stops_it_five_seconds_after_closing() {
        let mut shell = Shell::boot(
            "export state p = false\nbar B {\n  text \"x\"\n  popup { open: <-> p; text pct(memory.usage) }\n}\n",
        );
        let memory = shell.real.builtin.memory.clone();
        assert_eq!(memory.starts(), 0, "the popup is closed");
        shell.set("p", true);
        assert_eq!((memory.starts(), memory.readers()), (1, 1));
        assert!(memory.running());
        shell.until("memory's first read", |s| {
            s.scene.texts().iter().any(|t| t != "x" && t != "0%")
        });
        shell.set("p", false);
        assert_eq!(memory.readers(), 0);
        shell.tick(STOP_GRACE - Duration::from_millis(100));
        assert!(memory.running(), "still inside the grace");
        // Reopened inside the grace: the same run carries on.
        shell.set("p", true);
        shell.tick(Duration::from_secs(10));
        assert!(memory.running());
        assert_eq!((memory.starts(), memory.stops()), (1, 0));
        // Closed for good: stopped once the grace is over.
        shell.set("p", false);
        shell.tick(STOP_GRACE - Duration::from_millis(1));
        assert!(memory.running());
        shell.tick(Duration::from_millis(2));
        assert!(!memory.running(), "stopped 5 s after its last reader left");
        assert_eq!((memory.starts(), memory.stops()), (1, 1));
        // Opened again later: a fresh start.
        shell.set("p", true);
        assert_eq!(memory.starts(), 2);
    }

    /// (history) A `graph` in a popup on the real `cpu` service, end to
    /// end through the composite host: the config keeps `cpu` (at the
    /// 3 s hidden period of a 60 s history), so it runs and samples
    /// while the popup is closed and past the 5 s grace; the graph,
    /// mounted again, gets the values it showed and the coarse one taken
    /// while closed (a ramp). A reload without the graph lets `cpu` go,
    /// and it stops 5 s later as after its last reader.
    #[test]
    fn a_graph_mounted_again_shows_the_cpu_history_kept_while_closed() {
        let graph = "export state p = true\nbar B {\n  text \"x\"\n  popup { open: <-> p; graph cpu.usage { history: 60s } }\n}\n";
        let build = |prev: Option<&strand_compiler::reconcile::Build>, text: &str| {
            let mut map = SourceMap::new();
            map.add("svc.strand", text.to_string());
            strand_compiler::reconcile::Build::compile_with(prev, map, schema())
                .unwrap_or_else(|d| panic!("{d:#?}"))
        };
        let first = build(None, graph);
        let rt = Runtime::new();
        let fallback = Rc::new(SchemaHost::new(&rt, &first.program.types, None));
        let screen = fallback.record(
            "Screen",
            &[
                ("id", Value::text("Mock | DP-1 | Display")),
                ("name", Value::text("DP-1")),
            ],
        );
        fallback
            .set(&rt, "screens.all", Value::list(vec![screen]))
            .unwrap();
        let real = Real::start(&rt, &first.program.types, Buses::none(), fallback, || {});
        let mut inst = Instance::from_build(&rt, &first, real.host.clone(), Storage::none());
        let mut scene = SceneMirror::new();
        let mut now = Duration::ZERO;
        let mut step = |inst: &Instance, scene: &mut SceneMirror, by: Duration| {
            real.services.pump(&rt);
            now += by;
            let u = inst.tick(now);
            assert!(u.errors.is_empty(), "{:?}", u.errors);
            scene.apply(&u.diff).unwrap();
        };
        let until = |inst: &Instance,
                     scene: &mut SceneMirror,
                     step: &mut dyn FnMut(&Instance, &mut SceneMirror, Duration),
                     what: &str,
                     done: &dyn Fn(&Instance) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(6);
            while !done(inst) {
                assert!(Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(10));
                step(inst, scene, Duration::from_millis(10));
            }
        };
        let points = |inst: &Instance| inst.histories().first().map_or(0, |h| h.points);
        step(&inst, &mut scene, Duration::ZERO);
        let cpu = real.builtin.cpu.clone();
        assert_eq!(cpu.kept(), Some(Duration::from_secs(3)));
        assert_eq!(cpu.readers(), 1, "the popup is open");
        until(&inst, &mut scene, &mut step, "two shown samples", &|i| {
            points(i) >= 2
        });
        // Closed: nothing reads cpu, yet it runs past the grace, kept.
        inst.set("svc.p", Value::Bool(false)).unwrap();
        step(&inst, &mut scene, Duration::ZERO);
        assert!(scene.of_kind(strand_scene::NodeKind::Graph).is_empty());
        assert_eq!(cpu.readers(), 0);
        let shown = points(&inst);
        step(&inst, &mut scene, STOP_GRACE + Duration::from_secs(1));
        assert!(cpu.running(), "kept past the grace");
        until(&inst, &mut scene, &mut step, "a hidden sample", &|i| {
            points(i) > shown
        });
        // Open again: the graph mounts onto all of it.
        inst.set("svc.p", Value::Bool(true)).unwrap();
        step(&inst, &mut scene, Duration::ZERO);
        let g = scene.of_kind(strand_scene::NodeKind::Graph)[0];
        let Some(strand_scene::PropValue::Backlog(b)) = scene.prop(g, strand_scene::Prop::Backlog)
        else {
            panic!("no backlog on {g:?}");
        };
        assert!(b.points.len() > shown, "{b:?}");
        assert_eq!(b.points[0].ramp, 0.0, "shown: a step");
        assert!(b.points[0].age > 6.0, "{b:?}");
        let hidden = &b.points[shown];
        assert!(hidden.ramp > 0.0 && hidden.ramp <= 3.0, "{b:?}");
        // A reload without the graph lets cpu go: it stops after the grace.
        let second = build(
            Some(&first),
            "export state p = true\nbar B { text \"x\" }\n",
        );
        inst.reload(&second);
        step(&inst, &mut scene, Duration::ZERO);
        assert!(inst.histories().is_empty());
        assert_eq!(cpu.kept(), None);
        step(&inst, &mut scene, STOP_GRACE + Duration::from_secs(1));
        assert!(!cpu.running(), "let go: stopped after the grace");
        inst.shutdown();
        real.shutdown(&rt);
    }

    /// design.md's launcher, unchanged, on the real `apps` service:
    /// closed, it starts nothing and searches nothing; open, it lists the
    /// apps (an empty query lists them all), a query narrows them with
    /// its matched ranges marked on the name, and activating a row
    /// launches the app (detached) and closes the launcher. A query typed
    /// while it is closed is never searched.
    /// The apps tests set the process's apps configuration: one at a
    /// time.
    static APPS_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_design_launcher_searches_real_apps_only_while_open() {
        use strand_scene::Prop;
        let _serial = APPS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("strand-launcher-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("applications");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = root.join("launched");
        let script = root.join("run.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" > {0}.tmp\nmv {0}.tmp {0}\n",
                marker.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let entry = |id: &str, name: &str| {
            std::fs::write(
                dir.join(format!("{id}.desktop")),
                format!(
                    "[Desktop Entry]\nType=Application\nName={name}\nComment={name} app\nExec={} {id}\n",
                    script.display()
                ),
            )
            .unwrap();
        };
        entry("editor", "Text Editor");
        entry("viewer", "Image Viewer");
        strand_services::apps::set_config(Some(strand_services::apps::Config {
            dirs: vec![dir.clone()],
            desktops: Vec::new(),
            state: None,
        }));
        let launcher = include_str!("../../../strand-compiler/tests/fixtures/launcher.strand");
        let mut shell = Shell::boot(launcher);
        let apps = shell.real.builtin.apps.clone();
        let searched = strand_services::apps::searches();
        assert_eq!(apps.starts(), 0, "a closed launcher starts nothing");
        shell.set("open", true);
        assert_eq!(apps.starts(), 1);
        shell.until("every app listed", |s| {
            let t = s.scene.texts();
            t.contains(&"Text Editor".to_string()) && t.contains(&"Image Viewer".to_string())
        });
        assert!(strand_services::apps::searches() > searched);
        shell
            .inst
            .set_value("svc", "query", Value::text("edit"))
            .unwrap();
        shell.until("the query narrows the list", |s| {
            let t = s.scene.texts();
            t.contains(&"Text Editor".to_string()) && !t.contains(&"Image Viewer".to_string())
        });
        let name = shell.scene.find_text("Text Editor").unwrap();
        let marks = format!("{:?}", shell.scene.prop(name, Prop::Marks));
        assert!(
            marks.contains('5') && marks.contains('9'),
            "Edit marked: {marks}"
        );
        // Activating the row launches the app and closes the launcher.
        let rows = shell.inst.nodes_handling("activate");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(shell.inst.event(rows[0], "activate", Vec::new()));
        shell.tick(Duration::ZERO);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "editor");
        assert_eq!(shell.inst.get("svc.open").unwrap(), Value::Bool(false));
        // Closed: a new query is not searched.
        let before = strand_services::apps::searches();
        shell
            .inst
            .set_value("svc", "query", Value::text("view"))
            .unwrap();
        shell.tick(Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(100));
        shell.tick(Duration::ZERO);
        assert_eq!(
            strand_services::apps::searches(),
            before,
            "closed: no search"
        );
        // Opened again (`on show` clears the query): it searches once
        // more and lists every app.
        shell.set("open", true);
        shell.until("listed again", |s| {
            let t = s.scene.texts();
            t.contains(&"Text Editor".to_string()) && t.contains(&"Image Viewer".to_string())
        });
        assert!(strand_services::apps::searches() > before);

        // Open, the hits follow the app list and the icons it resolves
        // (the search memo reads the service's state, not just `query`).
        let sources = |s: &Shell| -> Vec<String> {
            s.scene
                .of_kind(strand_scene::NodeKind::Image)
                .into_iter()
                .map(|n| format!("{:?}", s.scene.prop(n, Prop::Source)))
                .collect()
        };
        entry("player", "Music Player");
        strand_services::apps::changed();
        shell.until("a new app is listed", |s| {
            s.scene.texts().contains(&"Music Player".to_string())
        });
        std::fs::remove_file(dir.join("viewer.desktop")).unwrap();
        strand_services::apps::changed();
        shell.until("a removed app is gone", |s| {
            !s.scene.texts().contains(&"Image Viewer".to_string())
        });
        assert!(shell.scene.texts().contains(&"Text Editor".to_string()));
        // An icon that appears (as a theme switch makes a name resolve):
        // the shown hit's icon changes from the fallback to it.
        let icon = root.join("editor-icon.png");
        std::fs::write(
            dir.join("editor.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName=Text Editor\nIcon={}\nExec={} editor\n",
                icon.display(),
                script.display()
            ),
        )
        .unwrap();
        strand_services::apps::changed();
        shell.until("the fallback icon", |s| {
            sources(s)
                .iter()
                .filter(|x| x.contains(strand_services::apps::FALLBACK_ICON))
                .count()
                == 2
        });
        std::fs::write(&icon, b"png").unwrap();
        strand_services::apps::changed();
        shell.until("the hit shows the icon that appeared", |s| {
            let icon = icon.display().to_string();
            sources(s).iter().any(|x| x.contains(&icon))
        });
        drop(shell);
        strand_services::apps::set_config(None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The documented limit of the async gate (decisions.md, wave4-a3
    /// "an async call waits for a reader"): it holds a call back while
    /// nothing reads the *service*, so with a dock showing `apps.all` a
    /// launcher opened once and closed searches a new query (once,
    /// nothing shown);
    /// with the dock gone too it is not.
    #[test]
    fn a_closed_launcher_searches_only_while_another_surface_reads_apps() {
        let _serial = APPS_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("strand-dock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("applications");
        std::fs::create_dir_all(&dir).unwrap();
        for (id, name) in [("editor", "Text Editor"), ("viewer", "Image Viewer")] {
            std::fs::write(
                dir.join(format!("{id}.desktop")),
                format!("[Desktop Entry]\nType=Application\nName={name}\nExec=true\n"),
            )
            .unwrap();
        }
        strand_services::apps::set_config(Some(strand_services::apps::Config {
            dirs: vec![dir.clone()],
            desktops: Vec::new(),
            state: None,
        }));
        let launcher = include_str!("../../../strand-compiler/tests/fixtures/launcher.strand");
        let src = format!(
            "{launcher}\nexport state dock = true\npanel Dock {{ open: <-> dock; row {{ for a in apps.all {{ text a.name }} }} }}\n"
        );
        let mut shell = Shell::boot(&src);
        shell.until("the dock lists the apps", |s| {
            s.scene.texts().contains(&"Text Editor".to_string())
        });
        // Opened once (its search created) and closed again.
        shell.set("open", true);
        shell.until("the launcher lists the apps", |s| {
            s.scene
                .texts()
                .iter()
                .filter(|t| *t == "Text Editor")
                .count()
                == 2
        });
        shell.set("open", false);
        // The dock reads `apps`: the closed launcher's query is searched.
        let before = strand_services::apps::searches();
        shell
            .inst
            .set_value("svc", "query", Value::text("view"))
            .unwrap();
        shell.until("searched", |_| strand_services::apps::searches() > before);
        shell.tick(Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(100));
        shell.tick(Duration::ZERO);
        assert_eq!(
            strand_services::apps::searches(),
            before + 1,
            "one search per query change"
        );
        assert_eq!(shell.inst.get("svc.open").unwrap(), Value::Bool(false));
        // The dock hidden too: nothing reads `apps`, no search.
        shell.set("dock", false);
        let before = strand_services::apps::searches();
        shell
            .inst
            .set_value("svc", "query", Value::text("edit"))
            .unwrap();
        shell.tick(Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(100));
        shell.tick(Duration::ZERO);
        assert_eq!(
            strand_services::apps::searches(),
            before,
            "nothing reads apps"
        );
        drop(shell);
        strand_services::apps::set_config(None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A hidden surface stops a visible-only stream at once (cpu samples
    /// only while a reader is visible), long before the service stops.
    #[test]
    fn a_hidden_surface_stops_a_visible_only_stream() {
        let mut shell =
            Shell::boot("export state o = true\npanel P { open: <-> o; text pct(cpu.usage) }\n");
        let cpu = shell.real.builtin.cpu.clone();
        assert_eq!(cpu.readers(), 1);
        // Visible: it samples once a second.
        let first = cpu.reports();
        shell.until("a second sample", |s| {
            s.real.builtin.cpu.reports() >= first + 2
        });
        shell.set("o", false);
        assert_eq!(cpu.readers(), 0);
        assert!(cpu.running(), "the stop waits for the grace");
        // What was in flight when it was hidden.
        std::thread::sleep(Duration::from_millis(200));
        shell.tick(Duration::ZERO);
        let hidden = cpu.reports();
        // More than two sampling periods.
        std::thread::sleep(strand_services::cpu::PERIOD * 5 / 2);
        shell.tick(Duration::ZERO);
        assert_eq!(cpu.reports(), hidden, "no samples while hidden");
        // Shown again: sampling resumes.
        shell.set("o", true);
        shell.until("sampling again", |s| s.real.builtin.cpu.reports() > hidden);
        assert_eq!((cpu.starts(), cpu.stops()), (1, 0));
    }

    /// A store with a keyed list, an `rw` field, an event, item actions
    /// and an async method, through `StoreHost` and `Composite` as the VM
    /// drives them.
    mod tally {
        use std::future::Future;
        use std::task::{Context, Poll, Wake, Waker};

        use strand_compiler::vm::host::{ActionCtx, ActionTarget, PathSeg};
        use strand_core::Runtime;
        use strand_services::{Call, Cx, Event, Msg, ServiceError, Store, service};

        use super::*;

        pub const SCHEMA: &str = "
/// An item of the tally.
record TallyItem key id {
  /// Its key.
  id: int
  /// Its name.
  name: text
  /// Its own level, written by the shell.
  level: float rw
  /// Takes it off the tally.
  action remove()
}

/// A tally of items.
service tally {
  /// A level, written by the shell.
  level: float rw
  /// The items, keyed by `id`.
  items: [TallyItem]
  /// Adds an item.
  action add(name: text)
  /// Its text, back.
  fn echo(text: text) -> Async<text>
  /// An item was taken off.
  event removed(item: TallyItem)
}
";

        #[derive(strand_services::Data, Clone, Debug, Default, PartialEq)]
        #[data(name = "TallyItem", key = id)]
        pub struct Item {
            pub id: i64,
            pub name: String,
            pub level: f64,
        }

        #[derive(Call, Debug)]
        pub enum TallyAction {
            Add(String),
            Remove { item: Item },
        }

        #[derive(Call, Debug)]
        pub enum TallyCall {
            Echo { text: String },
        }

        /// See the schema.
        #[service(name = "tally", schema = SCHEMA, action = TallyAction, call = TallyCall)]
        #[derive(Store, Clone, Debug, Default, PartialEq)]
        pub struct Tally {
            /// A level.
            #[store(rw)]
            pub level: f64,
            /// The items.
            #[store(keyed)]
            pub items: Vec<Item>,
            /// An item was taken off.
            pub removed: Event<Item>,
        }

        /// The item writes the tally saw.
        static TALLY_WRITES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

        impl Tally {
            async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
                cx.ready();
                while let Some(m) = cx.recv().await {
                    match m {
                        // An item's level: applied as written, the write
                        // logged with the item's key.
                        Msg::Write(w) if w.key.is_some() => {
                            let item: Item = w.field_value().map_err(ServiceError::from)?;
                            TALLY_WRITES.lock().unwrap().push(format!(
                                "{} {:?}{} {:?}",
                                w.field,
                                w.key,
                                w.path.iter().map(ToString::to_string).collect::<String>(),
                                w.value
                            ));
                            cx.report(&w, |s| {
                                if let Some(i) = s.items.iter_mut().find(|i| i.id == item.id) {
                                    *i = item;
                                }
                            });
                        }
                        // Settles on half what was written.
                        Msg::Write(w) => {
                            let v: f64 = w.value().unwrap_or(0.0);
                            cx.report(&w, |s| s.level = v / 2.0);
                        }
                        Msg::Action(TallyAction::Add(name)) => {
                            cx.update(|s| {
                                let id = s.items.len() as i64 + 1;
                                s.items.push(Item {
                                    id,
                                    name,
                                    level: 0.0,
                                });
                            });
                        }
                        Msg::Action(TallyAction::Remove { item }) => {
                            cx.update(|s| s.items.retain(|i| i.id != item.id));
                            cx.emit(TallyEvent::Removed(item));
                        }
                        Msg::Call(TallyCall::Echo { text }, reply) => {
                            reply.send::<_, String>(Ok(text));
                        }
                        Msg::Visible(_) | Msg::Watch { .. } | Msg::Keep(_) => {}
                    }
                }
                Ok(())
            }
        }

        pub const WIFI_SCHEMA: &str = "
/// Wi-Fi.
service wifi {
  /// The network joined.
  ssid: text
  /// The networks in range: scanned only while a visible reader reads
  /// them.
  networks: [text]
}
";

        /// What the wifi service's body saw, in order.
        static WIFI_LOG: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        /// The tests reading `WIFI_LOG` run one at a time.
        static WIFI_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

        /// A service with a stream field.
        #[service(name = "wifi", schema = WIFI_SCHEMA)]
        #[derive(Store, Clone, Debug, Default, PartialEq)]
        pub struct Wifi {
            /// The network joined.
            pub ssid: String,
            /// The networks in range: scanned only while a visible reader
            /// reads them.
            #[store(stream)]
            pub networks: Vec<String>,
        }

        impl Wifi {
            async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
                let log = |s: String| WIFI_LOG.lock().unwrap().push(s);
                log(format!("start scanning={}", cx.watched("networks")));
                cx.update(|s| s.ssid = "home".into());
                cx.ready();
                while let Some(m) = cx.recv().await {
                    if let Msg::Watch { field, on } = m {
                        log(format!("scanning={on} ({field})"));
                        if on {
                            cx.update(|s| s.networks = vec!["home".into(), "cafe".into()]);
                        }
                    }
                }
                Ok(())
            }
        }

        pub const SHELF_SCHEMA: &str = "
/// A shelf: its boot report carries a keyed list and a flag.
service shelf {
  /// Whether it is dark.
  dark: bool
  /// The items, keyed by `id`.
  items: [TallyItem]
  /// Adds an item.
  action add(name: text)
}
";

        #[derive(Call, Debug)]
        pub enum ShelfAction {
            Add(String),
        }

        /// A service whose first report (before `ready()`) holds a keyed
        /// list and a plain field, as a real service's boot read does.
        #[service(name = "shelf", schema = SHELF_SCHEMA, action = ShelfAction)]
        #[derive(Store, Clone, Debug, Default, PartialEq)]
        pub struct Shelf {
            /// Whether it is dark.
            pub dark: bool,
            /// The items.
            #[store(keyed)]
            pub items: Vec<Item>,
        }

        /// Lets the shelf's boot read finish (after the shell has run).
        static SHELF_GO: tokio::sync::Notify = tokio::sync::Notify::const_new();

        impl Shelf {
            async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
                SHELF_GO.notified().await;
                cx.update(|s| {
                    s.dark = true;
                    s.items = vec![
                        Item {
                            id: 1,
                            name: "a".into(),
                            level: 0.0,
                        },
                        Item {
                            id: 2,
                            name: "b".into(),
                            level: 0.0,
                        },
                    ];
                });
                cx.ready();
                while let Some(m) = cx.recv().await {
                    if let Msg::Action(ShelfAction::Add(name)) = m {
                        cx.update(|s| {
                            let id = s.items.len() as i64 + 1;
                            s.items.push(Item {
                                id,
                                name,
                                level: 0.0,
                            });
                            s.dark = !s.dark;
                        });
                    }
                }
                Ok(())
            }
        }

        /// A service's boot report reaches `on change` as a baseline, not
        /// a change, through the store host: neither a keyed field's list
        /// nor a plain field fires (`on change` never fires at boot, nor
        /// when a service starts late); a later change fires both.
        #[test]
        fn on_change_skips_a_services_boot_report() {
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let shelf = services.register::<Shelf>(&rt);
            let schema_text = format!("{SCHEMA}{SHELF_SCHEMA}");
            let (mut inst, mut scene) = mount(
                &rt,
                &schema_text,
                "export state items = 0\nexport state darks = 0\non change shelf.items { items = items + 1 }\non change shelf.dark { darks = darks + 1 }\nbar B { text join(\" \", shelf.items.count(i => true), shelf.dark) }\n",
                shelf.dynamic(),
            );
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            // The shell runs (its handlers have their baselines) before the
            // service's boot read lands, as with a slow D-Bus service.
            for _ in 0..3 {
                step(&mut inst);
            }
            SHELF_GO.notify_one();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"2 true".to_string()) {
                assert!(Instant::now() < deadline, "the boot report never landed");
                std::thread::sleep(Duration::from_millis(5));
            }
            for _ in 0..5 {
                step(&mut inst);
            }
            assert_eq!(inst.value_of("svc", "items").unwrap(), Value::int(0));
            assert_eq!(inst.value_of("svc", "darks").unwrap(), Value::int(0));
            shelf
                .dynamic()
                .action(
                    &rt,
                    "add",
                    None,
                    &[strand_services::Data::Text("c".into())],
                    Default::default(),
                )
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"3 false".to_string()) {
                assert!(Instant::now() < deadline, "the change never landed");
                std::thread::sleep(Duration::from_millis(5));
            }
            step(&mut inst);
            assert_eq!(inst.value_of("svc", "items").unwrap(), Value::int(1));
            assert_eq!(inst.value_of("svc", "darks").unwrap(), Value::int(1));
            inst.shutdown();
            services.shutdown();
        }

        /// `src` mounted on one monitor against a composite host serving
        /// `svc` (declared by `schema_text`) and the schema's defaults.
        fn mount(
            rt: &Runtime,
            schema_text: &str,
            src: &str,
            svc: Rc<dyn strand_services::DynService>,
        ) -> (Instance, SceneMirror) {
            let schema = Schema::builtin_with(&[schema_text]).unwrap();
            let mut map = SourceMap::new();
            map.add("svc.strand", src.to_string());
            let compiled = strand_compiler::compile_with(&map, &schema);
            assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
            let program = Arc::new(lower::lower(&compiled.program, &schema));
            let types = Rc::new(program.types.clone());
            let fallback = Rc::new(SchemaHost::new(rt, &types, None));
            let screen = fallback.record(
                "Screen",
                &[
                    ("id", Value::text("Mock | DP-1 | Display")),
                    ("name", Value::text("DP-1")),
                ],
            );
            fallback
                .set(rt, "screens.all", Value::list(vec![screen]))
                .unwrap();
            let name = svc.name();
            let items = svc.item_records();
            let store = Rc::new(StoreHost::new(rt, svc, types.clone()));
            let mut host = Composite::new(fallback, types);
            host.add(store, &[name], &items);
            let inst = Instance::new(rt, program, Rc::new(host), Storage::none());
            (inst, SceneMirror::new())
        }

        /// An async method called anywhere reaches the service: inside a
        /// larger expression of a binding (`tally.echo(x) ?? "none"`, the
        /// scope's load of that call) and awaited in a handler.
        #[test]
        fn async_calls_anywhere_reach_the_service() {
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let tally = services.register::<Tally>(&rt);
            let (mut inst, mut scene) = mount(
                &rt,
                SCHEMA,
                "export state q = \"hi\"\nexport state got = \"\"\nbar B {\n  text tally.echo(q) ?? \"none\"\n  text join(\"+\", tally.echo(\"a\") ?? \"\", \"b\")\n  text got\n}\nafter 1s { got = await tally.echo(\"late\") }\n",
                tally.dynamic(),
            );
            let mut now = Duration::ZERO;
            let mut step = |inst: &mut Instance, by: Duration| {
                services.pump(&rt);
                now += by;
                let u = inst.tick(now);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            let wait = |what: &str,
                        step: &mut dyn FnMut(&mut Instance, Duration) -> Vec<String>,
                        inst: &mut Instance,
                        want: &[&str]| {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let texts = step(inst, Duration::ZERO);
                    if want.iter().all(|w| texts.iter().any(|t| t == w)) {
                        return;
                    }
                    assert!(Instant::now() < deadline, "never: {what} ({texts:?})");
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            wait(
                "the answers in bindings",
                &mut step,
                &mut inst,
                &["hi", "a+b"],
            );
            // The argument changes: one new fetch, its answer shown.
            inst.set("svc.q", Value::text("there")).unwrap();
            wait("the new answer", &mut step, &mut inst, &["there"]);
            // A handler awaits the call.
            step(&mut inst, Duration::from_secs(1));
            wait("the handler's answer", &mut step, &mut inst, &["late"]);
            inst.shutdown();
            services.shutdown();
        }

        /// `for i in tally.items { … i.level = … }`: an `rw` field of an
        /// item of a keyed list is written through the service by the
        /// item's key, from a handler and from a slider's `<->`. The row
        /// shows each write at once; the service's answer to an earlier
        /// write, arriving after a later one, is ignored as its echo, so
        /// the item never goes back to the earlier value.
        #[test]
        fn an_items_rw_field_is_written_by_its_key() {
            TALLY_WRITES.lock().unwrap().clear();
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let tally = services.register::<Tally>(&rt);
            let (mut inst, mut scene) = mount(
                &rt,
                SCHEMA,
                "bar B {\n  for i in tally.items {\n    row {\n      text join(\" \", i.name, i.level)\n      box { on click { i.level = 0.3; i.level = 0.6 } }\n      slider { value: <-> i.level }\n    }\n  }\n}\n",
                tally.dynamic(),
            );
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.clone()
            };
            for name in ["a", "b"] {
                tally
                    .dynamic()
                    .action(
                        &rt,
                        "add",
                        None,
                        &[strand_services::Data::Text(name.into())],
                        Default::default(),
                    )
                    .unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).texts().contains(&"b 0".to_string()) {
                assert!(Instant::now() < deadline, "the items never showed");
                std::thread::sleep(Duration::from_millis(5));
            }
            // Every level the second item takes, as the store applies it.
            let levels: Rc<std::cell::RefCell<Vec<strand_services::Data>>> = Rc::default();
            let l = levels.clone();
            tally.dynamic().observe(Box::new(move |_, a| {
                if let strand_services::Applied::Keyed { diffs, .. } = a {
                    for d in diffs {
                        if let strand_core::VecDiff::Update { value, .. } = d
                            && let Ok(level) =
                                strand_services::record_field(value, "TallyItem", "level")
                        {
                            l.borrow_mut().push(level.clone());
                        }
                    }
                }
            }));
            // Its button writes 0.3, then 0.6: both shown at once (the last
            // wins), both sent with the item's key.
            let sc = step(&mut inst);
            let b = sc.of_kind(strand_scene::NodeKind::Box)[1];
            assert!(inst.event(b, "click", Vec::new()));
            assert!(step(&mut inst).texts().contains(&"b 0.6".to_string()));
            let deadline = Instant::now() + Duration::from_secs(5);
            while TALLY_WRITES.lock().unwrap().len() < 2
                || tally.cells().items.pending_item_writes(&rt, &2) > 0
            {
                assert!(Instant::now() < deadline, "the answers never came");
                std::thread::sleep(Duration::from_millis(5));
                step(&mut inst);
            }
            assert_eq!(
                *TALLY_WRITES.lock().unwrap(),
                [
                    "items Some(Int(2)).level Float(0.3)",
                    "items Some(Int(2)).level Float(0.6)"
                ]
            );
            use strand_services::Data::Float;
            assert_eq!(
                *levels.borrow(),
                [Float(0.3), Float(0.6)],
                "the answer to 0.3 came after the 0.6 write: an echo, ignored"
            );
            let texts = step(&mut inst).texts();
            assert!(texts.contains(&"b 0.6".to_string()), "{texts:?}");
            assert!(texts.contains(&"a 0".to_string()), "{texts:?}");
            // The first row's slider writes the first item.
            let slider = step(&mut inst).of_kind(strand_scene::NodeKind::Slider)[0];
            inst.write(
                slider,
                strand_scene::Prop::Value,
                strand_scene::PropValue::Number(0.25),
            )
            .unwrap();
            assert!(step(&mut inst).texts().contains(&"a 0.25".to_string()));
            let deadline = Instant::now() + Duration::from_secs(5);
            while TALLY_WRITES.lock().unwrap().len() < 3 {
                assert!(Instant::now() < deadline, "the slider's write never came");
                std::thread::sleep(Duration::from_millis(5));
                step(&mut inst);
            }
            assert_eq!(
                TALLY_WRITES.lock().unwrap()[2],
                "items Some(Int(1)).level Float(0.25)"
            );
            inst.shutdown();
            services.shutdown();
        }

        /// A bar shows the SSID; a closed popup lists the networks. The
        /// scan (the stream field) runs only while the popup is open,
        /// though the service runs all along.
        #[test]
        fn a_closed_popup_keeps_a_stream_field_off_while_a_bar_reads_another() {
            let _serial = WIFI_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            WIFI_LOG.lock().unwrap().clear();
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let wifi = services.register::<Wifi>(&rt);
            let (mut inst, mut scene) = mount(
                &rt,
                WIFI_SCHEMA,
                "export state p = false\nbar B {\n  text wifi.ssid\n  popup { open: <-> p; for n in wifi.networks key n { text n } }\n}\n",
                wifi.dynamic(),
            );
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"home".to_string()) {
                assert!(Instant::now() < deadline, "the bar never showed the SSID");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(wifi.running());
            assert_eq!(wifi.field_readers(0), 1, "the bar reads the SSID");
            assert_eq!(wifi.field_readers(1), 0, "nobody reads the networks");
            assert_eq!(*WIFI_LOG.lock().unwrap(), ["start scanning=false"]);
            // The popup opens: the scan starts.
            inst.set("svc.p", Value::Bool(true)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"cafe".to_string()) {
                assert!(
                    Instant::now() < deadline,
                    "the popup never listed the networks"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(wifi.field_readers(1), 1);
            // It closes: the scan stops at once; the service runs on for
            // the bar.
            inst.set("svc.p", Value::Bool(false)).unwrap();
            step(&mut inst);
            assert_eq!(wifi.field_readers(1), 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            while WIFI_LOG.lock().unwrap().len() < 3 {
                assert!(Instant::now() < deadline, "the scan never stopped");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                *WIFI_LOG.lock().unwrap(),
                [
                    "start scanning=false",
                    "scanning=true (networks)",
                    "scanning=false (networks)"
                ]
            );
            assert!(wifi.running());
            assert_eq!(wifi.starts(), 1);
            inst.shutdown();
            services.shutdown();
        }

        /// Live reload never restarts a built-in service (design.md, "Live
        /// reload": built-in services are kept): a bar reading `cpu.usage`
        /// and a shown popup reading a stream field (`wifi.networks`) go
        /// through a prop edit and a hard reload with each service started
        /// once, never stopped, and the stream never switched off and on
        /// (a Wi-Fi scan or a level meter would restart).
        #[test]
        fn live_reload_keeps_the_services_and_their_streams_running() {
            let _serial = WIFI_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            WIFI_LOG.lock().unwrap().clear();
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let builtin = Builtin::register(&services, &rt);
            let wifi = services.register::<Wifi>(&rt);
            let mut texts = strand_services::schemas();
            texts.push(WIFI_SCHEMA);
            let schema = Schema::builtin_with(&texts).unwrap();
            let src = |w: u32| {
                format!(
                    "export state p = true\nbar B {{\n  text pct(cpu.usage)\n  box {{ width: {w} }}\n  popup {{ open: <-> p; for n in wifi.networks key n {{ text n }} }}\n}}\n"
                )
            };
            let build = |prev: Option<&strand_compiler::reconcile::Build>, text: String| {
                let mut map = SourceMap::new();
                map.add("svc.strand", text);
                strand_compiler::reconcile::Build::compile_with(prev, map, &schema)
                    .unwrap_or_else(|d| panic!("{d:#?}"))
            };
            let first = build(None, src(10));
            let types = Rc::new(first.program.types.clone());
            let fallback = Rc::new(SchemaHost::new(&rt, &types, None));
            let screen = fallback.record(
                "Screen",
                &[
                    ("id", Value::text("Mock | DP-1 | Display")),
                    ("name", Value::text("DP-1")),
                ],
            );
            fallback
                .set(&rt, "screens.all", Value::list(vec![screen]))
                .unwrap();
            let mut host = Composite::new(fallback, types.clone());
            for svc in builtin.all().into_iter().chain([wifi.dynamic()]) {
                let name = svc.name();
                let items = svc.item_records();
                host.add(
                    Rc::new(StoreHost::new(&rt, svc, types.clone())),
                    &[name],
                    &items,
                );
            }
            let mut inst = Instance::from_build(&rt, &first, Rc::new(host), Storage::none());
            let mut scene = SceneMirror::new();
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"cafe".to_string()) {
                assert!(
                    Instant::now() < deadline,
                    "the popup never listed the networks"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            let cpu = &builtin.cpu;
            let settled = |what: &str| {
                assert_eq!((cpu.starts(), cpu.stops()), (1, 0), "cpu, {what}");
                assert_eq!((wifi.starts(), wifi.stops()), (1, 0), "wifi, {what}");
                assert_eq!(cpu.readers(), 1, "cpu readers, {what}");
                assert_eq!(wifi.field_readers(1), 1, "networks readers, {what}");
            };
            settled("at boot");
            // A prop edit, reconciled in place.
            let second = build(Some(&first), src(11));
            inst.reload(&second);
            for _ in 0..5 {
                step(&mut inst);
            }
            settled("after a prop edit");
            // A hard reload: a new tree from the same build.
            inst.reload_hard(&second);
            for _ in 0..5 {
                step(&mut inst);
            }
            settled("after a hard reload");
            // Past the 5 s grace too: nothing was released for good.
            inst.tick(STOP_GRACE + Duration::from_secs(1));
            services.pump(&rt);
            settled("past the grace");
            // Give the service thread time to log anything it was sent.
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(
                *WIFI_LOG.lock().unwrap(),
                ["start scanning=false", "scanning=true (networks)"],
                "the scan was never switched off and on"
            );
            inst.shutdown();
            services.shutdown();
        }

        /// A service read only through a top-level `let` (or a `fn`) is
        /// held by the scopes that read the `let`, not by the top level:
        /// a closed popup showing it keeps it stopped, and its field
        /// unread; a top-level `on change` of a `let` holds it for good.
        #[test]
        fn a_let_read_only_by_a_closed_popup_holds_nothing() {
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let tally = services.register::<Tally>(&rt);
            let (mut inst, mut scene) = mount(
                &rt,
                SCHEMA,
                "export state p = false\nlet lv = tally.level\nlet twice = lv * 2.0\nfn n() -> int { tally.items.count(i => true) }\nbar B {\n  text \"bar\"\n  popup { open: <-> p; text join(\" \", twice, n()) }\n}\n",
                tally.dynamic(),
            );
            let mut step = |inst: &mut Instance| {
                services.pump(&rt);
                let u = inst.tick(Duration::ZERO);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
                scene.apply(&u.diff).unwrap();
                scene.texts()
            };
            for _ in 0..5 {
                step(&mut inst);
            }
            assert_eq!(tally.readers(), 0, "nothing shown reads the `let`");
            assert!(!tally.running());
            assert_eq!(tally.starts(), 0);
            // The popup opens: it holds what `twice` and `n()` read.
            inst.set("svc.p", Value::Bool(true)).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !step(&mut inst).contains(&"0 0".to_string()) {
                assert!(Instant::now() < deadline, "the popup never showed");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(tally.readers(), 1);
            assert!(tally.running());
            assert_eq!(tally.field_readers(0), 1, "the popup reads the level");
            assert_eq!(tally.field_readers(1), 1, "and the items");
            inst.set("svc.p", Value::Bool(false)).unwrap();
            step(&mut inst);
            assert_eq!(tally.readers(), 0);
            assert_eq!(tally.field_readers(0), 0);
            inst.shutdown();

            // A top-level handler reading the `let` holds it all along.
            let (mut inst, _) = mount(
                &rt,
                SCHEMA,
                "export state seen = 0.0\nlet lv = tally.level\non change lv { seen = lv }\nbar B { text \"bar\" }\n",
                tally.dynamic(),
            );
            services.pump(&rt);
            inst.tick(Duration::ZERO);
            assert_eq!(tally.readers(), 1);
            assert_eq!(tally.field_readers(0), 1);
            inst.shutdown();
            services.shutdown();
        }

        struct Unpark(std::thread::Thread);

        impl Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        fn block_on<T>(fut: impl Future<Output = T>) -> T {
            let mut fut = std::pin::pin!(fut);
            let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
            let mut cx = Context::from_waker(&waker);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                    return v;
                }
                assert!(Instant::now() < deadline, "never resolved");
                std::thread::park_timeout(Duration::from_millis(50));
            }
        }

        /// Pump until `done`.
        fn until(rt: &Runtime, services: &Services, what: &str, done: impl Fn() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(Duration::from_millis(5));
                services.pump(rt);
                rt.flush();
            }
        }

        #[test]
        fn keyed_lists_writes_events_item_actions_and_async_calls_cross() {
            let mut schema = Schema::builtin().clone();
            schema.extend(SCHEMA).unwrap();
            assert!(
                schema.undocumented().is_empty(),
                "{:?}",
                schema.undocumented()
            );
            let types = Rc::new(schema.types.clone());
            let rt = Runtime::new();
            let services = Services::new(&rt, Buses::none(), || {});
            let client = services.register::<Tally>(&rt);
            let store = Rc::new(StoreHost::new(&rt, client.dynamic(), types.clone()));
            let fallback = Rc::new(SchemaHost::new(&rt, &types, None));
            let mut host = Composite::new(fallback, types.clone());
            let items = client.dynamic().item_records();
            assert_eq!(items, ["TallyItem"]);
            // Its actions and async method are its schema's, and so are
            // its item record's.
            super::assert_calls_match(&schema, &rt, &*client.dynamic());
            host.add(store.clone(), &["tally"], &items);
            let host: Rc<dyn ServiceHost> = Rc::new(host);
            host.acquire(&rt, "tally");
            let keyed = host
                .read_keyed(&rt, "tally", "items")
                .expect("a keyed field");
            assert!(host.read_keyed(&rt, "tally", "level").is_none());
            let removed = host.event(&rt, "tally", "removed").expect("an event");
            let heard = Rc::new(std::cell::RefCell::new(Vec::<Vec<Value>>::new()));
            let h = heard.clone();
            removed
                .on(&rt, move |_, args| {
                    h.borrow_mut().push(args.clone());
                    Ok(())
                })
                .unwrap();
            let len = |rt: &Runtime| keyed.with_untracked(rt, |v| v.len()).unwrap();
            // Actions on the service: the items arrive as keyed diffs.
            for name in ["a", "b", "c"] {
                host.action(
                    &rt,
                    ActionTarget::Service("tally"),
                    "add",
                    &[Value::text(name)],
                    ActionCtx::default(),
                )
                .unwrap();
            }
            until(&rt, &services, "three items", || len(&rt) == 3);
            let list = rt.untrack(|rt| host.read(rt, "tally", "items")).unwrap();
            let names: Vec<String> = list
                .as_list()
                .unwrap()
                .iter()
                .map(|v| {
                    v.field(&types, "name")
                        .unwrap()
                        .as_text()
                        .unwrap()
                        .to_string()
                })
                .collect();
            assert_eq!(names, ["a", "b", "c"]);
            // An item's action goes to the member whose actions take it.
            let b = list.as_list().unwrap()[1].clone();
            host.action(
                &rt,
                ActionTarget::Item(&b),
                "remove",
                &[],
                ActionCtx::default(),
            )
            .unwrap();
            until(&rt, &services, "b removed", || len(&rt) == 2);
            until(&rt, &services, "the event", || !heard.borrow().is_empty());
            assert_eq!(heard.borrow()[0], vec![b.clone()]);
            // A write shows at once; the service's answer (half) replaces
            // it, tagged so it is not taken for an echo.
            host.write(
                &rt,
                "tally",
                &[PathSeg::Field("level".into())],
                Value::float(0.8),
            )
            .unwrap();
            assert_eq!(
                rt.untrack(|rt| host.read(rt, "tally", "level")).unwrap(),
                Value::float(0.8)
            );
            until(&rt, &services, "the report", || {
                rt.untrack(|rt| host.read(rt, "tally", "level")).unwrap() == Value::float(0.4)
            });
            // Read-only fields refuse writes.
            assert!(
                host.write(&rt, "tally", &[PathSeg::Field("items".into())], Value::Null)
                    .is_err()
            );
            // An async method completes its fetch.
            let got = block_on(host.fetch(&rt, "tally", "echo", vec![Value::text("hi")]));
            assert_eq!(got.unwrap(), Value::text("hi"));
            assert!(
                host.call(&rt, "tally", "echo", &[Value::text("x")])
                    .is_err()
            );
            host.release(&rt, "tally");
            services.shutdown();
        }
    }

    /// No-code services (design.md: `service … from dbus|file|listen|poll`)
    /// through `strand run`'s composite host: read, written, followed,
    /// polled only while read, and restarted alone by a reload.
    mod custom_services {
        use std::path::Path;

        use strand_compiler::reconcile::Build;
        use strand_compiler::vm::host::PathSeg;
        use strand_services::testing::{DbusMock, PrivateBus};

        use super::*;

        /// A config on the real services, mounted from a build (so a
        /// reload can follow).
        struct Live {
            rt: Runtime,
            real: Real,
            inst: Instance,
            build: Build,
            now: Duration,
        }

        fn build(src: &str, prev: Option<&Build>) -> Build {
            let mut map = SourceMap::new();
            map.add("svc.strand", src.to_string());
            match Build::compile_with(prev, map, schema()) {
                Ok(b) => b,
                Err(d) => panic!("{d:#?}"),
            }
        }

        impl Live {
            fn boot(src: &str, buses: Buses) -> Live {
                let b = build(src, None);
                let rt = Runtime::new();
                let fallback = Rc::new(SchemaHost::new(&rt, &b.program.types, None));
                let screen = fallback.record(
                    "Screen",
                    &[
                        ("id", Value::text("Mock | DP-1 | Display")),
                        ("name", Value::text("DP-1")),
                    ],
                );
                fallback
                    .set(&rt, "screens.all", Value::list(vec![screen]))
                    .unwrap();
                let real = Real::start(&rt, &b.program.types, buses, fallback, || {});
                let inst = Instance::from_build(&rt, &b, real.host.clone(), Storage::none());
                let mut live = Live {
                    rt,
                    real,
                    inst,
                    build: b,
                    now: Duration::ZERO,
                };
                live.tick(Duration::ZERO);
                live
            }

            fn tick(&mut self, by: Duration) {
                self.real.services.pump(&self.rt);
                self.now += by;
                let u = self.inst.tick(self.now);
                assert!(u.errors.is_empty(), "{:?}", u.errors);
            }

            fn value(&self, service: &str, field: &str) -> Value {
                self.real.host.read(&self.rt, service, field).unwrap()
            }

            fn until(&mut self, what: &str, done: impl Fn(&Live) -> bool) {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !done(self) {
                    assert!(
                        Instant::now() < deadline,
                        "timed out: {what}; {:?}; {:?}; {:?}",
                        self.real.services.take_diagnostics(),
                        self.real.custom,
                        self.real.custom.debug_state(&self.rt)
                    );
                    std::thread::sleep(Duration::from_millis(10));
                    self.tick(Duration::ZERO);
                }
            }

            fn reload(&mut self, src: &str) {
                let b = build(src, Some(&self.build));
                self.inst.reload(&b);
                self.build = b;
                self.tick(Duration::ZERO);
            }
        }

        impl Drop for Live {
            fn drop(&mut self) {
                self.inst.shutdown();
                self.real.shutdown(&self.rt);
            }
        }

        fn temp(tag: &str) -> std::path::PathBuf {
            let d =
                std::env::temp_dir().join(format!("strand-custom-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        /// Written whole, as editors and tools save.
        fn save(path: &Path, text: &str) {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, text).unwrap();
            std::fs::rename(&tmp, path).unwrap();
        }

        /// `from file`: JSON read by key path into the declared types,
        /// followed through atomic saves and removal, with no polling.
        #[test]
        fn a_file_service_follows_its_file() {
            let dir = temp("file");
            let file = dir.join("mood.json");
            save(
                &file,
                r#"{"mood": {"level": 3}, "name": "calm", "temp": "41.5"}"#,
            );
            let src = format!(
                "service mood from file \"{}\" {{ level: int = mood.level; name: text; temp: float; gone: text? = nope }}\nbar B {{ text mood.name }}\n",
                file.display()
            );
            let mut live = Live::boot(&src, Buses::none());
            live.until("the file read", |l| {
                l.value("mood", "level") == Value::int(3)
            });
            assert_eq!(live.value("mood", "name"), Value::text("calm"));
            assert_eq!(
                live.value("mood", "temp"),
                Value::float(41.5),
                "text to float"
            );
            assert_eq!(live.value("mood", "gone"), Value::Null);
            save(&file, r#"{"mood": {"level": 5}, "name": "busy"}"#);
            live.until("the save followed", |l| {
                l.value("mood", "level") == Value::int(5)
            });
            assert_eq!(live.value("mood", "name"), Value::text("busy"));
            assert_eq!(
                live.value("mood", "temp"),
                Value::float(0.0),
                "a key the new document dropped reads as the default"
            );
            std::fs::remove_file(&file).unwrap();
            live.until("the removal followed", |l| {
                l.value("mood", "level") == Value::int(0)
            });
            assert_eq!(live.value("mood", "name"), Value::text(""));
            let client = live.real.custom.client("mood").unwrap();
            assert_eq!(client.starts(), 1);
            let reports = client.reports();
            std::thread::sleep(Duration::from_millis(300));
            live.tick(Duration::ZERO);
            assert_eq!(
                client.reports(),
                reports,
                "nothing changes, nothing reported"
            );
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// (M6, the owner's ruling) A config's `service battery` shadows
        /// the builtin `battery` (spec.md 4.2): its reads go to the
        /// declared service, never to the builtin one.
        #[test]
        fn a_config_service_shadows_the_builtin_of_its_name() {
            let dir = temp("shadow");
            let file = dir.join("battery.json");
            save(&file, r#"{"level": 42}"#);
            let src = format!(
                "service battery from file \"{}\" {{ level: int }}\nbar B {{ text join(\" \", battery.level) }}\n",
                file.display()
            );
            let mut live = Live::boot(&src, Buses::none());
            live.until("the config's battery read", |l| {
                l.value("battery", "level") == Value::int(42)
            });
            assert!(live.real.custom.client("battery").is_some());
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// (M6, the review of the owner's ruling) A config `service
        /// system` or `service screens` shadows the builtin for the
        /// config's reads only: the built-in palette still reads the
        /// builtin `system.dark`, `accent` and `contrast`, and the bars
        /// still mount on the builtin `screens` (each `tick` asserts no
        /// runtime error).
        #[test]
        fn the_runtimes_own_reads_go_to_the_builtin_services() {
            let svc = "permit exec \"true\"\n";
            let system = format!(
                "{svc}service system from poll [\"true\"] every 30s {{ up: int }}\nbar B {{ text join(\" \", system.up) {{ color: $accent; on click {{ }} }} }}\n"
            );
            let live = Live::boot(&system, Buses::none());
            assert_eq!(live.inst.nodes_handling("click").len(), 1);
            let dark = live
                .rt
                .untrack(|rt| live.real.host.read_builtin(rt, "system", "dark"));
            assert!(matches!(dark, Ok(Value::Bool(_))), "{dark:?}");
            drop(live);
            let screens = format!(
                "{svc}service screens from poll [\"true\"] every 30s {{ n: int }}\nbar B {{ text join(\" \", screens.n) {{ on click {{ }} }} }}\n"
            );
            let live = Live::boot(&screens, Buses::none());
            assert_eq!(live.inst.nodes_handling("click").len(), 1, "the bar mounts");
        }

        /// (M6, the review) A reload that adds a config `service cpu`
        /// lets go of the builtin `cpu` its old scopes held: a release
        /// goes to the host that took the hold, not to the one the name
        /// routes to now.
        #[test]
        fn a_reload_adding_a_shadow_lets_go_of_the_builtin() {
            let mut live = Live::boot("bar B { text pct(cpu.usage) }\n", Buses::none());
            let cpu = live.real.builtin.cpu.clone();
            assert_eq!(cpu.readers(), 1);
            live.reload(
                "permit exec \"true\"\nservice cpu from poll [\"true\"] every 30s { usage: float }\nbar B { text pct(cpu.usage) }\n",
            );
            assert_eq!(cpu.readers(), 0, "the builtin is let go");
            assert!(live.real.custom.client("cpu").is_some());
        }

        /// A file whose directory does not exist yet (an app's cache
        /// before its first run) is waited for, not failed and retried:
        /// the directories are followed as they appear, and again after
        /// they are removed.
        #[test]
        fn a_file_service_waits_for_its_directory() {
            waits_for_its_directory(Duration::ZERO);
        }

        /// The same with the directories made one at a time (`a`, then
        /// `a/b` later): the watch moves down a level and waits there
        /// (custom::tests::a_file_partly_on_its_way_is_waited_for_quietly
        /// bounds its turns).
        #[test]
        fn a_file_service_waits_for_its_directory_one_level_at_a_time() {
            waits_for_its_directory(Duration::from_millis(400));
        }

        fn waits_for_its_directory(pause: Duration) {
            let dir = temp(if pause.is_zero() {
                "file-dir"
            } else {
                "file-dir-paused"
            });
            let file = dir.join("a/b/mood.json");
            let src = format!(
                "service mood from file \"{}\" {{ level: int }}\nbar B {{ text join(\" \", mood.level) }}\n",
                file.display()
            );
            let mut live = Live::boot(&src, Buses::none());
            let client = live.real.custom.client("mood").unwrap();
            live.until("running", |_| client.running());
            std::thread::sleep(Duration::from_millis(100));
            std::fs::create_dir(dir.join("a")).unwrap();
            std::thread::sleep(pause);
            std::fs::create_dir(dir.join("a/b")).unwrap();
            save(&file, r#"{"level": 7}"#);
            live.until("the file in a new directory", |l| {
                l.value("mood", "level") == Value::int(7)
            });
            // Its directories go, and come back.
            std::fs::remove_dir_all(dir.join("a")).unwrap();
            live.until("the removal followed", |l| {
                l.value("mood", "level") == Value::int(0)
            });
            std::fs::create_dir_all(dir.join("a/b")).unwrap();
            save(&file, r#"{"level": 9}"#);
            live.until("the file back", |l| {
                l.value("mood", "level") == Value::int(9)
            });
            assert_eq!(client.starts(), 1, "never failed and restarted");
            assert!(
                !live
                    .real
                    .services
                    .take_diagnostics()
                    .iter()
                    .any(|d| d.service == "mood"),
                "nothing failed"
            );
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A value that does not convert to its field's type reads as the
        /// type's default and is reported once (naming the field, its key
        /// and the value) until a value converts again; a colour reads
        /// from hex text.
        #[test]
        fn a_value_that_does_not_convert_is_reported_once() {
            let dir = temp("mismatch");
            let file = dir.join("m.json");
            save(&file, r##"{"level": "abc", "tint": "#ff0000"}"##);
            let src = format!(
                "service m from file \"{}\" {{ lvl: int = level; tint: color }}\nbar B {{ text join(\" \", m.lvl) }}\n",
                file.display()
            );
            let mut live = Live::boot(&src, Buses::none());
            // (A colour field reads a colour before the file is read too:
            // the default.)
            let red = Value::Color(strand_scene::Color::from_hex("#ff0000").unwrap());
            live.until("the file read", |l| l.value("m", "tint") == red);
            assert_eq!(live.value("m", "lvl"), Value::int(0));
            let mismatches = |l: &Live| -> Vec<strand_services::ServiceDiagnostic> {
                l.real
                    .services
                    .take_diagnostics()
                    .into_iter()
                    .filter(|d| d.message.contains("`m.lvl`"))
                    .collect()
            };
            let first = mismatches(&live);
            assert_eq!(first.len(), 1, "{first:?}");
            assert!(!first[0].resolved && !first[0].notice);
            // Keyed by the declared name, not one key for every custom
            // service.
            assert_eq!(first[0].service, "m");
            assert!(
                first[0].message.contains("`level`") && first[0].message.contains("\"abc\""),
                "{}",
                first[0].message
            );
            // Another value that does not convert: not reported again.
            save(&file, r##"{"level": [1], "tint": "#ff0000"}"##);
            live.until("the second save read", |l| {
                l.real
                    .custom
                    .debug_state(&l.rt)
                    .iter()
                    .any(|s| s.contains("List"))
            });
            assert_eq!(live.value("m", "lvl"), Value::int(0));
            assert!(mismatches(&live).is_empty());
            // A value that converts resolves it; a bad one after reports
            // again.
            save(&file, r##"{"level": 4}"##);
            live.until("the fix read", |l| l.value("m", "lvl") == Value::int(4));
            let fixed = mismatches(&live);
            assert_eq!(fixed.len(), 1, "{fixed:?}");
            assert!(fixed[0].resolved);
            assert_eq!(fixed[0].message, first[0].message);
            save(&file, r##"{"level": true}"##);
            live.until("the bad save read", |l| {
                l.real
                    .custom
                    .debug_state(&l.rt)
                    .iter()
                    .any(|s| s.contains("Bool"))
            });
            assert_eq!(live.value("m", "lvl"), Value::int(1), "a bool is 0 or 1");
            save(&file, r##"{"level": "nope"}"##);
            live.until("the bad save read", |l| {
                l.real
                    .custom
                    .debug_state(&l.rt)
                    .iter()
                    .any(|s| s.contains("nope"))
            });
            assert_eq!(live.value("m", "lvl"), Value::int(0));
            let again = mismatches(&live);
            assert_eq!(again.len(), 1, "{again:?}");
            assert!(!again[0].resolved && again[0].message.contains("\"nope\""));
            // The usual fix, the declared type changed: the reload
            // restarts the service, which resolves the open report (and
            // the new type takes the value, so nothing is reported anew).
            live.reload(&src.replace("lvl: int", "lvl: text"));
            live.until("the retyped field read", |l| {
                l.value("m", "lvl") == Value::text("nope")
            });
            let restarted = mismatches(&live);
            assert_eq!(restarted.len(), 1, "{restarted:?}");
            assert!(restarted[0].resolved && restarted[0].message == again[0].message);
            // Back to `int`: reported again; the service removed: resolved.
            live.reload(&src);
            // The retyped cell starts at `int`'s default 0 before the
            // restarted run's read reports the mismatch: wait for the
            // report itself (taking drains, so collect what arrives).
            let seen = std::cell::RefCell::new(Vec::new());
            live.until("the int field's mismatch reported", |l| {
                seen.borrow_mut().extend(mismatches(l));
                !seen.borrow().is_empty()
            });
            assert_eq!(live.value("m", "lvl"), Value::int(0));
            let back = seen.into_inner();
            assert_eq!(back.len(), 1, "{back:?}");
            assert!(!back[0].resolved);
            live.reload("bar B { text \"none\" }\n");
            let stopped = mismatches(&live);
            assert_eq!(stopped.len(), 1, "{stopped:?}");
            assert!(stopped[0].resolved && stopped[0].message == back[0].message);
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// `from poll`: the command runs every interval only while a
        /// reader is visible; `from listen`: each line a command prints is
        /// a document, and the command ends with the service.
        #[test]
        fn poll_runs_only_while_read_and_listen_reads_lines() {
            let dir = temp("poll");
            let runs = dir.join("runs");
            let value = dir.join("value");
            std::fs::write(&value, "7\n").unwrap();
            let src = format!(
                "permit exec \"sh\"\nexport state o = true\nservice p from poll [\"sh\", \"-c\", \"echo run >> {runs}; cat {value}\"] every 100ms {{ n: int }}\nservice l from listen [\"sh\", \"-c\", \"echo '{{\\\"a\\\": 1}}'; echo a=2; echo b=x; sleep 30\"] {{ a: int; b: text }}\npanel P {{ open: <-> o; text join(\" \", p.n, l.a, l.b) }}\n",
                runs = runs.display(),
                value = value.display()
            );
            let mut live = Live::boot(&src, Buses::none());
            live.until("polled", |l| l.value("p", "n") == Value::int(7));
            live.until("several polls", |_| {
                std::fs::read_to_string(&runs)
                    .unwrap_or_default()
                    .lines()
                    .count()
                    >= 3
            });
            live.until("lines read", |l| {
                l.value("l", "a") == Value::int(2) && l.value("l", "b") == Value::text("x")
            });
            // Hidden: polling stops at once (the service lives on for the
            // grace).
            live.inst.set("svc.o", Value::Bool(false)).unwrap();
            live.tick(Duration::ZERO);
            std::thread::sleep(Duration::from_millis(150));
            let count = std::fs::read_to_string(&runs).unwrap().lines().count();
            std::thread::sleep(Duration::from_millis(500));
            assert_eq!(
                std::fs::read_to_string(&runs).unwrap().lines().count(),
                count,
                "no polls while hidden"
            );
            // Shown again: polling resumes.
            live.inst.set("svc.o", Value::Bool(true)).unwrap();
            live.tick(Duration::ZERO);
            live.until("polling again", |_| {
                std::fs::read_to_string(&runs).unwrap().lines().count() > count
            });
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// (history) `from poll` under a `graph`: hidden, it polls at
        /// the kept period (2 s for a 10 s history), not every 100 ms
        /// and not never; a graph's history kept of a no-code service
        /// as of a builtin one.
        #[test]
        fn a_kept_poll_service_polls_at_the_hidden_period() {
            let dir = temp("kept-poll");
            let runs = dir.join("runs");
            let src = format!(
                "permit exec \"sh\"\nexport state o = true\nservice p from poll [\"sh\", \"-c\", \"echo run >> {runs}; echo 0.5\"] every 100ms {{ n: float }}\npanel P {{ open: <-> o; graph p.n {{ history: 10s }} }}\n",
                runs = runs.display(),
            );
            let count = || {
                std::fs::read_to_string(&runs)
                    .unwrap_or_default()
                    .lines()
                    .count()
            };
            let mut live = Live::boot(&src, Buses::none());
            live.until("several polls", |_| count() >= 3);
            live.inst.set("svc.o", Value::Bool(false)).unwrap();
            live.tick(Duration::ZERO);
            std::thread::sleep(Duration::from_millis(150));
            let hidden = count();
            std::thread::sleep(Duration::from_millis(3000));
            let polls = count() - hidden;
            assert!((1..=2).contains(&polls), "every 2 s while hidden: {polls}");
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// `from poll`: shown again sooner than `every` after the last
        /// poll (a hover popup flickering), it does not poll early.
        #[test]
        fn a_flickering_reader_does_not_poll_faster_than_every() {
            let dir = temp("flicker");
            let runs = dir.join("runs");
            let src = format!(
                "permit exec \"sh\"\nexport state o = true\nservice p from poll [\"sh\", \"-c\", \"echo run >> {runs}; echo 7\"] every 30s {{ n: int }}\npanel P {{ open: <-> o; text join(\" \", p.n) }}\n",
                runs = runs.display()
            );
            let count = || {
                std::fs::read_to_string(&runs)
                    .unwrap_or_default()
                    .lines()
                    .count()
            };
            let mut live = Live::boot(&src, Buses::none());
            live.until("polled", |l| l.value("p", "n") == Value::int(7));
            assert_eq!(count(), 1);
            for _ in 0..5 {
                live.inst.set("svc.o", Value::Bool(false)).unwrap();
                live.tick(Duration::ZERO);
                std::thread::sleep(Duration::from_millis(40));
                live.inst.set("svc.o", Value::Bool(true)).unwrap();
                live.tick(Duration::ZERO);
                std::thread::sleep(Duration::from_millis(40));
            }
            std::thread::sleep(Duration::from_millis(300));
            live.tick(Duration::ZERO);
            assert_eq!(count(), 1, "shown again within `every`: no early poll");
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// `from listen`: a line that is not UTF-8 is read leniently and
        /// the lines after it are still read.
        #[test]
        fn listen_reads_on_past_a_line_that_is_not_utf8() {
            let src = "permit exec \"sh\"\nservice l from listen [\"sh\", \"-c\", \"echo a=1; printf 'a=\\\\377\\\\n'; sleep .2; echo a=3; sleep 30\"] { a: int }\nbar B { text join(\" \", l.a) }\n";
            let mut live = Live::boot(src, Buses::none());
            live.until("the line after it read", |l| {
                l.value("l", "a") == Value::int(3)
            });
            // No failure: at most the lenient line's value, which is not
            // an int, reported and resolved by the line after it.
            let failures: Vec<_> = live
                .real
                .services
                .take_diagnostics()
                .into_iter()
                .filter(|d| !d.message.starts_with("`l.a`:"))
                .collect();
            assert!(failures.is_empty(), "{failures:?}");
        }

        fn gone(pid: &str) -> bool {
            match std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())) {
                Err(_) => true,
                Ok(stat) => stat
                    .rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
            }
        }

        /// What a `listen` command started ends with the service: on a
        /// reload restart and on a stop.
        #[test]
        fn a_listen_command_leaves_nothing_running() {
            let dir = temp("orphans");
            let pids = dir.join("pids");
            let src = |fields: &str, with: bool| {
                if !with {
                    return "bar B { text \"x\" }\n".to_string();
                }
                format!(
                    "permit exec \"sh\"\nservice l from listen [\"sh\", \"-c\", \"sleep 30 & echo $! >> {pids}; echo a=1; wait\"] {{ {fields} }}\nbar B {{ text join(\" \", l.a) }}\n",
                    pids = pids.display()
                )
            };
            let started = || -> Vec<String> {
                std::fs::read_to_string(&pids)
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            };
            let mut live = Live::boot(&src("a: int", true), Buses::none());
            live.until("read", |l| l.value("l", "a") == Value::int(1));
            live.until("started", |_| started().len() == 1);
            let first = started()[0].clone();
            assert!(!gone(&first));
            // A reload restart: the first command's child ends.
            live.reload(&src("a: int; b: text", true));
            live.until("restarted", |_| started().len() == 2);
            live.until("the first child gone", |_| gone(&first));
            let second = started()[1].clone();
            assert!(!gone(&second));
            // Stopped: its child ends too.
            live.reload(&src("", false));
            live.until("the second child gone", |_| gone(&second));
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A reload that changes one custom service's declaration
        /// restarts only it; one that removes it stops it; the other keeps
        /// running.
        #[test]
        fn a_reload_restarts_only_the_changed_custom_service() {
            let dir = temp("reload");
            let (a, b) = (dir.join("a"), dir.join("b"));
            save(&a, "1");
            save(&b, "2");
            let src = |b_key: &str, with_b: bool| {
                let mut s = format!("service fa from file \"{}\" {{ x: int }}\n", a.display());
                if with_b {
                    s.push_str(&format!(
                        "service fb from file \"{}\" {{ {b_key}: int }}\nbar B {{ text join(\" \", fa.x, fb.{b_key}) }}\n",
                        b.display()
                    ));
                } else {
                    s.push_str("bar B { text join(\" \", fa.x) }\n");
                }
                s
            };
            let mut live = Live::boot(&src("y", true), Buses::none());
            live.until("both read", |l| {
                l.value("fa", "x") == Value::int(1) && l.value("fb", "y") == Value::int(2)
            });
            let ca = live.real.custom.client("fa").unwrap();
            let cb = live.real.custom.client("fb").unwrap();
            assert_eq!((ca.starts(), cb.starts()), (1, 1));
            assert_eq!((ca.name(), cb.name()), ("fa", "fb"));
            let registered = live.real.services.registered();
            live.reload(&src("z", true));
            live.until("the renamed field read", |l| {
                l.value("fb", "z") == Value::int(2)
            });
            assert_eq!(cb.starts(), 2, "the changed service restarted");
            assert_eq!((ca.starts(), ca.stops()), (1, 0), "the other did not");
            live.reload(&src("z", false));
            assert!(!cb.running(), "a removed service stops");
            assert!(live.real.custom.client("fb").is_none());
            assert_eq!(
                live.real.services.registered(),
                registered - 1,
                "a removed service leaves no registration behind"
            );
            // Declared again and removed again: still none left behind.
            live.reload(&src("w", true));
            live.until("declared again", |l| l.value("fb", "w") == Value::int(2));
            assert_eq!(live.real.services.registered(), registered);
            live.reload(&src("w", false));
            assert_eq!(live.real.services.registered(), registered - 1);
            assert_eq!((ca.starts(), ca.stops()), (1, 0));
            assert_eq!(live.value("fa", "x"), Value::int(1));
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A reload that declares (or removes) types before a service's
        /// enum renumbers them without changing the service: it keeps
        /// running, and its value is of the new program's enum.
        #[test]
        fn a_reload_that_renumbers_types_retypes_the_service() {
            let dir = temp("retype");
            let file = dir.join("mode.json");
            save(&file, r#"{"mode": "fast"}"#);
            let src = |extra: &str| {
                format!(
                    "{extra}enum Mode {{ slow, fast }}\nservice e from file \"{}\" {{ mode: Mode }}\nbar B {{ text e.mode == fast ? \"yes\" : \"no\" }}\n",
                    file.display()
                )
            };
            let mode = |l: &Live| l.build.program.types.find_enum("Mode").unwrap();
            let mut live = Live::boot(&src(""), Buses::none());
            live.until("the file read", |l| {
                l.value("e", "mode") == Value::Enum(mode(l), 1)
            });
            let client = live.real.custom.client("e").unwrap();
            let before = mode(&live);
            for extra in ["enum Aaa { a }\nenum Bbb { b }\n", ""] {
                live.reload(&src(extra));
                let now = mode(&live);
                assert_eq!(
                    live.value("e", "mode"),
                    Value::Enum(now, 1),
                    "the value is of the new program's Mode (after {extra:?})"
                );
                assert!(now.0 as usize <= live.build.program.types.enums.len());
            }
            assert_eq!(mode(&live), before);
            assert_eq!(
                (client.starts(), client.stops()),
                (1, 0),
                "renumbered, not restarted"
            );
            drop(live);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// design.md's `service ppd from dbus system
        /// "net.hadess.PowerProfiles" { profile: text rw = ActiveProfile }`
        /// against python-dbusmock's power-profiles-daemon: checked against
        /// its introspection, read, written through `Set`, and following
        /// the property when something else changes it.
        #[test]
        fn the_design_ppd_service_on_a_mock_power_profiles_daemon() {
            let Some(bus) = PrivateBus::start() else {
                return;
            };
            let Some(_ppd) = DbusMock::start(
                &bus,
                "power_profiles_daemon",
                true,
                None,
                "net.hadess.PowerProfiles",
            ) else {
                return;
            };
            let src = "enum Profile { power_saver, balanced, performance }\nservice ppd from dbus system \"net.hadess.PowerProfiles\" { profile: text rw = ActiveProfile; degraded: text = PerformanceDegraded }\nservice mode from dbus system \"net.hadess.PowerProfiles\" { active: Profile rw = ActiveProfile; action hold(profile: Profile, reason: text, app: text) = HoldProfile }\nbar B { text join(\" \", ppd.profile, mode.active) }\n";
            // The check against introspection, on the mock's bus.
            let address = match bus.buses().system {
                strand_services::Bus::Address(a) => a,
                other => panic!("{other:?}"),
            };
            struct OnBus(String);
            impl strand_compiler::check::dbus::Introspect for OnBus {
                fn properties(
                    &self,
                    _system: bool,
                    name: &str,
                    path: &str,
                ) -> Option<Result<Vec<strand_compiler::check::dbus::BusProperty>, String>>
                {
                    Some(
                        strand_introspect::properties(
                            &strand_introspect::Bus::Address(self.0.clone()),
                            name,
                            path,
                        )
                        .map(crate::services::custom::bus_properties),
                    )
                }
            }
            let check = |src: &str| {
                let mut map = SourceMap::new();
                map.add("svc.strand", src.to_string());
                let c = strand_compiler::compile_with(&map, schema());
                strand_compiler::check::dbus::check(&c.program, &OnBus(address.clone()))
            };
            assert!(check(src).is_empty(), "{:?}", check(src));
            let bad = check(&src.replace("= ActiveProfile", "= ActiveProfil"));
            assert_eq!(bad[0].code, "check::dbus_property", "{bad:?}");
            assert_eq!(bad[0].suggestions[0].replacement, "ActiveProfile");

            let mut live = Live::boot(src, bus.buses());
            live.until("read", |l| {
                l.value("ppd", "profile") == Value::text("balanced")
            });
            assert_eq!(live.value("ppd", "degraded"), Value::text(""));
            // Written: the property is set (and the echo ignored).
            live.real
                .host
                .write(
                    &live.rt,
                    "ppd",
                    &[PathSeg::Field("profile".into())],
                    Value::text("performance"),
                )
                .unwrap();
            assert_eq!(
                live.value("ppd", "profile"),
                Value::text("performance"),
                "at once"
            );
            let get = || {
                let tokio = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                tokio.block_on(async {
                    let c = zbus::connection::Builder::address(address.as_str())
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    let p = zbus::fdo::PropertiesProxy::builder(&c)
                        .destination("net.hadess.PowerProfiles")
                        .unwrap()
                        .path("/net/hadess/PowerProfiles")
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    let v = p
                        .get(
                            zbus::names::InterfaceName::try_from("net.hadess.PowerProfiles")
                                .unwrap(),
                            "ActiveProfile",
                        )
                        .await
                        .unwrap();
                    String::try_from(v).unwrap()
                })
            };
            live.until("the daemon has it", |_| get() == "performance");
            // Changed by someone else: followed through PropertiesChanged.
            {
                let tokio = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                tokio.block_on(async {
                    let c = zbus::connection::Builder::address(address.as_str())
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    let p = zbus::fdo::PropertiesProxy::builder(&c)
                        .destination("net.hadess.PowerProfiles")
                        .unwrap()
                        .path("/net/hadess/PowerProfiles")
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    p.set(
                        zbus::names::InterfaceName::try_from("net.hadess.PowerProfiles").unwrap(),
                        "ActiveProfile",
                        zbus::zvariant::Value::from("power-saver"),
                    )
                    .await
                    .unwrap();
                });
            }
            live.until("followed", |l| {
                l.value("ppd", "profile") == Value::text("power-saver")
            });
            // An enum field reads `power-saver` as `power_saver`, and
            // writes it back as the daemon spells it.
            let types = live.build.program.types.clone();
            let profile = types.find_enum("Profile").unwrap();
            let variant =
                |name: &str| Value::Enum(profile, types.enum_(profile).variant(name).unwrap());
            live.until("the enum read", |l| {
                l.value("mode", "active") == variant("power_saver")
            });
            for (name, spelled) in [("balanced", "balanced"), ("power_saver", "power-saver")] {
                live.real
                    .host
                    .write(
                        &live.rt,
                        "mode",
                        &[PathSeg::Field("active".into())],
                        variant(name),
                    )
                    .unwrap();
                live.until("the daemon has the enum's spelling", |_| get() == spelled);
                live.until("read back", |l| l.value("mode", "active") == variant(name));
            }
            // (M6, the review of the owner's ruling) An action calls its
            // method through `CustomHost::action`, an enum argument
            // spelled as the daemon spells it (`power-saver`), as a field
            // write is.
            live.real
                .host
                .action(
                    &live.rt,
                    strand_compiler::vm::host::ActionTarget::Service("mode"),
                    "hold",
                    &[
                        variant("power_saver"),
                        Value::text("a reason with spaces"),
                        Value::text("strand"),
                    ],
                    strand_compiler::vm::host::ActionCtx::default(),
                )
                .unwrap();
            let held = || -> Vec<Vec<String>> {
                let tokio = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                tokio.block_on(async {
                    let c = zbus::connection::Builder::address(address.as_str())
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    let reply = c
                        .call_method(
                            Some("net.hadess.PowerProfiles"),
                            "/net/hadess/PowerProfiles",
                            Some("org.freedesktop.DBus.Mock"),
                            "GetMethodCalls",
                            &("HoldProfile",),
                        )
                        .await
                        .unwrap();
                    let calls: Vec<(u64, Vec<zbus::zvariant::OwnedValue>)> =
                        reply.body().deserialize().unwrap();
                    calls
                        .into_iter()
                        .map(|(_, args)| {
                            args.into_iter()
                                .map(|v| String::try_from(v).unwrap_or_default())
                                .collect()
                        })
                        .collect()
                })
            };
            live.until("the method was called", |_| !held().is_empty());
            assert_eq!(
                held(),
                [["power-saver", "a reason with spaces", "strand"]],
                "the enum as the daemon spells it"
            );
            assert!(
                live.real
                    .host
                    .action(
                        &live.rt,
                        strand_compiler::vm::host::ActionTarget::Service("mode"),
                        "nope",
                        &[],
                        strand_compiler::vm::host::ActionCtx::default(),
                    )
                    .is_err(),
                "an undeclared action"
            );
            // A read-only field refuses writes.
            assert!(
                live.real
                    .host
                    .write(
                        &live.rt,
                        "ppd",
                        &[PathSeg::Field("degraded".into())],
                        Value::text("x"),
                    )
                    .is_err()
            );
        }
    }

    /// Plugin services (decisions.md m5-plugins): an extra served as a
    /// builtin is, and a service a plugin schema file declares that
    /// nothing serves. All through `Real::start_with` and a `Served`
    /// from `schema_with`, never the process-wide lists.
    mod plugins {
        use std::path::{Path, PathBuf};

        use strand_compiler::vm::host::{ActionCtx, ActionTarget, PathSeg};
        use strand_services::{Call, Cx, Msg, ServiceError, Store, service};

        use super::*;

        pub const COUNTER: &str = "
/// A counter, a plugin service of the tests.
service counter {
  /// The count, 41 after its first read.
  count: int rw
  /// Adds one.
  action bump()
}
";

        #[derive(Call, Debug)]
        pub enum CounterAction {
            Bump,
        }

        /// See the schema.
        #[service(name = "counter", schema = COUNTER, action = CounterAction)]
        #[derive(Store, Clone, Debug, Default, PartialEq)]
        pub struct Counter {
            /// The count.
            #[store(rw)]
            pub count: i64,
        }

        impl Counter {
            async fn run(mut cx: Cx<Self>) -> Result<(), ServiceError> {
                cx.update(|s| s.count = 41);
                cx.ready();
                while let Some(m) = cx.recv().await {
                    match m {
                        Msg::Write(w) => {
                            let n: i64 = w.value().map_err(ServiceError::from)?;
                            cx.report(&w, |s| s.count = n);
                        }
                        Msg::Action(CounterAction::Bump) => {
                            cx.update(|s| s.count += 1);
                        }
                        _ => {}
                    }
                }
                Ok(())
            }
        }

        const HELLO: &str = "/// Hello.\nservice hello {\n  /// A greeting.\n  greeting: text\n  /// A count.\n  count: int rw\n  /// Greets.\n  action greet(name: text)\n}\n";

        struct Temp(PathBuf);

        impl Temp {
            fn new(tag: &str) -> Temp {
                let p = std::env::temp_dir()
                    .join(format!("strand-served-{tag}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Temp(p)
            }

            fn file(&self, name: &str, text: &str) -> (PathBuf, String) {
                let p = self.0.join(name);
                std::fs::write(&p, text).unwrap();
                (p, text.to_string())
            }
        }

        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn leak(s: Served) -> &'static Served {
            Box::leak(Box::new(s))
        }

        /// A config mounted on one monitor against `Real::start_with`.
        fn boot(
            src: &str,
            extras: &[Registration],
            served: &'static Served,
        ) -> (Runtime, Real, Instance) {
            let mut map = SourceMap::new();
            map.add("svc.strand", src.to_string());
            let compiled = strand_compiler::compile_with(&map, &served.schema);
            assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
            let program = Arc::new(lower::lower(&compiled.program, &served.schema));
            let rt = Runtime::new();
            let fallback = Rc::new(SchemaHost::new(&rt, &program.types, None));
            let screen = fallback.record(
                "Screen",
                &[
                    ("id", Value::text("Mock | DP-1 | Display")),
                    ("name", Value::text("DP-1")),
                ],
            );
            fallback
                .set(&rt, "screens.all", Value::list(vec![screen]))
                .unwrap();
            let types = program.types.clone();
            let real =
                Real::start_with(&rt, &types, Buses::none(), fallback, || {}, extras, served);
            let inst = Instance::new(&rt, program, real.host.clone(), Storage::none());
            let u = inst.tick(Duration::ZERO);
            assert!(u.errors.is_empty(), "{:?}", u.errors);
            (rt, real, inst)
        }

        /// `Real::served` finds a builtin and a registered extra.
        #[test]
        fn real_served_finds_the_extra() {
            let extras = [Registration::of::<Counter>()];
            let served = leak(schema_with(&extras, &[]));
            assert!(served.warnings.is_empty(), "{:?}", served.warnings);
            let (rt, real, mut inst) = boot("bar B { text \"x\" }\n", &extras, served);
            assert!(real.served("counter").is_some());
            assert!(real.served("battery").is_some());
            assert!(real.served("nope").is_none());
            inst.shutdown();
            real.shutdown(&rt);
        }

        /// `strand set counter.count +1` on the extra nobody reads starts
        /// from its first read (41), not from the default.
        #[test]
        fn a_relative_set_on_an_unread_extra_steps_from_its_first_read() {
            let extras = [Registration::of::<Counter>()];
            let served = leak(schema_with(&extras, &[]));
            let (rt, real, mut inst) = boot("bar B { text \"x\" }\n", &extras, served);
            real.set_text(&rt, "counter.count", "+1").unwrap();
            let count = real.host.read(&rt, "counter", "count").unwrap();
            assert_eq!(count, Value::int(42));
            inst.shutdown();
            real.shutdown(&rt);
        }

        /// The served schema: a linked service's own file is skipped (a
        /// stale one says so), and a file that declares what nothing
        /// serves is listed with its path.
        #[test]
        fn files_join_the_served_schema() {
            let t = Temp::new("files");
            let head = strand_compiler::schema::plugins::header("/x/plugin");
            let fresh = t.file("counter.schema", &format!("{head}{COUNTER}"));
            let hello = t.file("hello.schema", HELLO);
            let extras = [Registration::of::<Counter>()];
            let s = schema_with(&extras, &[fresh.clone(), hello.clone()]);
            assert!(s.warnings.is_empty(), "{:?}", s.warnings);
            assert_eq!(s.unserved, [("hello".to_string(), hello.0.clone())]);
            assert!(s.schema.services.contains_key("counter"));
            let stale = t.file("counter.schema", "/// Old.\nservice counter {}\n");
            let s = schema_with(&extras, &[stale]);
            assert!(
                s.warnings.len() == 1 && s.warnings[0].contains("stale; run"),
                "{:?}",
                s.warnings
            );
            // Without the code, its file declares it: unserved.
            let s = schema_with(&[], std::slice::from_ref(&fresh));
            assert_eq!(s.unserved, [("counter".to_string(), fresh.0)]);
        }

        /// A service only a file declares: read at its defaults, with one
        /// notice naming the file on the first acquire, resolved on the
        /// last release; writes and actions refused with its text.
        #[test]
        fn an_unserved_file_service_raises_a_notice_while_read() {
            let t = Temp::new("unserved");
            let hello = t.file("hello.schema", HELLO);
            let path: &Path = &hello.0;
            let served = leak(schema_with(&[], std::slice::from_ref(&hello)));
            let (rt, real, mut inst) = boot("bar B { text hello.greeting }\n", &[], served);
            let said = unserved::notice("hello", path);
            assert!(said.contains(&path.display().to_string()), "{said}");
            let d = real.services.take_diagnostics();
            assert_eq!(d.len(), 1, "{d:?}");
            assert!(
                d[0].notice && !d[0].resolved && d[0].message == said,
                "{d:?}"
            );
            assert_eq!(
                real.host.read(&rt, "hello", "greeting").unwrap(),
                Value::text("")
            );
            let err = real.set_text(&rt, "hello.count", "1").unwrap_err();
            assert!(err.contains(&said), "{err}");
            let err = real
                .host
                .action(
                    &rt,
                    ActionTarget::Service("hello"),
                    "greet",
                    &[Value::text("world")],
                    ActionCtx::default(),
                )
                .unwrap_err();
            assert!(err.to_string().contains(&said), "{err}");
            inst.shutdown();
            let d = real.services.take_diagnostics();
            assert!(
                d.len() == 1 && d[0].resolved && d[0].message == said,
                "{d:?}"
            );
            real.shutdown(&rt);
        }

        /// An unserved file service's items refuse their actions and
        /// writes with the notice's text too (review round 3: they went
        /// to the fallback, which simulated them and answered `Ok`).
        #[test]
        fn an_unserved_file_service_refuses_item_actions() {
            const GREETER: &str = "/// A greeter.\nrecord Greeter {\n  /// Its name.\n  name: text\n  /// Pokes it.\n  action poke()\n}\n/// Hello.\nservice hello {\n  /// The main one.\n  main: Greeter\n}\n";
            let t = Temp::new("unserved-items");
            let hello = t.file("hello.schema", GREETER);
            let served = leak(schema_with(&[], std::slice::from_ref(&hello)));
            assert!(served.warnings.is_empty(), "{:?}", served.warnings);
            let (rt, real, mut inst) = boot("bar B { text hello.main.name }\n", &[], served);
            let said = unserved::notice("hello", &hello.0);
            let g = real.host.read(&rt, "hello", "main").unwrap();
            assert!(matches!(g, Value::Record(_)), "{g:?}");
            let err = real
                .host
                .action(
                    &rt,
                    ActionTarget::Item(&g),
                    "poke",
                    &[],
                    ActionCtx::default(),
                )
                .unwrap_err();
            assert!(err.to_string().contains(&said), "{err}");
            let err = real
                .host
                .write_item(&rt, &g, &[PathSeg::Field("name".into())], Value::text("x"))
                .unwrap_err();
            assert!(err.to_string().contains(&said), "{err}");
            inst.shutdown();
            real.shutdown(&rt);
        }

        /// Under `STRAND_MOCK` (no `Real`), a config reading a file's
        /// service and a plugin's own mounts on the schema host alone and
        /// reads both at their defaults (review round 2: it failed with
        /// "`hello` has no field `greeting`").
        #[test]
        fn the_mock_reads_plugin_services_at_their_defaults() {
            let t = Temp::new("mock");
            let hello = t.file("hello.schema", HELLO);
            let extras = [Registration::of::<Counter>()];
            let served = schema_with(&extras, &[hello]);
            let src = "export let g = hello.greeting\n\
                       export let c = counter.count\n\
                       bar B { text g }\n";
            let mut map = SourceMap::new();
            map.add("svc.strand", src.to_string());
            let compiled = strand_compiler::compile_with(&map, &served.schema);
            assert_eq!(compiled.errors(), 0, "{:#?}", compiled.diagnostics);
            let program = Arc::new(lower::lower(&compiled.program, &served.schema));
            let rt = Runtime::new();
            let host = Rc::new(SchemaHost::new(&rt, &program.types, None));
            let screen = host.record(
                "Screen",
                &[
                    ("id", Value::text("Mock | DP-1 | Display")),
                    ("name", Value::text("DP-1")),
                ],
            );
            host.set(&rt, "screens.all", Value::list(vec![screen]))
                .unwrap();
            mock_served(&rt, &host, &program.types, &served);
            let mut inst = Instance::new(&rt, program, host.clone(), Storage::none());
            let u = inst.tick(Duration::ZERO);
            assert!(u.errors.is_empty(), "{:?}", u.errors);
            assert_eq!(
                host.read(&rt, "hello", "greeting").unwrap(),
                Value::text("")
            );
            assert_eq!(host.read(&rt, "counter", "count").unwrap(), Value::int(0));
            inst.shutdown();
        }

        /// A file nobody reads raises nothing.
        #[test]
        fn an_unread_file_service_raises_nothing() {
            let t = Temp::new("unread");
            let hello = t.file("hello.schema", HELLO);
            let served = leak(schema_with(&[], &[hello]));
            let (rt, real, mut inst) = boot("bar B { text \"x\" }\n", &[], served);
            assert!(real.services.take_diagnostics().is_empty());
            inst.shutdown();
            assert!(real.services.take_diagnostics().is_empty());
            real.shutdown(&rt);
        }
    }
}
