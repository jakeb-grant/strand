//! `strand-eval explain` (docs/eval.md, "Reading a failure"): a node's
//! frames as a text table, for reading a finding without the viewer:
//! where it was drawn and laid, its opacity, the motions moving it, the
//! axes authored, and the parts its offset is made of.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::checks::{Finding, Sev, Timeline, describe, select_seen};
use crate::select::Selector;
use crate::trace::{Row, RunTrace};

/// `probe::Motions` bit names, by bit.
pub const MOTIONS: [&str; 19] = [
    "PROP_SPRING",
    "OPACITY_SPRING",
    "GLIDE",
    "SIZE_SPRING",
    "ENTER",
    "EXIT",
    "KEYFRAMES",
    "SHARED_MORPH",
    "REVEAL",
    "STAGGER",
    "LEAN",
    "JELLY",
    "LIFT",
    "TIME",
    "FILL_SPRING",
    "INK_SPRING",
    "LINE_SPRING",
    "SCROLL",
    "FLING",
];

/// `probe::Axes` bit names, by bit.
pub const AXES: [&str; 7] = ["x", "y", "w", "h", "opacity", "scale", "rotate"];

pub fn motion_names(m: u32) -> Vec<&'static str> {
    MOTIONS
        .iter()
        .enumerate()
        .filter(|(i, _)| m & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect()
}

pub fn axes_names(a: u8) -> Vec<&'static str> {
    AXES.iter()
        .enumerate()
        .filter(|(i, _)| a & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect()
}

/// What to explain.
#[derive(Debug, Default)]
pub struct Ask {
    pub node: Option<Selector>,
    /// Ticks, inclusive.
    pub frames: Option<(u32, u32)>,
    /// Only this check's failing findings (their nodes, three frames
    /// either side of each).
    pub check: Option<String>,
}

fn pair(p: [f32; 2]) -> String {
    if p == [0.0, 0.0] {
        "·".into()
    } else {
        format!("{:+.2},{:+.2}", p[0], p[1])
    }
}

/// (eval-coverage) What a row painted itself, as the recording writes
/// it (`crate::paint`), or nothing.
fn paint(r: &Row) -> String {
    let Some(p) = &r.paint else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (name, v) in [
        ("bg", &p.bg),
        ("bd", &p.bd),
        ("sh", &p.sh),
        ("fg", &p.fg),
        ("pt", &p.pt),
    ] {
        if !v.is_empty() {
            parts.push(format!("{name}={}", v.join("/")));
        }
    }
    if !p.gl.is_empty() {
        parts.push(format!("gl={:?}", p.gl));
    }
    format!(" | {}", parts.join(" "))
}

fn line(k: u32, r: &Row, mark: bool) -> String {
    let mot = motion_names(r.mot);
    let inherited = motion_names(r.emot & !r.mot);
    let mut m = mot.join("+");
    if !inherited.is_empty() {
        if !m.is_empty() {
            m.push(' ');
        }
        m += &format!("^{}", inherited.join("+"));
    }
    format!(
        "{}{:>5} {:>8.2} {:>8.2} {:>7.2} {:>7.2} | {:>8.2} {:>8.2} {:>7.2} {:>7.2} | {:>5.3} {:>5.3} | {:<9} {:<3} | {:<22} | {:<14} | {} {} {} {} {}{}",
        if mark { '*' } else { ' ' },
        k,
        r.visual[0],
        r.visual[1],
        r.visual[2],
        r.visual[3],
        r.laid[0],
        r.laid[1],
        r.laid[2],
        r.laid[3],
        r.op,
        r.own_op,
        r.hidden.as_deref().unwrap_or("-"),
        r.text.as_deref().unwrap_or("-"),
        if m.is_empty() { "-".into() } else { m },
        {
            let a = axes_names(r.auth).join(",");
            if a.is_empty() { "-".into() } else { a }
        },
        pair(r.inherited),
        pair(r.written),
        pair(r.spring),
        pair(r.glide),
        pair(r.morph),
        paint(r),
    )
}

/// The table for `ask` over `trace`, its findings from `findings` (a
/// run's checks, when it was checked).
pub fn explain(trace: &RunTrace, findings: &[Finding], ask: &Ask) -> Result<String, String> {
    let tl = Timeline::new(trace);
    let picked: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.sev == Sev::Fail)
        .filter(|f| ask.check.as_deref().is_none_or(|c| f.check == c))
        .collect();
    let mut keys: Vec<String> = match &ask.node {
        Some(sel) => {
            let k = select_seen(trace, sel);
            if k.is_empty() {
                return Err(format!("`{sel}` names no node this run saw"));
            }
            k
        }
        None => {
            let set: BTreeSet<String> = picked.iter().filter_map(|f| f.key.clone()).collect();
            if set.is_empty() {
                return Err(match &ask.check {
                    Some(c) => {
                        format!("`{c}` has no failing finding on a node here: name one with --node")
                    }
                    None => "no failing finding names a node: name one with --node".into(),
                });
            }
            set.into_iter().collect()
        }
    };
    keys.sort();
    // The ticks to show.
    let near: Option<BTreeSet<u32>> = ask.check.as_ref().map(|_| {
        let mut s = BTreeSet::new();
        for f in &picked {
            if let Some(k) = f.k
                && let Some(i) = tl.at(k)
            {
                for j in i.saturating_sub(3)..=(i + 3).min(tl.ks.len().saturating_sub(1)) {
                    s.insert(tl.ks[j]);
                }
            }
        }
        s
    });
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} ({} Hz, scale {}, {}{}); geometry in logical px",
        trace.scenario.id,
        trace.scenario.hz,
        trace.scenario.scale,
        trace.build.rev,
        if trace.scenario.reduced_motion {
            ", motion reduced"
        } else {
            ""
        }
    );
    for key in &keys {
        let meta = trace.nodes.get(key);
        let _ = writeln!(
            out,
            "\n{} {}  [{}]",
            meta.map_or("?", |m| m.label.as_str()),
            meta.and_then(|m| m.origin.as_deref()).unwrap_or(""),
            key
        );
        let _ = writeln!(
            out,
            "     k   visual x        y       w       h |   laid x        y       w       h |    op   own | hidden    txt | motions (^ inherited)  | authored       | offset parts: inherited written spring glide morph | paint"
        );
        let mine: Vec<&Finding> = picked
            .iter()
            .copied()
            .filter(|f| f.key.as_deref() == Some(key.as_str()))
            .collect();
        let mut shown = 0;
        for (i, k) in tl.ks.iter().enumerate() {
            if let Some((a, b)) = ask.frames
                && (*k < a || *k > b)
            {
                continue;
            }
            if let Some(n) = &near
                && !n.contains(k)
            {
                continue;
            }
            // A frame that repeats the one before (no surface painted it)
            // is skipped.
            if i > 0 && tl.frames[i].k != *k {
                continue;
            }
            let Some(r) = tl.row(i, key) else { continue };
            let mark = mine.iter().any(|f| f.k == Some(*k));
            let _ = writeln!(out, "{}", line(*k, r, mark));
            shown += 1;
        }
        if shown == 0 {
            let _ = writeln!(out, "  (not drawn in these frames)");
        }
        for f in mine {
            let _ = writeln!(out, "  * {}", describe(f, trace));
        }
    }
    Ok(out)
}
