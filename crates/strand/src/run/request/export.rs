//! `strand export` over the socket (S-tools; decisions.md m5-tools, "the
//! CLI", step 1): the live token table, overlays included, rendered as
//! the offline mount renders it (`crate::export::produce`), so the two
//! agree. A template that reads `.light()`/`.dark()` holds the
//! instance's other-mode memo while the reply is built; one whose other
//! mode's palette is still loading waits, retried after each tick's diff
//! (`Shell::after_diff`), and is answered once it resolves or fails.

use std::time::{Duration, Instant};

use serde_json::json;
use strand_compiler::instantiate::theme::is_pending;
use strand_compiler::template::Template;
use strand_core::ipc as proto;

use super::{Caller, Shell};
use crate::export::{Format, Source, compile_template, produce};

/// How long a request waits for a pending other-mode palette.
const PENDING_WAIT: Duration = Duration::from_secs(30);

/// A request waiting for its other mode's palette, holding the memo.
#[derive(Debug)]
pub(in crate::run) struct Waiting {
    from: Caller,
    source: Source,
    template: TemplateBox,
    since: Instant,
}

/// The compiled template (no `Debug` of its own worth printing).
struct TemplateBox(Template);

impl std::fmt::Debug for TemplateBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Template")
    }
}

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn export_request(
        &mut self,
        from: Caller,
        kind: &str,
        template: Option<String>,
        name: Option<String>,
    ) {
        let format = match kind {
            "json" => Format::Json,
            "base24" => Format::Base24,
            "template" => Format::Template(Default::default()),
            other => {
                let why = format!("`export`'s `kind` is json, base24 or template, not `{other}`");
                return self.answer(from, proto::refused(why));
            }
        };
        let Format::Template(_) = format else {
            let ans = reply(produce(&self.inst, &format, None, None));
            return self.answer(from, ans);
        };
        let Some(text) = template else {
            return self.answer(from, proto::refused("`export` needs `template`"));
        };
        let source = Source {
            name: name.unwrap_or_else(|| "template".into()),
            text,
        };
        let template = match compile_template(&source) {
            Ok(t) => t,
            Err(e) => return self.answer(from, proto::refused(e)),
        };
        if !template.reads_other_mode() {
            let ans = reply(produce(
                &self.inst,
                &format,
                Some((&source, &template)),
                None,
            ));
            return self.answer(from, ans);
        }
        // Held until the reply is built: shared with the outputs.
        self.inst.hold_other_mode();
        let w = Waiting {
            from,
            source,
            template: TemplateBox(template),
            since: Instant::now(),
        };
        if let Some(w) = self.try_export(w) {
            self.exports.push(w);
        }
    }

    /// Answers `w` if its other mode is ready (or failed, or waited too
    /// long), releasing its hold; else gives it back to wait.
    fn try_export(&mut self, w: Waiting) -> Option<Waiting> {
        let ans = match self.inst.other_table() {
            Err(e) if is_pending(&e) && w.since.elapsed() < PENDING_WAIT => return Some(w),
            Err(e) => proto::refused(format!("the other mode has no palette: {e}")),
            Ok(other) => reply(produce(
                &self.inst,
                &Format::Template(Default::default()),
                Some((&w.source, &w.template.0)),
                other.as_ref(),
            )),
        };
        self.inst.release_other_mode();
        self.answer(w.from, ans);
        None
    }

    /// After a tick's diff is sent: the exports waiting for their other
    /// mode, tried again.
    pub(in crate::run) fn retry_exports(&mut self) {
        if self.exports.is_empty() {
            return;
        }
        for w in std::mem::take(&mut self.exports) {
            if let Some(w) = self.try_export(w) {
                self.exports.push(w);
            }
        }
    }
}

fn reply(r: Result<Vec<u8>, String>) -> proto::Answer {
    match r {
        Ok(bytes) => json!({"ok": true, "text": String::from_utf8_lossy(&bytes)}),
        Err(e) => proto::refused(e),
    }
}
