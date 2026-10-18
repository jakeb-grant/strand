//! The run trace (`trace.json`, schema 1; docs/eval.md "Trace"): what a
//! scenario's run drew, frame by frame and node by node, keyed so runs
//! and builds compare. Geometry is in logical pixels, quantised to 1/64;
//! `op` is effective opacity (the node's times its ancestors'); `auth` is
//! effective authored axes (its own or an ancestor's). The digest hashes
//! every deterministic field and none of the wall times.

use std::collections::BTreeMap;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The trace format's version.
pub const SCHEMA: u32 = 1;

/// The columns of a row before `fx`, `fy` (still read).
const OLD_COLS: usize = 34;
/// The columns of a row before `fit`, `ink` (still read).
const FLIGHT_COLS: usize = 36;

/// The node row's columns, in order (`Row`'s array form).
pub const COLS: [&str; 38] = [
    "key", "s", "x", "y", "w", "h", "vx", "vy", "vw", "vh", "lx", "ly", "lw", "lh", "op", "oo",
    "sx", "sy", "rot", "mot", "emot", "auth", "hid", "txt", "ix", "iy", "wx", "wy", "spx", "spy",
    "gx", "gy", "mx", "my", "fx", "fy", "fit", "ink",
];

/// A rect, `[x, y, w, h]`.
pub type Rect = [f32; 4];

/// The whole run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunTrace {
    pub schema: u32,
    pub scenario: ScenarioInfo,
    pub build: BuildInfo,
    pub surfaces: Vec<SurfaceInfo>,
    /// Every node seen, by key.
    pub nodes: BTreeMap<String, NodeMeta>,
    /// The steps as they ran.
    pub events: Vec<EventRec>,
    pub segments: Vec<Segment>,
    /// Each `rest` and whether it reached rest.
    pub rests: Vec<RestRec>,
    pub expects: Vec<ExpectRec>,
    /// Actions and commands the shell ran (recorded, never executed).
    pub commands: Vec<CommandRec>,
    pub cols: Vec<String>,
    /// The ticks that painted something.
    pub frames: Vec<Frame>,
    pub digest: String,
    /// Rows and input regions moved by their surface's origin since it
    /// was first placed ([`RunTrace::on_screen`]); never written.
    #[serde(skip)]
    pub on_screen: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScenarioInfo {
    pub id: String,
    pub file: String,
    pub hz: u32,
    pub scale: f64,
    pub backend: String,
    pub text: String,
    /// The whole run had motion reduced (the twin, or the scenario's own
    /// `reduce_motion`).
    pub reduced_motion: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub rev: String,
    pub dirty: bool,
    pub profile: String,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInfo {
    pub id: u32,
    /// `bar Top`, `popup #card`.
    pub label: String,
    pub kind: String,
    /// The node key of its root.
    pub root: String,
    pub attached: u32,
    pub detached: Option<u32>,
}

/// What a node is (it does not change while it lives).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeMeta {
    /// `box#bars`, `text("iiii")`, `row@shell.strand:9`.
    pub label: String,
    pub kind: String,
    /// `file:line:col`.
    pub origin: Option<String>,
    /// The parent's key.
    pub parent: Option<String>,
    pub id: Option<String>,
    pub text: Option<String>,
    /// Its `enter` pose when it was first seen.
    pub enter: Option<Pose>,
    /// Its shared-element `morph:` name, when written as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub morph: Option<String>,
    /// A text that cuts itself with an ellipsis (`ellipsis:`) rather
    /// than wrapping when its box is narrower than it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ellipsis: bool,
}

/// The parts of an `enter` pose the checks read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    pub opacity: Option<f32>,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub scale: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EventRec {
    /// The tick it acted at.
    pub k: u32,
    pub line: u32,
    pub text: String,
    pub segment: Option<usize>,
}

/// From a step that changes something to the next rest
/// (docs/eval.md, "Checks").
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub name: String,
    /// The line that opened it.
    pub line: u32,
    /// The tick before its first change (none for the boot segment).
    pub s: Option<u32>,
    /// Its last tick: the rest's, or the run's last.
    pub e: u32,
    /// It ended at a rest that settled.
    pub settled: bool,
    /// Motion was reduced at some tick of it.
    pub reduced_motion: bool,
    /// The rate at its end.
    pub hz: u32,
    pub intends: Vec<IntendRec>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IntendRec {
    pub line: u32,
    pub what: String,
    pub keys: Vec<String>,
    /// `probe::Axes` bits.
    pub axes: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RestRec {
    pub line: u32,
    /// The first tick it ran and how many it ran.
    pub k: u32,
    pub ticks: u32,
    pub max: u32,
    pub settled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExpectRec {
    pub k: u32,
    pub line: u32,
    pub text: String,
    /// still | visible | hidden | moves.
    pub what: String,
    pub keys: Vec<String>,
    pub axis: Option<String>,
    pub from: u32,
    pub segment: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommandRec {
    pub k: u32,
    pub text: String,
}

/// A held text at a frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HeldRec {
    pub key: u64,
    pub text: String,
    pub paints: u32,
}

/// A surface's costs over one tick (every paint of it in the tick).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub paints: u32,
    pub damage_px: u64,
    pub raster_px: u64,
    /// Of its last paint.
    pub items: u32,
    pub records: u32,
    pub layout_passes: u32,
    pub layout_nodes: u32,
    pub text_pending: u32,
    pub text_held: u32,
    /// Wall-clock, advisory: never in the digest, never gating.
    pub flatten_us: u64,
    pub text_wait_us: u64,
    pub raster_us: u64,
}

/// One tick that painted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub k: u32,
    pub t_ns: u64,
    /// Surfaces painted this tick.
    pub painted: Vec<u32>,
    /// Surfaces still wanting a frame after it.
    pub wants: Vec<u32>,
    /// Surfaces shown, and their logical sizes.
    pub sizes: BTreeMap<u32, [f32; 2]>,
    pub fresh: BTreeMap<u32, bool>,
    pub cost: BTreeMap<u32, Cost>,
    pub held_text: Vec<HeldRec>,
    /// The image each painted surface showed (`frames/<hash>.png`).
    pub png: BTreeMap<u32, String>,
    /// What each painted surface asked of the compositor besides pixels.
    #[serde(default)]
    pub regions: BTreeMap<u32, Regions>,
    /// The nodes of each painted surface, at its last paint of the tick.
    pub n: Vec<Row>,
    /// The screen's scale at this tick (a `scale` step changes it); 0 in
    /// a trace from before it was recorded: the scenario's.
    #[serde(default)]
    pub scale: f64,
    /// Logic's diff with changes in it, or input, reached the renderer
    /// since the frame before: what is laid out anew may be theirs.
    #[serde(default)]
    pub driven: bool,
    /// Where each shown surface's buffer lies on the output (logical
    /// px): a popup the compositor re-centres as it grows moves.
    #[serde(default)]
    pub origins: BTreeMap<u32, [f32; 2]>,
    /// With `compositor_poses`, the pose each painted surface commits
    /// with this frame (`Renderer::delegated_pose`), which the
    /// compositor applies to it: absent is none (at rest).
    #[serde(default)]
    pub poses: BTreeMap<u32, PoseRec>,
}

/// A surface pose the compositor applies (`strand_scene::SurfacePose`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PoseRec {
    /// Multiplied into the surface's alpha.
    pub opacity: f32,
    /// The surface drawn this much of its size, its top-left kept.
    pub scale: f32,
    /// Logical px its top-left moves.
    pub x: f32,
    pub y: f32,
}

/// A surface's `Painter` regions after a frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Regions {
    /// Where it takes input, logical `[x, y, w, h]`; `None` keeps the
    /// surface manager's (its box inside the shadow overhang).
    pub input: Option<Vec<Rect>>,
    /// Where it is opaque, buffer pixels.
    pub opaque: Vec<[i64; 4]>,
    /// Where it asks for a blur behind it, buffer pixels.
    pub blur: Vec<BlurRec>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BlurRec {
    pub rect: [i64; 4],
    pub radii: [f32; 4],
    pub radius: f32,
}

/// One node in one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub key: String,
    pub surface: u32,
    pub drawn: Rect,
    pub visual: Rect,
    pub laid: Rect,
    /// Effective opacity (own times its ancestors').
    pub op: f32,
    /// Its own opacity.
    pub own_op: f32,
    pub scale: [f32; 2],
    pub rotate: f32,
    /// Its own `probe::Motions`, and with its ancestors'.
    pub mot: u32,
    pub emot: u32,
    /// Effective authored `probe::Axes`.
    pub auth: u8,
    /// `opacity0`, `scale0`, `inert`.
    pub hidden: Option<String>,
    /// `S` shaped, `E` estimate, `I` a stand-in.
    pub text: Option<String>,
    pub inherited: [f32; 2],
    pub written: [f32; 2],
    pub spring: [f32; 2],
    pub glide: [f32; 2],
    pub morph: [f32; 2],
    /// Of `inherited`, the part in flight: the ancestors' springs and
    /// glides, a drag's lift included (what they have written at rest
    /// left out). The sandbox's sum of their `spring` and `glide`.
    pub flight: [f32; 2],
    /// A text cut to its box: the line box width it asked to be shaped
    /// for (its unbounded layout is wider than its box).
    pub fit: Option<f32>,
    /// A text's glyphs as drawn: the ink of the layout it drew, clipped
    /// where the frame clips it (`[x, y, w, h]`, before transforms).
    pub ink: Option<Rect>,
}

/// `v` to 1/64.
pub fn q(v: f32) -> f32 {
    (v * 64.0).round() / 64.0
}

impl Serialize for Row {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(COLS.len()))?;
        seq.serialize_element(&self.key)?;
        seq.serialize_element(&self.surface)?;
        for r in [self.drawn, self.visual, self.laid] {
            for v in r {
                seq.serialize_element(&q(v))?;
            }
        }
        seq.serialize_element(&self.op)?;
        seq.serialize_element(&self.own_op)?;
        seq.serialize_element(&self.scale[0])?;
        seq.serialize_element(&self.scale[1])?;
        seq.serialize_element(&self.rotate)?;
        seq.serialize_element(&self.mot)?;
        seq.serialize_element(&self.emot)?;
        seq.serialize_element(&self.auth)?;
        seq.serialize_element(&self.hidden)?;
        seq.serialize_element(&self.text)?;
        for p in [
            self.inherited,
            self.written,
            self.spring,
            self.glide,
            self.morph,
            self.flight,
        ] {
            seq.serialize_element(&q(p[0]))?;
            seq.serialize_element(&q(p[1]))?;
        }
        seq.serialize_element(&self.fit.map(q))?;
        seq.serialize_element(&self.ink.map(|r| r.map(q)))?;
        seq.end()
    }
}

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v: Vec<serde_json::Value> = Vec::deserialize(d)?;
        // A trace from before `fx`, `fy` were recorded (an A/B against
        // an earlier revision) reads with nothing in flight.
        if ![COLS.len(), FLIGHT_COLS, OLD_COLS].contains(&v.len()) {
            return Err(D::Error::custom(format!(
                "a node row has {} columns, not {}",
                v.len(),
                COLS.len()
            )));
        }
        let f = |i: usize| -> Result<f32, D::Error> {
            v[i].as_f64()
                .map(|x| x as f32)
                .ok_or_else(|| D::Error::custom(format!("column {} is not a number", COLS[i])))
        };
        let u = |i: usize| -> Result<u64, D::Error> {
            v[i].as_u64()
                .ok_or_else(|| D::Error::custom(format!("column {} is not a count", COLS[i])))
        };
        let s = |i: usize| -> Result<Option<String>, D::Error> {
            match &v[i] {
                serde_json::Value::Null => Ok(None),
                serde_json::Value::String(t) => Ok(Some(t.clone())),
                _ => Err(D::Error::custom(format!("column {} is not text", COLS[i]))),
            }
        };
        let rect =
            |i: usize| -> Result<Rect, D::Error> { Ok([f(i)?, f(i + 1)?, f(i + 2)?, f(i + 3)?]) };
        let pair = |i: usize| -> Result<[f32; 2], D::Error> { Ok([f(i)?, f(i + 1)?]) };
        Ok(Row {
            key: s(0)?.ok_or_else(|| D::Error::custom("a node row without a key"))?,
            surface: u(1)? as u32,
            drawn: rect(2)?,
            visual: rect(6)?,
            laid: rect(10)?,
            op: f(14)?,
            own_op: f(15)?,
            scale: pair(16)?,
            rotate: f(18)?,
            mot: u(19)? as u32,
            emot: u(20)? as u32,
            auth: u(21)? as u8,
            hidden: s(22)?,
            text: s(23)?,
            inherited: pair(24)?,
            written: pair(26)?,
            spring: pair(28)?,
            glide: pair(30)?,
            morph: pair(32)?,
            flight: if v.len() > OLD_COLS {
                pair(34)?
            } else {
                [0.0, 0.0]
            },
            fit: match v.get(36) {
                None | Some(serde_json::Value::Null) => None,
                Some(_) => Some(f(36)?),
            },
            ink: match v.get(37) {
                None | Some(serde_json::Value::Null) => None,
                Some(r) => {
                    let r: Vec<f32> = serde_json::from_value(r.clone())
                        .map_err(|_| D::Error::custom("column ink is not a rect"))?;
                    match r.as_slice() {
                        [x, y, w, h] => Some([*x, *y, *w, *h]),
                        _ => return Err(D::Error::custom("column ink is not a rect")),
                    }
                }
            },
        })
    }
}

impl RunTrace {
    /// An empty trace of the current schema.
    pub fn new(scenario: ScenarioInfo, build: BuildInfo) -> RunTrace {
        RunTrace {
            schema: SCHEMA,
            scenario,
            build,
            cols: COLS.iter().map(|c| c.to_string()).collect(),
            ..RunTrace::default()
        }
    }

    /// The hash of everything deterministic the run drew: each frame's
    /// tick, surfaces and nodes, its deterministic costs and held texts,
    /// the segments and rests. Wall times and images are left out.
    pub fn compute_digest(&self) -> String {
        let mut h = blake3::Hasher::new();
        let mut put = |v: &dyn erased::Json| {
            h.update(&v.bytes());
            h.update(b"\n");
        };
        for f in &self.frames {
            let det: BTreeMap<u32, Cost> = f
                .cost
                .iter()
                .map(|(s, c)| {
                    (
                        *s,
                        Cost {
                            flatten_us: 0,
                            text_wait_us: 0,
                            raster_us: 0,
                            ..*c
                        },
                    )
                })
                .collect();
            put(&(f.k, f.t_ns, &f.painted, &f.wants, &f.sizes));
            put(&(&det, &f.held_text, &f.n));
            put(&f.regions);
            put(&(f.scale, f.driven));
            put(&(&f.origins, &f.poses));
        }
        put(&self.segments);
        put(&self.rests);
        put(&self.nodes);
        format!("blake3:{}", h.finalize().to_hex())
    }

    /// The screen's scale at `f`: its own, else the scenario's, else 1.
    pub fn scale_at(&self, f: &Frame) -> f64 {
        if f.scale > 0.0 {
            f.scale
        } else if self.scenario.scale > 0.0 {
            self.scenario.scale
        } else {
            1.0
        }
    }

    /// The least scale any frame was drawn at (the scenario's when none
    /// was): the coarsest physical pixel, in logical px, is its inverse.
    pub fn min_scale(&self) -> f64 {
        self.frames
            .iter()
            .map(|f| self.scale_at(f))
            .fold(None, |m: Option<f64>, s| Some(m.map_or(s, |m| m.min(s))))
            .unwrap_or(if self.scenario.scale > 0.0 {
                self.scenario.scale
            } else {
                1.0
            })
    }

    /// Where each surface was first placed (its first frame's origin).
    pub fn first_origins(&self) -> BTreeMap<u32, [f32; 2]> {
        let mut first = BTreeMap::new();
        for f in &self.frames {
            for (s, o) in &f.origins {
                first.entry(*s).or_insert(*o);
            }
        }
        first
    }

    /// How far surface `s` has moved at `f` from where it was first
    /// placed (`first`, [`RunTrace::first_origins`]).
    pub fn moved(first: &BTreeMap<u32, [f32; 2]>, f: &Frame, s: u32) -> [f32; 2] {
        match (f.origins.get(&s), first.get(&s)) {
            (Some(o), Some(a)) => [o[0] - a[0], o[1] - a[1]],
            _ => [0.0, 0.0],
        }
    }

    /// The trace as the eye sees it, where the compositor moves or poses
    /// a surface:
    ///
    /// - a surface that moves on the output (a content-sized popup the
    ///   compositor re-centres as it grows): every row's boxes and ink,
    ///   and every input region, moved by how far its surface has moved
    ///   since it was first placed, so content render keeps in place on
    ///   screen while its surface moves under it reads as still;
    /// - a surface pose the compositor applies (`compositor_poses`): its
    ///   opacity multiplied into every row's, and into the root's own;
    ///   its scale (about the surface's top-left) and offset applied to
    ///   every visual box and to the root's scale; the rows authored on
    ///   every axis it moves, as a pose authors its subtree.
    ///
    /// `None` when no surface moved and none was posed (the trace as it
    /// is).
    pub fn on_screen(&self) -> Option<RunTrace> {
        let first = self.first_origins();
        let moves = self.frames.iter().any(|f| {
            !f.poses.is_empty()
                || f.origins
                    .keys()
                    .any(|s| RunTrace::moved(&first, f, *s) != [0.0, 0.0])
        });
        if !moves {
            return None;
        }
        let roots: BTreeMap<u32, String> = self
            .surfaces
            .iter()
            .map(|s| (s.id, s.root.clone()))
            .collect();
        let mut t = self.clone();
        t.on_screen = true;
        for f in &mut t.frames {
            let shift: BTreeMap<u32, [f32; 2]> = f
                .origins
                .keys()
                .map(|s| (*s, RunTrace::moved(&first, f, *s)))
                .collect();
            let by = |s: u32| shift.get(&s).copied().unwrap_or([0.0, 0.0]);
            let mv = |r: &mut Rect, d: [f32; 2]| {
                r[0] += d[0];
                r[1] += d[1];
            };
            for r in &mut f.n {
                if let Some(p) = f.poses.get(&r.surface) {
                    let k = p.scale;
                    r.op *= p.opacity;
                    r.visual = [
                        p.x + k * r.visual[0],
                        p.y + k * r.visual[1],
                        k * r.visual[2],
                        k * r.visual[3],
                    ];
                    if roots.get(&r.surface) == Some(&r.key) {
                        r.own_op *= p.opacity;
                        r.scale = [r.scale[0] * k, r.scale[1] * k];
                    }
                    use strand_render::probe::Axes;
                    let mut auth = Axes::NONE;
                    if p.opacity != 1.0 {
                        auth |= Axes::OPACITY;
                    }
                    if k != 1.0 {
                        auth |= Axes::X | Axes::Y | Axes::W | Axes::H | Axes::SCALE;
                    }
                    if p.x != 0.0 || p.y != 0.0 {
                        auth |= Axes::X | Axes::Y;
                    }
                    r.auth |= auth.0;
                }
                let d = by(r.surface);
                mv(&mut r.drawn, d);
                mv(&mut r.visual, d);
                mv(&mut r.laid, d);
                if let Some(ink) = r.ink.as_mut() {
                    mv(ink, d);
                }
            }
            for (s, reg) in &mut f.regions {
                let d = by(*s);
                for r in reg.input.iter_mut().flatten() {
                    mv(r, d);
                }
            }
        }
        Some(t)
    }

    /// Fills in the digest.
    pub fn seal(&mut self) {
        self.digest = self.compute_digest();
    }

    /// The trace as JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    pub fn from_json(text: &str) -> Result<RunTrace, String> {
        serde_json::from_str(text).map_err(|e| e.to_string())
    }
}

mod erased {
    /// Something hashed as its JSON.
    pub trait Json {
        fn bytes(&self) -> Vec<u8>;
    }

    impl<T: serde::Serialize> Json for T {
        fn bytes(&self) -> Vec<u8> {
            serde_json::to_vec(self).unwrap_or_default()
        }
    }
}
