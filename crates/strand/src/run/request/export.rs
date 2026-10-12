//! `strand export` over the socket (S-tools; decisions.md m5-tools, "the
//! CLI", step 1): the live token table, overlays included, rendered as
//! the offline mount renders it (`crate::export::produce`), so the two
//! agree. A template that reads `.light()`/`.dark()` holds the
//! instance's other-mode memo while the reply is built; one whose other
//! mode's palette is still loading waits, retried after each tick's diff
//! (`Shell::after_diff`), and is answered once it resolves or fails.

use std::time::{Duration, Instant};

use serde_json::json;
use strand_compiler::instantiate::Instance;
use strand_compiler::instantiate::theme::{OtherModeHold, is_pending};
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
    pending: Pending,
}

/// A template export that reads the other mode, from its hold to its
/// answer.
#[derive(Debug)]
struct Pending {
    source: Source,
    template: TemplateBox,
    since: Instant,
    /// Its hold, on the program it was taken on: a reload meanwhile
    /// disposed of that memo, and the release lets go of nothing the
    /// reloaded program's outputs hold.
    hold: OtherModeHold,
}

/// The compiled template (no `Debug` of its own worth printing).
struct TemplateBox(Template);

impl std::fmt::Debug for TemplateBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Template")
    }
}

impl Pending {
    /// Takes the hold (shared with the outputs) until the answer.
    fn begin(inst: &Instance, source: Source, template: Template) -> Pending {
        Pending {
            source,
            template: TemplateBox(template),
            since: Instant::now(),
            hold: inst.hold_other_mode(),
        }
    }

    /// How long until it is answered whatever its other mode does: the
    /// logic loop wakes by then, so an idle shell still answers at 30 s.
    fn wait(&self, now: Instant) -> Duration {
        (self.since + PENDING_WAIT).saturating_duration_since(now)
    }

    /// The answer, its hold let go, once the other mode is ready (or
    /// failed, or waited too long); else itself, to wait.
    fn settle(self, inst: &Instance) -> Result<proto::Answer, Pending> {
        let ans = match inst.other_table() {
            Err(e) if is_pending(&e) && self.since.elapsed() < PENDING_WAIT => return Err(self),
            Err(e) => proto::refused(format!("the other mode has no palette: {e}")),
            Ok(other) => reply(produce(
                inst,
                &Format::Template(Default::default()),
                Some((&self.source, &self.template.0)),
                other.as_ref(),
            )),
        };
        inst.release_other_mode(self.hold);
        Ok(ans)
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
        let pending = Pending::begin(&self.inst, source, template);
        if let Some(w) = self.try_export(Waiting { from, pending }) {
            self.exports.push(w);
        }
    }

    /// Answers `w` if it can be now; else gives it back to wait.
    fn try_export(&mut self, w: Waiting) -> Option<Waiting> {
        match w.pending.settle(&self.inst) {
            Ok(ans) => {
                self.answer(w.from, ans);
                None
            }
            Err(pending) => Some(Waiting {
                from: w.from,
                pending,
            }),
        }
    }

    /// When the logic loop must wake for a waiting export's deadline
    /// (none waiting: `None`).
    pub(in crate::run) fn exports_wait(&self, now: Instant) -> Option<Duration> {
        self.exports.iter().map(|w| w.pending.wait(now)).min()
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

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use strand_compiler::SourceMap;
    use strand_compiler::instantiate::Storage;
    use strand_compiler::reconcile::Build;
    use strand_compiler::vm::Value;
    use strand_compiler::vm::schema_host::SchemaHost;
    use strand_core::Runtime;

    use super::*;

    const THEME: &str = "let scheme: Palette = material(seed: #7aa2f7, dark: system.dark)\n\
                         use palette scheme\n";

    fn build(src: &str) -> Build {
        let mut map = SourceMap::new();
        map.add("theme.strand", src.to_string());
        Build::compile(None, map).unwrap_or_else(|d| panic!("{d:#?}"))
    }

    fn mount(src: &str) -> (Runtime, Instance) {
        let b = build(src);
        let rt = Runtime::new();
        let host = Rc::new(SchemaHost::mock(&rt, &b.program.types));
        host.set(&rt, "system.dark", Value::Bool(true)).unwrap();
        let inst = Instance::from_build(&rt, &b, host, Storage::none());
        assert!(inst.flush().errors.is_empty());
        (rt, inst)
    }

    fn source(text: &str) -> (Source, Template) {
        let s = Source {
            name: "t.tmpl".into(),
            text: text.into(),
        };
        let t = compile_template(&s).unwrap();
        (s, t)
    }

    /// Over IPC, a template reading `.light()` with no mounted output
    /// renders as offline does, and the memo is gone after the reply.
    #[test]
    fn the_hold_is_let_go_after_the_reply() {
        let (_rt, inst) = mount(THEME);
        let (s, t) = source("{{ $surface }} {{ $surface.light() }}\n");
        let p = Pending::begin(&inst, s.clone(), t.clone());
        assert_eq!(inst.other_mode_holders(), 1);
        let ans = p.settle(&inst).unwrap();
        assert_eq!(inst.other_mode_holders(), 0, "the memo is gone");
        let text = ans["text"].as_str().unwrap().to_string();
        let parts: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(parts.len(), 2, "{ans}");
        assert_ne!(parts[0], parts[1], "{text}");
        // As the offline mount renders it.
        let other = inst.other_table().unwrap();
        let offline = produce(
            &inst,
            &Format::Template(Default::default()),
            Some((&s, &t)),
            other.as_ref(),
        )
        .unwrap();
        assert_eq!(text.as_bytes(), offline);
    }

    /// (Review round 2, item 5) A waiting request's deadline is when the
    /// loop must wake, so an idle shell answers at 30 s.
    #[test]
    fn a_waiting_export_wakes_the_loop_at_its_deadline() {
        let (_rt, inst) = mount(THEME);
        let (s, t) = source("{{ $surface.light() }}");
        let p = Pending::begin(&inst, s, t);
        assert_eq!(p.wait(p.since), PENDING_WAIT);
        assert_eq!(p.wait(p.since + PENDING_WAIT), Duration::ZERO);
        let _ = p.settle(&inst);
    }

    /// A request waiting across a reload lets go of its own hold only.
    #[test]
    fn a_reload_meanwhile_keeps_the_outputs_hold() {
        let with_output =
            format!("{THEME}file \"/nonexistent/x.txt\" {{ text: $surface.light() ?? $fg }}\n");
        let (_rt, mut inst) = mount(&with_output);
        let (s, t) = source("{{ $surface.light() }}");
        let p = Pending::begin(&inst, s, t);
        let _ = inst.take_outputs();
        assert_eq!(inst.other_mode_holders(), 2);
        inst.reload(&build(&format!("{with_output}export state x = 1\n")));
        assert!(inst.flush().errors.is_empty());
        let _ = inst.take_outputs();
        assert_eq!(inst.other_mode_holders(), 1, "the reloaded output's");
        let ans = p.settle(&inst).unwrap();
        assert_eq!(ans["ok"], true, "{ans}");
        assert_eq!(inst.other_mode_holders(), 1, "still the output's");
    }
}
