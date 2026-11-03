//! `strand compositor-rules [dir] [--classic]`: the Hyprland layer rules a
//! config wants, for the user to paste into their Hyprland config.
//! Strand never applies them itself.
//!
//! First, one rule for every surface Strand makes ([`NO_ANIM_NAMESPACE`]):
//! no layer animation. Strand animates its own surfaces (`enter`, `exit`,
//! `morph`), and Hyprland animates a layer surface's map, unmap and moves
//! over them, a fade a panel's first frames are drawn through (decisions.md
//! compositor-motion). No protocol lets a client opt out.
//!
//! Then the blur ladder's second rung (design.md, "Blur ladder"). Hyprland
//! blurs layer surfaces through its own layer rules rather than
//! `ext-background-effect-v1`, so this
//! prints a rule for every surface whose tree asks for `blur`,
//! matched by the surface's stable `strand-<Name>` namespace.
//!
//! The query ([`blur_rules`]) walks a compiled [`Build`]: a layer surface
//! (`bar`, `panel`, `osd`) gets `blur` when a node of its own tree has
//! `blur`, and `blur_popups` when a popup nested in it does (xdg popups
//! have no namespace of their own; Hyprland blurs them through their
//! layer's rule). Components are followed into their bodies. A `lock` is
//! a session-lock surface, which layer rules do not reach.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use strand_compiler::diagnostic::{Style, render};
use strand_compiler::hir::DefId;
use strand_compiler::lower::{Element, ElementKind, Node, Program, Prop};
use strand_compiler::reconcile::Build;
use strand_compiler::source::{SourceMap, find_files};
use strand_scene::{NodeKind, Prop as SceneProp};

/// The alpha below which Hyprland does not blur (`ignore_alpha`): the
/// transparent margin around a rounded surface and its shadow (the
/// elevation tokens reach 0.4 at most) stay sharp, while the translucent
/// backgrounds `blur` goes with (0.72 and up in the examples) blur.
pub const IGNORE_ALPHA: &str = "0.5";

/// The namespaces of every layer surface Strand makes (`strand-<Name>`,
/// its scrims and click-away catchers too). Hyprland matches a rule's
/// pattern against the whole namespace (`RE2::FullMatch`), so a prefix
/// alone (`^strand-`) matches nothing.
pub const NO_ANIM_NAMESPACE: &str = "^strand-.*$";

/// The rule that keeps Hyprland's layer animations off Strand's surfaces,
/// as Hyprland's Lua config.
pub fn lua_no_anim() -> String {
    format!(
        "hl.layer_rule({{\n    name = \"strand-no-anim\",\n    match = {{ namespace = \"{}\" }},\n    no_anim = true,\n}})\n",
        lua_escape(NO_ANIM_NAMESPACE)
    )
}

/// The same as a classic hyprlang `layerrule` line.
pub fn classic_no_anim() -> String {
    "layerrule = noanim, ^(strand-.*)$\n".into()
}

/// One layer surface's blur rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// `strand-<Name>`, as `SurfaceSpec::namespace` names it.
    pub namespace: String,
    /// A node of the surface's own tree has `blur`.
    pub blur: bool,
    /// A popup nested in it has `blur`.
    pub blur_popups: bool,
}

/// The rules for `build`, one per namespace (bars on several monitors
/// share one), sorted by namespace; surfaces that ask for no blur get
/// none.
pub fn blur_rules(build: &Build) -> Vec<Rule> {
    let program = &*build.program;
    let mut found: BTreeMap<String, (bool, bool)> = BTreeMap::new();
    let mut walk = Walk {
        program,
        found: &mut found,
        stack: Vec::new(),
    };
    for file in &program.files {
        walk.nodes(&file.items, None);
    }
    found
        .into_iter()
        .filter(|(_, (b, p))| *b || *p)
        .map(|(namespace, (blur, blur_popups))| Rule {
            namespace,
            blur,
            blur_popups,
        })
        .collect()
}

/// The namespace a surface of `kind` named `name` gets (the same rule as
/// `strand_scene::SurfaceSpec::namespace`).
fn namespace(kind: NodeKind, name: Option<&str>) -> String {
    format!("strand-{}", name.unwrap_or(kind.name()))
}

/// Where a walk is: the layer surface it is in, and whether inside a
/// popup of it.
#[derive(Clone, Copy)]
struct At<'n> {
    layer: &'n str,
    popup: bool,
}

struct Walk<'p, 'f> {
    program: &'p Program,
    found: &'f mut BTreeMap<String, (bool, bool)>,
    /// The components being walked, against recursion (a component
    /// reached again from another surface is walked again there).
    stack: Vec<DefId>,
}

impl Walk<'_, '_> {
    fn mark(&mut self, at: Option<At<'_>>) {
        let Some(at) = at else { return };
        let entry = self.found.entry(at.layer.to_string()).or_default();
        if at.popup {
            entry.1 = true;
        } else {
            entry.0 = true;
        }
    }

    fn props(&mut self, props: &[Prop], at: Option<At<'_>>) {
        if props.iter().any(|p| p.prop == Some(SceneProp::Blur)) {
            self.mark(at);
        }
    }

    fn nodes(&mut self, nodes: &[Node], at: Option<At<'_>>) {
        for node in nodes {
            match node {
                Node::Element(e) => self.element(e, None, &[], at),
                Node::Surface(s) => self.element(&s.element, s.name.as_deref(), &s.body.nodes, at),
                Node::If { then, else_, .. } => {
                    self.nodes(then, at);
                    self.nodes(else_, at);
                }
                Node::For(f) => self.nodes(&f.body.nodes, at),
                Node::Match { arms, .. } => {
                    for arm in arms {
                        self.nodes(arm, at);
                    }
                }
                Node::When { props, .. } | Node::Pose { props, .. } => self.props(props, at),
                Node::Slot
                | Node::State(_)
                | Node::Let { .. }
                | Node::Handler(_)
                | Node::Timer(_)
                | Node::Set(_)
                | Node::Play(_) => {}
            }
        }
    }

    /// Where the tree of a surface element of `kind` is: a layer surface
    /// starts its own namespace; a popup belongs to the layer it is in; a
    /// lock (and a popup outside any layer) is not reached by layer rules.
    fn surface_at(
        &self,
        kind: &ElementKind,
        name: Option<&str>,
        at: Option<At<'_>>,
    ) -> Option<(String, bool)> {
        match kind {
            ElementKind::Builtin(k @ (NodeKind::Bar | NodeKind::Panel | NodeKind::Osd)) => {
                Some((namespace(*k, name), false))
            }
            ElementKind::Builtin(NodeKind::Popup) => at.map(|a| (a.layer.to_string(), true)),
            ElementKind::Builtin(NodeKind::Lock) => None,
            _ => at.map(|a| (a.layer.to_string(), a.popup)),
        }
    }

    /// An element: a surface element starts (or leaves) a namespace; a
    /// component is followed into its body. `name` is a top-level
    /// surface's declared name (nested surface elements have none) and
    /// `body` its body.
    fn element(&mut self, e: &Element, name: Option<&str>, body: &[Node], at: Option<At<'_>>) {
        let surface = matches!(&e.kind, ElementKind::Builtin(k) if k.is_surface());
        let inner = if surface {
            self.surface_at(&e.kind, name, at)
        } else {
            at.map(|a| (a.layer.to_string(), a.popup))
        };
        let inner_at = inner.as_ref().map(|(ns, popup)| At {
            layer: ns,
            popup: *popup,
        });
        self.props(&e.props, inner_at);
        if let Some(arg) = &e.arg {
            self.props(std::slice::from_ref(arg), inner_at);
        }
        self.nodes(&e.children, inner_at);
        self.nodes(body, inner_at);
        if let ElementKind::Component(def) = &e.kind {
            if self.stack.contains(def) {
                return;
            }
            let Some(body) = self
                .program
                .components
                .get(def)
                .map(|c| c.body.nodes.clone())
            else {
                return;
            };
            self.stack.push(*def);
            self.nodes(&body, inner_at);
            self.stack.pop();
        }
    }
}

/// Lua string contents: `\` and `"` escaped (namespaces are identifiers,
/// so this is only a guard).
fn lua_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `s` with regex metacharacters escaped.
fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The rules as Hyprland's Lua config (`hl.layer_rule`, 0.55 and later).
pub fn lua(rules: &[Rule]) -> String {
    let mut out = String::new();
    for r in rules {
        let _ = writeln!(out, "hl.layer_rule({{");
        let _ = writeln!(out, "    name = \"{}-blur\",", lua_escape(&r.namespace));
        let _ = writeln!(
            out,
            "    match = {{ namespace = \"{}\" }},",
            lua_escape(&format!("^{}$", regex_escape(&r.namespace)))
        );
        if r.blur {
            let _ = writeln!(out, "    blur = true,");
        }
        if r.blur_popups {
            let _ = writeln!(out, "    blur_popups = true,");
        }
        let _ = writeln!(out, "    ignore_alpha = {IGNORE_ALPHA},");
        let _ = writeln!(out, "}})");
    }
    out
}

/// The rules as the classic hyprlang `layerrule` lines (Hyprland before
/// the Lua config).
pub fn classic(rules: &[Rule]) -> String {
    let mut out = String::new();
    for r in rules {
        let target = format!("^({})$", regex_escape(&r.namespace));
        if r.blur {
            let _ = writeln!(out, "layerrule = blur, {target}");
        }
        if r.blur_popups {
            let _ = writeln!(out, "layerrule = blurpopups, {target}");
        }
        let _ = writeln!(out, "layerrule = ignorealpha {IGNORE_ALPHA}, {target}");
    }
    out
}

/// Compiles the config at `dir` (every `.strand` file under it).
pub fn load(dir: &Path) -> Result<Build, String> {
    let found = find_files(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    if let Some((path, e)) = found.errors.first() {
        return Err(format!("cannot read {}: {e}", path.display()));
    }
    if found.files.is_empty() {
        return Err(format!("no .strand files in {}", dir.display()));
    }
    let mut map = SourceMap::new();
    for path in &found.files {
        let src =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let src =
            String::from_utf8(src).map_err(|_| format!("{}: not valid UTF-8", path.display()))?;
        map.add(path.display().to_string(), src);
    }
    let shown = map.clone();
    Build::compile_with(None, map, crate::services::schema()).map_err(|diags| {
        format!(
            "{}the config has errors (`strand check` lists them)",
            render(&diags, &shown, Style::Plain)
        )
    })
}

pub(crate) const USAGE: &str = "usage: strand compositor-rules [dir] [--classic]\n\n\
    Prints Hyprland layer rules for the config at dir (default \
    $XDG_CONFIG_HOME/strand), for you to paste into your Hyprland config: \
    one keeping Hyprland's layer animations off every Strand surface, which \
    Strand animates itself, and one blurring behind each surface that asks \
    for `blur`, matched by its strand-<Name> namespace. The default is Hyprland's Lua config (hl.layer_rule, 0.55 and \
    later); --classic prints `layerrule =` lines for a hyprlang config, which \
    Hyprland reads no more from 0.57. \
    Nothing is applied to the running session.\n";

/// What `strand compositor-rules` was asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Ask {
    Help,
    Print { dir: Option<PathBuf>, classic: bool },
}

/// Parses the arguments after `compositor-rules`.
pub fn parse(args: &[String]) -> Result<Ask, String> {
    let mut dir = None;
    let mut classic = false;
    for a in args {
        match a.as_str() {
            "-h" | "--help" => return Ok(Ask::Help),
            "--classic" => classic = true,
            s if s.starts_with('-') || dir.is_some() => return Err(USAGE.into()),
            s => dir = Some(PathBuf::from(s)),
        }
    }
    Ok(Ask::Print { dir, classic })
}

/// Runs `strand compositor-rules`: the rules for stdout, or an error for
/// stderr.
pub fn run(args: &[String]) -> Result<String, String> {
    let (dir, classic) = match parse(args)? {
        Ask::Help => return Ok(USAGE.into()),
        Ask::Print { dir, classic } => (dir, classic),
    };
    let dir = match dir {
        Some(d) => d,
        None => crate::check::default_dir(
            std::env::var_os("XDG_CONFIG_HOME"),
            std::env::var_os("HOME"),
        )
        .ok_or("set XDG_CONFIG_HOME or HOME, or pass a directory")?,
    };
    let build = load(&dir)?;
    let rules = blur_rules(&build);
    let comment = if classic { "#" } else { "--" };
    let mut out = format!(
        "{comment} Strand: layer rules for the surfaces of {}\n\
         {comment} (strand compositor-rules{}). Paste into your Hyprland config.\n\
         {comment} Strand animates its own surfaces (enter, exit, morph); Hyprland's\n\
         {comment} layer animations would play over them, a panel fading in through\n\
         {comment} what it opens from.\n",
        dir.display(),
        if classic { " --classic" } else { "" }
    );
    out.push_str(&if classic {
        classic_no_anim()
    } else {
        lua_no_anim()
    });
    let _ = write!(
        out,
        "{comment} Blur behind the surfaces that ask for `blur`, only for a Hyprland\n\
         {comment} without ext-background-effect-v1 (Strand's log says when it is\n\
         {comment} missing): where it is offered, Strand already blurs behind each\n\
         {comment} `blur` box, and these rules would blur all of the surface above\n\
         {comment} alpha {IGNORE_ALPHA} besides.\n"
    );
    if rules.is_empty() {
        let _ = writeln!(out, "{comment} No surface asks for `blur`: no blur rules.");
        return Ok(out);
    }
    out.push_str(&if classic {
        self::classic(&rules)
    } else {
        lua(&rules)
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(src: &str) -> Build {
        let mut map = SourceMap::new();
        map.add("rules.strand", src.to_string());
        match Build::compile_with(None, map.clone(), crate::services::schema()) {
            Ok(b) => b,
            Err(d) => panic!("{}", render(&d, &map, Style::Plain)),
        }
    }

    const CONFIG: &str = r#"
bar Top { edge: top; height: 30; bg: #202020b8; blur: 24
  text "clock"
  popup { open: true; width: 200; height: 100
    box { bg: #202020e0; blur: 12 }
  }
}
panel Plain { anchor: top_right; width: 300; height: 200
  text "no blur here"
}
panel Menu { anchor: top_left; width: 300; height: 200
  popup { open: true; width: 100; height: 100; Frosted }
}
osd Level { width: 200; height: 40
  if true { Frosted }
}
component Frosted { box { blur: 8 } }
lock Gate { on key(k) { auth.submit(k.name) }; box { blur: 4 } }
"#;

    /// Every layer surface whose tree has `blur` gets a rule under its
    /// namespace: its own nodes give `blur`, a nested popup's give
    /// `blur_popups`; components, `if`s and nested elements are followed;
    /// a surface without blur and a lock get none.
    #[test]
    fn surfaces_with_blur_get_rules_by_namespace() {
        let rules = blur_rules(&build(CONFIG));
        let rule = |ns: &str, blur, blur_popups| Rule {
            namespace: ns.into(),
            blur,
            blur_popups,
        };
        assert_eq!(
            rules,
            [
                rule("strand-Level", true, false),
                rule("strand-Menu", false, true),
                rule("strand-Top", true, true),
            ]
        );
    }

    /// Hyprland's Lua form and the classic hyprlang lines, matched
    /// by the exact namespace, with `ignore_alpha` keeping the shadows
    /// and transparent margins sharp.
    #[test]
    fn rules_print_as_lua_and_as_classic_lines() {
        let rules = [
            Rule {
                namespace: "strand-Top".into(),
                blur: true,
                blur_popups: true,
            },
            Rule {
                namespace: "strand-Menu".into(),
                blur: false,
                blur_popups: true,
            },
        ];
        assert_eq!(
            lua(&rules),
            "hl.layer_rule({\n    name = \"strand-Top-blur\",\n    match = { namespace = \"^strand-Top$\" },\n    blur = true,\n    blur_popups = true,\n    ignore_alpha = 0.5,\n})\n\
             hl.layer_rule({\n    name = \"strand-Menu-blur\",\n    match = { namespace = \"^strand-Menu$\" },\n    blur_popups = true,\n    ignore_alpha = 0.5,\n})\n"
        );
        assert_eq!(
            classic(&rules),
            "layerrule = blur, ^(strand-Top)$\n\
             layerrule = blurpopups, ^(strand-Top)$\n\
             layerrule = ignorealpha 0.5, ^(strand-Top)$\n\
             layerrule = blurpopups, ^(strand-Menu)$\n\
             layerrule = ignorealpha 0.5, ^(strand-Menu)$\n"
        );
        assert_eq!(regex_escape("a.b(c)$"), "a\\.b\\(c\\)\\$");
        assert_eq!(lua_escape("a\"b\\"), "a\\\"b\\\\");
    }

    #[test]
    fn arguments() {
        let args = |a: &[&str]| parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            args(&[]),
            Ok(Ask::Print {
                dir: None,
                classic: false
            })
        );
        assert_eq!(
            args(&["conf", "--classic"]),
            Ok(Ask::Print {
                dir: Some("conf".into()),
                classic: true
            })
        );
        assert_eq!(args(&["--help"]), Ok(Ask::Help));
        assert!(args(&["a", "b"]).is_err());
        assert!(args(&["--apply"]).is_err());
    }

    /// The command reads a config directory and prints the rules with a
    /// comment saying what they are; a config with errors prints them.
    #[test]
    fn the_command_prints_the_rules_of_a_directory() {
        let dir = std::env::temp_dir().join(format!("strand-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("shell.strand"), CONFIG).unwrap();
        let arg = dir.display().to_string();
        let out = run(std::slice::from_ref(&arg)).unwrap();
        assert!(out.starts_with("-- Strand: layer rules"), "{out}");
        // Every config gets the rule keeping Hyprland's animations off,
        // its pattern the whole namespace (Hyprland full-matches).
        assert!(
            out.contains("namespace = \"^strand-.*$\" },\n    no_anim = true,"),
            "{out}"
        );
        // Hyprland with the protocol needs no rules (decisions.md,
        // m4-interaction-finish): the header says so.
        assert!(out.contains("only for a Hyprland\n-- without ext-background-effect-v1"));
        assert_eq!(out.matches("hl.layer_rule(").count(), 4, "{out}");
        let out = run(&[arg.clone(), "--classic".into()]).unwrap();
        assert!(out.starts_with("# Strand"), "{out}");
        assert!(out.contains("layerrule = blur, ^(strand-Level)$"), "{out}");
        assert!(out.contains("layerrule = noanim, ^(strand-.*)$"), "{out}");
        std::fs::write(dir.join("shell.strand"), "panel P { text \"x\" }\n").unwrap();
        let out = run(std::slice::from_ref(&arg)).unwrap();
        assert!(
            out.contains("No surface asks for `blur`: no blur rules"),
            "{out}"
        );
        assert_eq!(out.matches("hl.layer_rule(").count(), 1, "{out}");
        std::fs::write(dir.join("shell.strand"), "panel P { nonsense_prop: 3 }\n").unwrap();
        let err = run(&[arg]).unwrap_err();
        assert!(err.contains("strand check"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
