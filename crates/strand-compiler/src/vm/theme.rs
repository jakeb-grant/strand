//! The VM's side of theming: `material(seed:)`, `material(image:)` and
//! `import(…)` through `strand-theme`, and the files they read.
//!
//! One [`ThemeHost`] lives as long as the [`crate::instantiate::Instance`]
//! (a reload keeps it, so a wallpaper being quantised is not started
//! again). `material(image:)` asks its [`Quantiser`], which answers from
//! a `stat` and quantises off-thread; when a job finishes, the worker
//! wakes a core task that bumps [`ThemeHost`]'s generation signal, which
//! every `material(image:)` and file `import` reads, so the palette
//! re-evaluates (and the token table with it) in the next flush. A file
//! the watcher saw change does the same through
//! [`ThemeHost::files_changed`].

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use strand_core::{Error, Runtime, Signal};
use strand_scene::Color;
use strand_theme::image::{Lookup, Quantiser};
use strand_theme::{Options, Palette, Variant};

/// Set by the quantiser's worker thread; awaited by the host's task.
#[derive(Default)]
struct Notify {
    fired: bool,
    waker: Option<Waker>,
}

struct Wait(Arc<Mutex<Notify>>);

impl Future for Wait {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut n = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if std::mem::take(&mut n.fired) {
            Poll::Ready(())
        } else {
            n.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// What `material(image:)` and `import()` share (see the module docs).
pub struct ThemeHost {
    /// Started on the first `material(image:)`.
    quantiser: RefCell<Option<Quantiser>>,
    cache_dir: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    generation: Signal<u64>,
    notify: Arc<Mutex<Notify>>,
    /// Wallpapers looked up (to watch, with their link targets).
    images: RefCell<BTreeSet<PathBuf>>,
    /// Files `import()` read.
    imports: RefCell<BTreeSet<PathBuf>>,
    /// The watch list changed since [`ThemeHost::take_files_changed`].
    files_dirty: Cell<bool>,
}

impl std::fmt::Debug for ThemeHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThemeHost")
            .field("images", &self.images.borrow().len())
            .finish_non_exhaustive()
    }
}

impl ThemeHost {
    /// `cache_dir`: where wallpaper seeds are kept (`None`: memory only);
    /// `config_dir`: what relative paths are read against.
    pub fn new(rt: &Runtime, cache_dir: Option<PathBuf>, config_dir: Option<PathBuf>) -> Rc<Self> {
        let host = Rc::new(ThemeHost {
            quantiser: RefCell::new(None),
            cache_dir,
            config_dir,
            generation: rt.signal(0u64),
            notify: Arc::default(),
            images: RefCell::default(),
            imports: RefCell::default(),
            files_dirty: Cell::new(false),
        });
        // The task that takes finished quantiser jobs: idle (no wakeups)
        // until the worker fires `notify`. Spawned here, outside any
        // scope, so no reload or re-evaluation cancels it.
        let notify = host.notify.clone();
        let weak = Rc::downgrade(&host);
        let wrt = rt.downgrade();
        rt.untrack(|rt| {
            rt.spawn(async move {
                loop {
                    Wait(notify.clone()).await;
                    let (Some(host), Some(rt)) = (weak.upgrade(), wrt.upgrade()) else {
                        return Ok(());
                    };
                    let any = host
                        .quantiser
                        .borrow_mut()
                        .as_mut()
                        .is_some_and(Quantiser::poll);
                    if any {
                        host.bump(&rt);
                    }
                }
            })
        });
        host
    }

    /// `~/x` against `$HOME`, a relative path against the config
    /// directory.
    pub fn resolve(&self, path: &str) -> PathBuf {
        strand_theme::import::resolve(path, self.config_dir.as_deref())
    }

    fn bump(&self, rt: &Runtime) {
        if let Ok(g) = self.generation.get_untracked(rt) {
            let _ = self.generation.set(rt, g.wrapping_add(1));
        }
    }

    /// `material(seed:, …)`.
    pub fn material_seed(&self, seed: Color, opts: Options) -> Palette {
        strand_theme::from_seed(seed, opts)
    }

    /// `material(image:, …)`: the palette of the wallpaper's seed, or a
    /// pending one holding the last palette made from an image.
    pub fn material_image(
        self: &Rc<Self>,
        rt: &Runtime,
        path: &str,
        opts: Options,
    ) -> Result<ImagePalette, Error> {
        self.generation.get(rt)?;
        let path = self.resolve(path);
        if self.images.borrow_mut().insert(path.clone()) {
            self.files_dirty.set(true);
        }
        let mut q = self.quantiser.borrow_mut();
        if q.is_none() {
            match Quantiser::new(self.cache_dir.clone()) {
                Ok(new) => {
                    let notify = self.notify.clone();
                    new.set_waker(move || {
                        let mut n = notify.lock().unwrap_or_else(PoisonError::into_inner);
                        n.fired = true;
                        if let Some(w) = n.waker.take() {
                            w.wake();
                        }
                    });
                    *q = Some(new);
                }
                Err(e) => return Ok(ImagePalette::Failed(e.to_string(), None)),
            }
        }
        let Some(quantiser) = q.as_mut() else {
            return Ok(ImagePalette::Failed("no quantiser".into(), None));
        };
        let palette = |c: Color| strand_theme::from_seed(c, opts);
        Ok(match quantiser.lookup(&path) {
            Lookup::Ready(c) => ImagePalette::Ready(palette(c)),
            Lookup::Pending { last } => ImagePalette::Pending(last.map(palette)),
            Lookup::Failed { error, last } => ImagePalette::Failed(error, last.map(palette)),
        })
    }

    /// `import(source)`.
    pub fn import(&self, rt: &Runtime, source: &str) -> Result<Palette, String> {
        if let Some(file) = strand_theme::import::file_of(source, self.config_dir.as_deref()) {
            let _ = self.generation.get(rt);
            if self.imports.borrow_mut().insert(file) {
                self.files_dirty.set(true);
            }
        }
        strand_theme::import(source, self.config_dir.as_deref()).map_err(|e| e.to_string())
    }

    /// The watcher saw these files change: anything read from them is
    /// read again (quantised again only if the content changed).
    pub fn files_changed(&self, rt: &Runtime, paths: &[PathBuf]) -> bool {
        let mut hit = false;
        {
            let images = self.images.borrow();
            let imports = self.imports.borrow();
            let mut q = self.quantiser.borrow_mut();
            for p in paths {
                if images.contains(p) {
                    if let Some(q) = q.as_mut() {
                        q.invalidate(p);
                    }
                    hit = true;
                }
                hit |= imports.contains(p);
            }
        }
        if hit {
            self.bump(rt);
        }
        hit
    }

    /// Wallpapers and imported files read so far.
    pub fn files(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        (
            self.images.borrow().iter().cloned().collect(),
            self.imports.borrow().iter().cloned().collect(),
        )
    }

    /// Whether [`ThemeHost::files`] grew since the last call.
    pub fn take_files_changed(&self) -> bool {
        self.files_dirty.replace(false)
    }

    /// Waits for running quantiser jobs (tests, `strand check`), then
    /// lets the next flush see their results.
    pub fn wait_images(&self, rt: &Runtime, timeout: std::time::Duration) -> bool {
        let done = self
            .quantiser
            .borrow_mut()
            .as_mut()
            .is_none_or(|q| q.wait(timeout));
        self.bump(rt);
        done
    }

    /// How many images were decoded and quantised.
    pub fn quantised(&self) -> usize {
        self.quantiser
            .borrow()
            .as_ref()
            .map_or(0, Quantiser::quantised)
    }

    /// The config directory relative paths are read against.
    pub fn config_dir(&self) -> Option<&Path> {
        self.config_dir.as_deref()
    }
}

/// [`ThemeHost::material_image`]'s answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ImagePalette {
    Ready(Palette),
    /// Being quantised: the last image palette, to hold meanwhile.
    Pending(Option<Palette>),
    /// No palette from this image (missing, not an image).
    Failed(String, Option<Palette>),
}

/// The variant named `name` (the schema's `Variant` enum).
pub fn variant(name: &str) -> Variant {
    Variant::from_name(name).unwrap_or_default()
}
