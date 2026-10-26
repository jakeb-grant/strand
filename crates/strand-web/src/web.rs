//! The JS boundary (wasm only, docs/live-examples.md): fonts and the
//! icon theme's files supplied once, then a [`Live`] per run of an
//! example. Errors cross as strings (a boot's is the compiler's
//! diagnostics, rendered as `strand check` renders them). A frame is
//! composited into a buffer the run keeps, and the page reads it
//! through a view on the module's memory (`frame_ptr`, `frame_len`):
//! nothing is copied across the boundary.

use std::cell::RefCell;
use std::sync::Arc;

use wasm_bindgen::prelude::*;

use crate::live::{Example, install_icons};
use crate::shell::Shell;
use crate::steps::{Advance, Player, Step};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(s: &str);
}

thread_local! {
    static FONTS: RefCell<Vec<Arc<Vec<u8>>>> = const { RefCell::new(Vec::new()) };
    static ICONS: RefCell<Vec<(String, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
}

/// A panic is written to the console (it would be "unreachable" else).
#[wasm_bindgen(start)]
pub fn start() {
    std::panic::set_hook(Box::new(|info| {
        console_error(&format!("strand-web: {info}"))
    }));
}

/// A font file text is shaped with, for every run booted after.
#[wasm_bindgen]
pub fn add_font(bytes: Vec<u8>) {
    FONTS.with(|f| f.borrow_mut().push(Arc::new(bytes)));
}

/// One icon theme file, by its path under the icon base
/// (`Adwaita/index.theme`); [`icons_ready`] supplies those added.
#[wasm_bindgen]
pub fn add_icon(path: String, bytes: Vec<u8>) {
    ICONS.with(|f| f.borrow_mut().push((path, bytes)));
}

/// The icon theme files added are the icons there are, from now on.
#[wasm_bindgen]
pub fn icons_ready() {
    install_icons(ICONS.with(|f| std::mem::take(&mut *f.borrow_mut())));
}

/// The module's linear memory now, in bytes.
#[wasm_bindgen]
pub fn memory_bytes() -> f64 {
    (core::arch::wasm32::memory_size(0) * 65536) as f64
}

fn js(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// A steps section as JSON: `[{"text", "kind", "acts"}]`, or why not.
#[wasm_bindgen]
pub fn parse_steps(text: &str) -> Result<String, JsValue> {
    let lines = crate::steps::parse(text).map_err(js)?;
    let v: Vec<serde_json::Value> = lines
        .iter()
        .map(
            |l| serde_json::json!({ "text": l.text, "kind": l.step.kind(), "acts": l.step.acts() }),
        )
        .collect();
    Ok(serde_json::Value::Array(v).to_string())
}

/// An example running: its shell, its steps and the frame last
/// composited.
#[wasm_bindgen]
#[derive(Debug)]
pub struct Live {
    shell: Shell,
    player: Player,
    frame: Vec<u8>,
}

fn files_of(json: &str) -> Result<Option<Vec<(String, String)>>, JsValue> {
    if json.is_empty() {
        return Ok(None);
    }
    let v: Vec<(String, String)> = serde_json::from_str(json).map_err(js)?;
    Ok(Some(v))
}

#[wasm_bindgen]
impl Live {
    /// Boots `example` (the JSON its figure carries) with `files` (JSON
    /// `[[name, text]]`, or empty for its own) in light or dark mode,
    /// with reduced motion or not, to its first tick.
    pub fn boot(
        example: &str,
        files: &str,
        dark: bool,
        reduced_motion: bool,
    ) -> Result<Live, JsValue> {
        let ex = Example::from_json(example).map_err(js)?;
        let files = files_of(files)?;
        let fonts = FONTS.with(|f| f.borrow().clone());
        let (shell, player) = ex
            .boot(files.as_deref(), dark, reduced_motion, fonts)
            .map_err(|e| JsValue::from_str(&e.text))?;
        Ok(Live {
            shell,
            player,
            frame: Vec::new(),
        })
    }

    /// One tick of the example's steps: the index of the step it ran in,
    /// or -1 when every step has run (and no tick ran).
    pub fn advance(&mut self) -> Result<i32, JsValue> {
        match self.player.advance(&mut self.shell).map_err(js)? {
            Advance::Ticked(i) => Ok(i as i32),
            Advance::Done => Ok(-1),
        }
    }

    /// One tick, outside the steps.
    pub fn tick(&mut self) -> Result<(), JsValue> {
        self.shell.tick().map_err(js)
    }

    pub fn at_rest(&self) -> bool {
        self.shell.at_rest()
    }

    /// The tick about to run.
    pub fn k(&self) -> u32 {
        self.shell.k()
    }

    /// The tick about to run's time from the clock's start, ms.
    pub fn now_ms(&self) -> f64 {
        self.shell
            .now()
            .saturating_sub(crate::shell::T0)
            .as_secs_f64()
            * 1000.0
    }

    /// `set <path> <value>` now, as a step would.
    pub fn set(&mut self, path: &str, value: &str) -> Result<(), JsValue> {
        self.shell.set(path, value).map_err(js)
    }

    /// The step at `index` (of [`parse_steps`]'s list) now, outside the
    /// steps' order: a reader's press of its button. Only a `set` acts.
    pub fn act(&mut self, index: usize) -> Result<(), JsValue> {
        let step = self.player.lines().get(index).map(|l| l.step.clone());
        match step {
            Some(Step::Set { path, value }) => self.shell.set(&path, &value).map_err(js),
            _ => Err(js(format!("step {index} is not a `set`"))),
        }
    }

    pub fn get(&self, path: &str) -> Result<String, JsValue> {
        self.shell.get(path).map_err(js)
    }

    pub fn pointer_move(&mut self, x: f32, y: f32) -> Result<(), JsValue> {
        self.shell.pointer_move([x, y]).map_err(js)
    }

    pub fn pointer_button(&mut self, pressed: bool) -> Result<(), JsValue> {
        self.shell.pointer_button(pressed).map_err(js)
    }

    pub fn pointer_leave(&mut self) -> Result<(), JsValue> {
        self.shell.pointer_leave().map_err(js)
    }

    /// The screen's scale (physical pixels a logical one).
    pub fn scale(&self) -> f64 {
        self.shell.surfaces().screen.scale
    }

    /// Frames painted so far: the page draws again when this grows.
    pub fn painted(&self) -> f64 {
        self.shell.painted() as f64
    }

    /// The part of the screen the shown surfaces hold, physical pixels:
    /// `[x, y, width, height]`, or empty with none shown.
    pub fn view(&self) -> Vec<u32> {
        self.shell
            .surfaces()
            .view()
            .map(|v| v.to_vec())
            .unwrap_or_default()
    }

    /// Composites the screen's `x, y, w, h` (physical) into the frame
    /// buffer, straight RGBA, `w × h`.
    pub fn compose(&mut self, x: u32, y: u32, w: u32, h: u32) {
        self.shell
            .surfaces()
            .composite_into([x, y, w.max(1), h.max(1)], &mut self.frame);
    }

    /// Where the frame buffer lies in the module's memory: valid until
    /// the next call into the module.
    pub fn frame_ptr(&self) -> u32 {
        self.frame.as_ptr() as u32
    }

    /// The frame buffer's length, bytes.
    pub fn frame_len(&self) -> u32 {
        self.frame.len() as u32
    }

    /// Keeps each frame painted from now on (its time and its pixels'
    /// hash, as the site's media keep eval's), for the checks.
    pub fn record(&mut self, on: bool) {
        self.shell.record = on;
        self.shell.paints.clear();
    }

    /// The frames kept: JSON `[[surface, t_ms, w, h, hash]]`.
    pub fn paints(&self) -> String {
        let v: Vec<serde_json::Value> = self
            .shell
            .paints
            .iter()
            .map(|p| {
                serde_json::json!([
                    p.surface.0,
                    p.t.as_secs_f64() * 1000.0,
                    p.size[0],
                    p.size[1],
                    p.hash
                ])
            })
            .collect();
        serde_json::Value::Array(v).to_string()
    }

    /// The notes the run left (runtime errors, what the web does not
    /// do), one per line.
    pub fn notes(&self) -> String {
        let mut out = self.shell.notes.join("\n");
        for k in self.shell.unsupported() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("a `{k}` is not shown on the web"));
        }
        out
    }

    pub fn warnings(&self) -> String {
        self.shell.warnings.clone()
    }
}
