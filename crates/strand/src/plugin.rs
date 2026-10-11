//! The plugin builder (docs/architecture.md, "Plugin builder";
//! decisions.md m5-plugins): `strand::Shell::builder().service::<S>()
//! .run()` is the whole `strand` command line with the plugin's own
//! services served beside the builtins. `strand`'s own `main` is the
//! builder with no services.
//!
//! **Unstable**, git-only: [`Shell`], [`Shell::builder`],
//! [`Builder::service`] and [`Builder::run`] are the whole surface.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::OnceLock;

use strand_compiler::schema::{Schema, plugins};
use strand_core::Runtime;
use strand_services::{DynService, Service, Services};

/// A Strand shell binary: `strand` itself, or a plugin binary that adds
/// its own `#[service]` types (`strand::Shell::builder()
/// .service::<MyService>().run()`).
#[derive(Debug)]
pub struct Shell;

impl Shell {
    /// A builder with no plugin services.
    pub fn builder() -> Builder {
        Builder { list: Vec::new() }
    }
}

/// The services a plugin binary adds; see [`Shell`].
#[derive(Debug)]
pub struct Builder {
    list: Vec<Registration>,
}

impl Builder {
    /// Serve `S` beside the builtin services: its schema joins the
    /// checked one and `--write-schemas` writes it for `strand check`
    /// and the LSP.
    pub fn service<S: Service>(mut self) -> Self {
        self.list.push(Registration::of::<S>());
        self
    }

    /// Run the command line (`run`, `check`, `call`, …, and
    /// `--write-schemas`), as `strand` does. A registration that cannot
    /// be served fails every command before it runs.
    pub fn run(self) -> ExitCode {
        let me = binary_name();
        if let Some(w) = setup_warning(probe_mimalloc(), probe_thp_off()) {
            eprintln!("{me}: warning: {w}");
        }
        let checked = match self.list.first() {
            Some(r) => (r.validate)(&self.list),
            None => Ok(()),
        };
        let checked = checked.and_then(|()| install(&EXTRAS, self.list));
        if let Err(e) = checked {
            eprintln!("{me}: {e}");
            return ExitCode::FAILURE;
        }
        crate::entry::main()
    }
}

/// One plugin service, as function pointers (monomorphised only in a
/// plugin binary).
#[derive(Clone, Copy)]
pub(crate) struct Registration {
    pub name: &'static str,
    pub schema: fn() -> &'static str,
    pub register: fn(&Services, &Runtime) -> Rc<dyn DynService>,
    /// [`validate`] and [`write_main`]: reached only through a
    /// registration, so `strand`, which has none, does not link them
    /// (decisions.md m5-plugins, "size").
    pub validate: fn(&[Registration]) -> Result<(), PluginError>,
    pub write: fn(&[Registration]) -> ExitCode,
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Registration").field(&self.name).finish()
    }
}

impl Registration {
    pub(crate) fn of<S: Service>() -> Registration {
        fn register<S: Service>(services: &Services, rt: &Runtime) -> Rc<dyn DynService> {
            services.register::<S>(rt).dynamic()
        }
        Registration {
            name: S::NAME,
            schema: S::schema,
            register: register::<S>,
            validate,
            write: write_main,
        }
    }
}

/// The process's plugin services, set once by [`Builder::run`].
static EXTRAS: OnceLock<Vec<Registration>> = OnceLock::new();

/// The plugin services this binary serves (none in `strand`). Reading
/// them before [`Builder::run`] installs them fixes them empty, and the
/// install then fails, loudly.
pub(crate) fn extras() -> &'static [Registration] {
    EXTRAS.get_or_init(Vec::new)
}

/// Store `list` in `slot`, which must not have been set or read.
fn install(slot: &OnceLock<Vec<Registration>>, list: Vec<Registration>) -> Result<(), PluginError> {
    slot.set(list).map_err(|_| PluginError::ReadEarly)
}

/// Why a plugin binary cannot run, or `--write-schemas` cannot write.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PluginError {
    /// A builtin's name, or registered twice.
    DuplicateService(String),
    /// Not `[a-z_][a-z0-9_]*`.
    BadName(String),
    /// Its text does not apply over the builtin and earlier plugin texts.
    Schema {
        service: String,
        errors: Vec<String>,
    },
    /// Entries of its text without a `///` doc (`--write-schemas` only).
    Undocumented {
        service: String,
        entries: Vec<String>,
    },
    NoDataDir,
    Write {
        path: PathBuf,
        error: String,
    },
    /// The list was read before `run` installed it (a bug).
    ReadEarly,
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateService(n) => {
                write!(
                    f,
                    "service `{n}` is already served (a builtin, or registered twice)"
                )
            }
            Self::BadName(n) => write!(f, "`{n}` is not a service name ([a-z_][a-z0-9_]*)"),
            Self::Schema { service, errors } => write!(
                f,
                "the schema of `{service}` does not apply: {}",
                errors.join("; ")
            ),
            Self::Undocumented { service, entries } => write!(
                f,
                "the schema of `{service}` has entries without a `///` doc, so nothing was \
                 written: {}",
                entries.join(", ")
            ),
            Self::NoDataDir => f.write_str("no data directory (set XDG_DATA_HOME or HOME)"),
            Self::Write { path, error } => write!(f, "cannot write {}: {error}", path.display()),
            Self::ReadEarly => f.write_str("the plugin services were read before `run`"),
        }
    }
}

fn valid_name(n: &str) -> bool {
    let mut chars = n.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_lowercase())
        && chars.all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The builtin schema with the linked service crates' texts.
fn builtin_schema() -> Result<Schema, PluginError> {
    Schema::builtin_with(&strand_services::schemas()).map_err(|(i, errors)| PluginError::Schema {
        service: format!("builtin #{i}"),
        errors: errors.iter().map(ToString::to_string).collect(),
    })
}

/// Each text applied over the builtin schema and the texts before it,
/// with the schema it extended (`--write-schemas` checks its docs).
fn layered(list: &[Registration]) -> Result<Vec<(Schema, Schema)>, PluginError> {
    let mut s = builtin_schema()?;
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for r in list {
        if !valid_name(r.name) {
            return Err(PluginError::BadName(r.name.into()));
        }
        if seen.contains(&r.name) || s.services.contains_key(r.name) {
            return Err(PluginError::DuplicateService(r.name.into()));
        }
        seen.push(r.name);
        let mut next = s.clone();
        let schema_err = |errors: Vec<String>| PluginError::Schema {
            service: r.name.into(),
            errors,
        };
        next.extend((r.schema)())
            .map_err(|e| schema_err(e.iter().map(ToString::to_string).collect()))?;
        if !next.services.contains_key(r.name) {
            return Err(schema_err(vec![format!(
                "it declares no `service {}`",
                r.name
            )]));
        }
        if next.provisional.len() < s.provisional.len() {
            return Err(schema_err(vec!["it redeclares a builtin service".into()]));
        }
        out.push((s, next.clone()));
        s = next;
    }
    Ok(out)
}

/// The checks `run` makes before any command: the names, then each text.
fn validate(list: &[Registration]) -> Result<(), PluginError> {
    layered(list).map(drop)
}

/// `--write-schemas`: write each registered service's schema into the
/// plugin schema directory, then remove this binary's files for services
/// it no longer registers. What it did is printed.
pub(crate) fn write_schemas_main(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("{} --write-schemas: takes no arguments", binary_name());
        return ExitCode::FAILURE;
    }
    match extras().first() {
        Some(r) => (r.write)(extras()),
        None => {
            println!("this binary registers no plugin services; nothing written");
            ExitCode::SUCCESS
        }
    }
}

/// [`write_schemas`] as this binary, into [`plugins::dir`].
fn write_main(list: &[Registration]) -> ExitCode {
    let me = binary_name();
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .ok()
        .map(|p| p.display().to_string());
    let mut out = String::new();
    let r = write_schemas(list, plugins::dir().as_deref(), exe.as_deref(), &mut out);
    print!("{out}");
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{me}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// [`write_schemas_main`] into `dir` as `exe` (`None`: unknown, so
/// nothing is removed), saying what it did in `out`.
fn write_schemas(
    list: &[Registration],
    dir: Option<&Path>,
    exe: Option<&str>,
    out: &mut String,
) -> Result<(), PluginError> {
    use std::fmt::Write as _;
    let dir = dir.ok_or(PluginError::NoDataDir)?;
    for (r, (before, after)) in list.iter().zip(layered(list)?) {
        let had = before.undocumented();
        let mut entries: Vec<String> = after
            .undocumented()
            .into_iter()
            .filter(|e| !had.contains(e))
            .collect();
        entries.sort();
        entries.dedup();
        if !entries.is_empty() {
            return Err(PluginError::Undocumented {
                service: r.name.into(),
                entries,
            });
        }
    }
    let failed = |path: &Path, e: std::io::Error| PluginError::Write {
        path: path.to_path_buf(),
        error: e.to_string(),
    };
    std::fs::create_dir_all(dir).map_err(|e| failed(dir, e))?;
    let header = plugins::header(exe.unwrap_or("unknown"));
    for r in list {
        let path = dir.join(format!("{}.schema", r.name));
        let tmp = dir.join(format!(".{}.schema.tmp", r.name));
        std::fs::write(&tmp, format!("{header}{}", (r.schema)())).map_err(|e| failed(&tmp, e))?;
        std::fs::rename(&tmp, &path).map_err(|e| failed(&path, e))?;
        let _ = writeln!(out, "wrote {}", path.display());
    }
    // This binary's files for services it no longer registers go; any
    // other file it does not register is listed, never touched.
    let (files, _) = plugins::load(dir);
    for (path, text) in files {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if list.iter().any(|r| r.name == stem) {
            continue;
        }
        match (plugins::writer(&text), exe) {
            (Some(w), Some(me)) if w == me => {
                std::fs::remove_file(&path).map_err(|e| failed(&path, e))?;
                let _ = writeln!(out, "removed {}", path.display());
            }
            (w, _) => {
                let _ = writeln!(
                    out,
                    "left alone: {} (written by {})",
                    path.display(),
                    w.unwrap_or("unknown")
                );
            }
        }
    }
    Ok(())
}

/// The warning when the binary runs without `strand::process_setup!()`:
/// not on mimalloc, or with transparent huge pages on.
pub(crate) fn setup_warning(mimalloc: bool, thp_off: bool) -> Option<&'static str> {
    (!(mimalloc && thp_off)).then_some(
        "this binary does not invoke `strand::process_setup!()` at its crate root \
         (mimalloc, and transparent huge pages off), so it uses more memory than `strand`",
    )
}

/// Whether a fresh heap block is mimalloc's.
fn probe_mimalloc() -> bool {
    let p = Box::into_raw(Box::new([0u8; 64]));
    // SAFETY: `p` is a live allocation; the query only reads metadata.
    let ours = unsafe { libmimalloc_sys::mi_is_in_heap_region(p.cast()) };
    // SAFETY: `p` came from `Box::into_raw` above.
    drop(unsafe { Box::from_raw(p) });
    ours
}

/// Whether THP is off for the process (true when the kernel cannot say).
fn probe_thp_off() -> bool {
    // SAFETY: `prctl` with integer arguments only.
    let got = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
    got != 0
}

/// argv[0]'s file name, for messages.
pub(crate) fn binary_name() -> String {
    std::env::args_os()
        .next()
        .and_then(|a| {
            Path::new(&a)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "strand".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const HELLO: &str = "/// The plugin test's service.\nservice hello {\n  /// A greeting.\n  greeting: text\n  /// A count.\n  count: int rw\n}\n";

    fn reg(name: &'static str, schema: fn() -> &'static str) -> Registration {
        fn none(_: &Services, _: &Runtime) -> Rc<dyn DynService> {
            unreachable!("never registered")
        }
        Registration {
            name,
            schema,
            register: none,
            validate,
            write: write_main,
        }
    }

    fn hello() -> Registration {
        reg("hello", || HELLO)
    }

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Temp {
            let p =
                std::env::temp_dir().join(format!("strand-plugin-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            Temp(p)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_builtin_name_or_a_duplicate_is_refused() {
        let battery = reg(
            "battery",
            || "/// B.\nservice battery {\n  /// x.\n  x: int\n}\n",
        );
        assert_eq!(
            validate(&[battery]),
            Err(PluginError::DuplicateService("battery".into()))
        );
        // `clock` has no store but is a builtin name all the same.
        let clock = reg("clock", || "");
        assert_eq!(
            validate(&[clock]),
            Err(PluginError::DuplicateService("clock".into()))
        );
        assert_eq!(
            validate(&[hello(), hello()]),
            Err(PluginError::DuplicateService("hello".into()))
        );
        assert_eq!(validate(&[hello()]), Ok(()));
    }

    #[test]
    fn a_bad_name_is_refused() {
        for bad in ["Hello", "9lives", "", "my-svc", "a.b", "../x"] {
            assert_eq!(
                validate(&[reg(bad, || "")]),
                Err(PluginError::BadName(bad.into())),
                "{bad}"
            );
        }
        assert!(valid_name("_x9"));
    }

    #[test]
    fn a_text_that_does_not_apply_is_refused() {
        let broken = reg("hello", || "service hello {");
        assert!(matches!(
            validate(&[broken]),
            Err(PluginError::Schema { service, .. }) if service == "hello"
        ));
        // A text declaring another name.
        let other = reg("hi", || HELLO);
        assert!(matches!(
            validate(&[other]),
            Err(PluginError::Schema { .. })
        ));
    }

    #[test]
    fn an_undocumented_text_writes_nothing() {
        let t = Temp::new("undoc");
        let bare = reg("bare", || "service bare {\n  x: int\n}\n");
        let mut out = String::new();
        let err = write_schemas(&[hello(), bare], Some(&t.0), Some("/x"), &mut out).unwrap_err();
        let PluginError::Undocumented { service, entries } = err else {
            panic!("{err:?}");
        };
        assert_eq!(service, "bare");
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(!t.0.join("hello.schema").exists(), "nothing written");
    }

    #[test]
    fn written_files_carry_the_header_and_land_by_rename() {
        let t = Temp::new("write");
        let mut out = String::new();
        write_schemas(&[hello()], Some(&t.0), Some("/opt/a/my-shell"), &mut out).unwrap();
        let path = t.0.join("hello.schema");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(plugins::writer(&text), Some("/opt/a/my-shell"));
        assert_eq!(plugins::body(&text), HELLO);
        assert!(out.contains(&format!("wrote {}", path.display())), "{out}");
        let names: Vec<_> = std::fs::read_dir(&t.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["hello.schema"], "no temporary left");
        assert_eq!(
            write_schemas(&[hello()], None, None, &mut out),
            Err(PluginError::NoDataDir)
        );
    }

    /// Only this exact binary's files for services it dropped are
    /// removed; another path's (even with the same file name) and a file
    /// with no header are listed and left; an unknown writer removes
    /// nothing.
    #[test]
    fn only_this_binarys_stale_files_are_removed() {
        let t = Temp::new("stale");
        std::fs::create_dir_all(&t.0).unwrap();
        let me = "/opt/a/my-shell";
        let put = |name: &str, exe: Option<&str>| {
            let svc = name.trim_end_matches(".schema");
            let text = format!("/// D.\nservice {svc} {{\n  /// x.\n  x: int\n}}\n");
            let head = exe.map(plugins::header).unwrap_or_default();
            std::fs::write(t.0.join(name), format!("{head}{text}")).unwrap();
        };
        put("dropped.schema", Some(me));
        put("theirs.schema", Some("/opt/b/my-shell"));
        put("hand.schema", None);
        let mut out = String::new();
        write_schemas(&[hello()], Some(&t.0), None, &mut out).unwrap();
        assert!(t.0.join("dropped.schema").exists(), "unknown writer: {out}");
        let mut out = String::new();
        write_schemas(&[hello()], Some(&t.0), Some(me), &mut out).unwrap();
        assert!(!t.0.join("dropped.schema").exists(), "{out}");
        assert!(out.contains("removed "), "{out}");
        assert!(t.0.join("theirs.schema").exists());
        assert!(t.0.join("hand.schema").exists());
        assert!(
            out.contains("theirs.schema (written by /opt/b/my-shell)"),
            "{out}"
        );
        assert!(out.contains("hand.schema (written by unknown)"), "{out}");
    }

    /// Reading the list before `run` installs it fixes it empty, and
    /// the install then fails rather than serving half a schema.
    #[test]
    fn the_list_read_before_install_is_caught() {
        let slot = OnceLock::new();
        assert!(slot.get_or_init(Vec::new).is_empty());
        assert_eq!(install(&slot, vec![hello()]), Err(PluginError::ReadEarly));
        let fresh = OnceLock::new();
        assert_eq!(install(&fresh, vec![hello()]), Ok(()));
        assert_eq!(fresh.get().map(Vec::len), Some(1));
    }

    #[test]
    fn the_setup_warning_needs_both_halves() {
        assert_eq!(setup_warning(true, true), None);
        for (m, t) in [(false, true), (true, false), (false, false)] {
            let w = setup_warning(m, t).unwrap();
            assert!(w.contains("strand::process_setup!()"), "{w}");
        }
        // The lib's tests run with the macro.
        assert!(probe_mimalloc() && probe_thp_off());
    }
}
