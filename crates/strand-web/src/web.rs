//! The JS boundary (wasm only): fonts added once, then a [`WebShell`]
//! per config. Errors cross as strings (a load's is the compiler's
//! diagnostics, rendered as `strand check` renders them).

use std::cell::RefCell;
use std::sync::Arc;

use wasm_bindgen::prelude::*;

use crate::shell::{Options, Shell};
use crate::surfaces::Screen;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(s: &str);
}

thread_local! {
    static FONTS: RefCell<Vec<Arc<Vec<u8>>>> = const { RefCell::new(Vec::new()) };
}

/// A panic is written to the console (it would be "unreachable" else).
#[wasm_bindgen(start)]
pub fn start() {
    std::panic::set_hook(Box::new(|info| {
        console_error(&format!("strand-web: {info}"))
    }));
}

/// A font file text is shaped with, for every shell loaded after.
#[wasm_bindgen]
pub fn add_font(bytes: Vec<u8>) {
    FONTS.with(|f| f.borrow_mut().push(Arc::new(bytes)));
}

/// The module's linear memory now, in bytes.
#[wasm_bindgen]
pub fn memory_bytes() -> f64 {
    (core::arch::wasm32::memory_size(0) * 65536) as f64
}

/// A config running.
#[wasm_bindgen]
#[derive(Debug)]
pub struct WebShell {
    shell: Shell,
}

fn pairs(json: &str) -> Result<Vec<(String, String)>, JsValue> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let obj = v
        .as_object()
        .ok_or_else(|| JsValue::from_str("expected an object of name: text"))?;
    Ok(obj
        .iter()
        .map(|(k, v)| {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                o => o.to_string(),
            };
            (k.clone(), text)
        })
        .collect())
}

#[wasm_bindgen]
impl WebShell {
    /// `files`: JSON `{"shell.strand": "…"}`; `services`: JSON
    /// `{"battery.percent": 0.82}`. Boots to the first tick.
    pub fn load(
        files: &str,
        services: &str,
        width: u32,
        height: u32,
        scale: f64,
        hz: u32,
    ) -> Result<WebShell, JsValue> {
        let files = pairs(files)?;
        let services = pairs(services)?;
        let fonts = FONTS.with(|f| f.borrow().clone());
        let opts = Options {
            screen: Screen {
                width,
                height,
                scale,
            },
            hz,
            fonts,
        };
        Shell::load(&files, &services, opts)
            .map(|shell| WebShell { shell })
            .map_err(|e| JsValue::from_str(&e.text))
    }

    pub fn tick(&mut self) -> Result<(), JsValue> {
        self.shell.tick().map_err(|e| JsValue::from_str(&e))
    }

    /// Ticks until at rest, at most `max`; how many ran.
    pub fn rest(&mut self, max: u32) -> Result<u32, JsValue> {
        self.shell.rest(max).map_err(|e| JsValue::from_str(&e))
    }

    pub fn at_rest(&self) -> bool {
        self.shell.at_rest()
    }

    /// The tick about to run.
    pub fn k(&self) -> u32 {
        self.shell.k()
    }

    pub fn set(&mut self, path: &str, text: &str) -> Result<(), JsValue> {
        self.shell
            .set(path, text)
            .map_err(|e| JsValue::from_str(&e))
    }

    pub fn get(&self, path: &str) -> Result<String, JsValue> {
        self.shell.get(path).map_err(|e| JsValue::from_str(&e))
    }

    pub fn pointer_move(&mut self, x: f32, y: f32) -> Result<(), JsValue> {
        self.shell
            .pointer_move([x, y])
            .map_err(|e| JsValue::from_str(&e))
    }

    pub fn pointer_button(&mut self, pressed: bool) -> Result<(), JsValue> {
        self.shell
            .pointer_button(pressed)
            .map_err(|e| JsValue::from_str(&e))
    }

    pub fn pointer_leave(&mut self) -> Result<(), JsValue> {
        self.shell
            .pointer_leave()
            .map_err(|e| JsValue::from_str(&e))
    }

    /// The screen's physical width and height (the frame's size).
    pub fn frame_width(&self) -> u32 {
        let s = self.shell.surfaces().screen;
        (f64::from(s.width) * s.scale).round() as u32
    }

    pub fn frame_height(&self) -> u32 {
        let s = self.shell.surfaces().screen;
        (f64::from(s.height) * s.scale).round() as u32
    }

    /// The screen now, straight RGBA, `frame_width × frame_height`.
    pub fn frame(&self) -> Vec<u8> {
        self.shell.surfaces().composite().2
    }

    /// The screen's top `rows` physical rows now, straight RGBA,
    /// `frame_width` wide.
    pub fn frame_top(&self, rows: u32) -> Vec<u8> {
        self.shell.surfaces().composite_top(rows).2
    }

    /// How far down the shown surfaces reach, physical rows.
    pub fn content_rows(&self) -> u32 {
        let s = self.shell.surfaces();
        (f64::from(s.bottom()) * s.screen.scale).ceil() as u32
    }

    /// Frames painted so far: the page draws again when this grows.
    pub fn painted(&self) -> f64 {
        self.shell.painted() as f64
    }

    /// The notes the run left (runtime errors, what the web does not
    /// do), one per line, and the config's warnings.
    pub fn notes(&self) -> String {
        let mut out = self.shell.notes.join("\n");
        for k in self.shell.unsupported() {
            out.push_str(&format!("\na `{k}` is not shown on the web"));
        }
        out
    }

    pub fn warnings(&self) -> String {
        self.shell.warnings.clone()
    }
}
