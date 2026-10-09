//! A raw PipeWire client of the audio tests' own on the `default`
//! metadata ([`MetaClient`]), to lose a metadata update on purpose. Apart
//! from `pipewire/mod.rs`, which the `strand` crate's tests share and
//! which so cannot name the `pipewire` crate.

use std::time::Duration;

use crate::pipewire::PipeWire;

/// Stops WirePlumber (`SIGSTOP`): it answers nothing until
/// [`resume_wireplumber`].
pub fn pause_wireplumber(pw: &PipeWire) {
    signal_wireplumber(pw, rustix::process::Signal::STOP);
}

/// Lets a paused WirePlumber run again (`SIGCONT`).
pub fn resume_wireplumber(pw: &PipeWire) {
    signal_wireplumber(pw, rustix::process::Signal::CONT);
}

fn signal_wireplumber(pw: &PipeWire, signal: rustix::process::Signal) {
    let pid = pw
        .wireplumber_pid()
        .and_then(|p| rustix::process::Pid::from_raw(p as i32))
        .expect("WirePlumber runs");
    rustix::process::kill_process(pid, signal).expect("WirePlumber is signalled");
}

/// What [`MetaClient`]'s thread is asked to do; each step ends with a
/// sync, whose `done` it reports.
enum MetaCmd {
    /// Set (or, with no value, clear) a key of the `default` metadata.
    Set {
        key: String,
        value: Option<String>,
    },
    /// Bind the `default` metadata once more.
    Bind,
    Quit,
}

/// A raw PipeWire client of our own on the `default` metadata (its own
/// thread and loop), to lose a metadata update on purpose.
///
/// PipeWire's `module-metadata` (1.0.5, `metadata_property`) forwards a
/// property event to a binding only `if (impl->pending == 0 ||
/// d->pong_seq != 0)`: while any bind of the metadata waits for the
/// owner's (WirePlumber's) pong, the bindings set up before get nothing.
/// [`MetaClient::lose`] makes that window last: with WirePlumber paused,
/// it sets a key (the daemon forwards the set to WirePlumber, which does
/// not read it yet), then binds the metadata again (the daemon queues a
/// ping to WirePlumber behind the set). Resumed, WirePlumber applies the
/// set and emits it before it answers the ping: every binding older than
/// the new one misses it. This client's newest binding gets it, and
/// [`MetaClient::keys`] shows it. The client stays connected until
/// dropped (its leaving is a client that goes).
pub struct MetaClient {
    tx: ::pipewire::channel::Sender<MetaCmd>,
    events: std::sync::mpsc::Receiver<()>,
    keys: Keys,
    thread: Option<std::thread::JoinHandle<()>>,
}

type Keys = std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>;

impl MetaClient {
    /// Connects and binds the `default` metadata (waiting until its keys
    /// are in).
    pub fn connect(pw: &PipeWire) -> MetaClient {
        let (tx, rx) = ::pipewire::channel::channel::<MetaCmd>();
        let (ev_tx, events) = std::sync::mpsc::channel();
        let keys = Keys::default();
        let thread_keys = keys.clone();
        let socket = pw.socket().display().to_string();
        let thread = std::thread::spawn(move || meta_client(socket, rx, ev_tx, thread_keys));
        let client = MetaClient {
            tx,
            events,
            keys,
            thread: Some(thread),
        };
        client.wait("first bind");
        client
    }

    fn send(&self, cmd: MetaCmd) -> Result<(), String> {
        self.tx
            .send(cmd)
            .map_err(|_| "the metadata client is gone".to_owned())
    }

    fn wait(&self, what: &str) {
        self.wait_for(what).unwrap_or_else(|e| panic!("{e}"));
    }

    fn wait_for(&self, what: &str) -> Result<(), String> {
        self.events
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| format!("the metadata client: no {what}: {e}"))
    }

    /// Sets (`Some`) or clears (`None`) `key` while a bind of the metadata
    /// waits for WirePlumber's pong, so the bindings set up before (the
    /// service's) miss it. WirePlumber is paused meanwhile, and runs again
    /// on return.
    pub fn lose(&self, pw: &PipeWire, key: &str, value: Option<&str>) {
        pause_wireplumber(pw);
        let held = (|| {
            self.send(MetaCmd::Set {
                key: key.to_owned(),
                value: value.map(str::to_owned),
            })?;
            // The daemon has forwarded the set to WirePlumber.
            self.wait_for("set")?;
            self.send(MetaCmd::Bind)
        })();
        // The bind's sync waits for WirePlumber's pong (the client is busy
        // until then): a margin for the daemon to read the bind and queue
        // its ping (a test of the service then checks the update was
        // missed, so a slow daemon fails loudly, not quietly).
        std::thread::sleep(Duration::from_millis(200));
        resume_wireplumber(pw);
        held.unwrap_or_else(|e| panic!("{e}"));
        self.wait("bind");
    }

    /// The `default` metadata's keys (subject 0) as this client's
    /// bindings last saw them.
    pub fn keys(&self) -> std::collections::BTreeMap<String, String> {
        self.keys.lock().expect("the keys").clone()
    }
}

impl Drop for MetaClient {
    fn drop(&mut self) {
        let _ = self.tx.send(MetaCmd::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// [`MetaClient`]'s thread.
fn meta_client(
    socket: String,
    rx: ::pipewire::channel::Receiver<MetaCmd>,
    events: std::sync::mpsc::Sender<()>,
    keys: Keys,
) {
    use ::pipewire::context::ContextRc;
    use ::pipewire::main_loop::MainLoopRc;
    use ::pipewire::metadata::{Metadata, MetadataListener};
    use ::pipewire::properties::PropertiesBox;
    use ::pipewire::registry::{GlobalObject, RegistryRc};
    use ::pipewire::types::ObjectType;
    use std::cell::RefCell;
    use std::rc::Rc;

    ::pipewire::init();
    let mainloop = MainLoopRc::new(None).expect("a loop");
    let context = ContextRc::new(&mainloop, None).expect("a context");
    let mut props = PropertiesBox::new();
    props.insert("remote.name", socket.as_str());
    let core = context.connect_rc(Some(props)).expect("a connection");
    let registry: RegistryRc = core.get_registry_rc().expect("the registry");

    let bindings: Rc<RefCell<Vec<(Metadata, MetadataListener)>>> = Rc::default();
    let global: Rc<RefCell<Option<GlobalObject<PropertiesBox>>>> = Rc::default();
    // The sync whose `done` the test thread waits for.
    let waiting: Rc<RefCell<Option<i32>>> = Rc::default();
    // Called from the loop (C): nothing below panics; a failure shows as
    // the test thread's timeout.
    let synced = {
        let (core, waiting) = (core.clone(), waiting.clone());
        move || {
            if let Ok(seq) = core.sync(0) {
                *waiting.borrow_mut() = Some(seq.seq());
            }
        }
    };
    let bind = {
        let (registry, bindings, global) = (registry.clone(), bindings.clone(), global.clone());
        move || {
            let g = global.borrow();
            let Some(g) = g.as_ref() else { return };
            let Ok(meta) = registry.bind::<Metadata, _>(g) else {
                return;
            };
            let keys = keys.clone();
            let listener = meta
                .add_listener_local()
                .property(move |subject, key, _ty, value| {
                    if subject == 0
                        && let Ok(mut k) = keys.lock()
                    {
                        match (key, value) {
                            (None, _) => k.clear(),
                            (Some(key), Some(v)) => {
                                k.insert(key.to_owned(), v.to_owned());
                            }
                            (Some(key), None) => {
                                k.remove(key);
                            }
                        }
                    }
                    0
                })
                .register();
            bindings.borrow_mut().push((meta, listener));
        }
    };

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (global, bind, synced) = (global.clone(), bind.clone(), synced.clone());
            move |g| {
                let default = g.type_ == ObjectType::Metadata
                    && g.props
                        .as_ref()
                        .and_then(|p| p.get("metadata.name"))
                        .is_some_and(|n| n == "default");
                if !default || global.borrow().is_some() {
                    return;
                }
                *global.borrow_mut() = Some(g.to_owned());
                bind();
                synced();
            }
        })
        .register();

    let _core_listener = core
        .add_listener_local()
        .done({
            let waiting = waiting.clone();
            move |id, seq| {
                let mut w = waiting.borrow_mut();
                if id == ::pipewire::core::PW_ID_CORE && *w == Some(seq.seq()) {
                    *w = None;
                    let _ = events.send(());
                }
            }
        })
        .register();

    let _cmds = rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |cmd| match cmd {
            MetaCmd::Set { key, value } => {
                if let Some((first, _)) = bindings.borrow().first() {
                    first.set_property(
                        0,
                        &key,
                        value.as_ref().map(|_| "Spa:String:JSON"),
                        value.as_deref(),
                    );
                }
                synced();
            }
            MetaCmd::Bind => {
                bind();
                synced();
            }
            MetaCmd::Quit => mainloop.quit(),
        }
    });
    mainloop.run();
}
