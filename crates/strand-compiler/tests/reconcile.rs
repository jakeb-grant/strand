//! Reload identity and Merkle hashes (`strand_compiler::reconcile`).

use strand_compiler::SourceMap;
use strand_compiler::reconcile::{Build, Sid};

fn build(prev: Option<&Build>, files: &[(&str, &str)]) -> Build {
    let mut map = SourceMap::new();
    for (n, t) in files {
        map.add(*n, *t);
    }
    match Build::compile(prev, map) {
        Ok(b) => b,
        Err(d) => panic!("{d:#?}"),
    }
}

/// The identity of each node whose source starts with `prefix` (the
/// first token of the node), in order.
fn sids(b: &Build, label: &str) -> Vec<Sid> {
    b.identity
        .entries()
        .filter(|(l, ..)| *l == label)
        .map(|(.., s)| s)
        .collect()
}

fn texts_of(b: &Build, label: &str) -> Vec<(String, Sid)> {
    let src = b
        .sources
        .get(strand_compiler::FileId(0))
        .unwrap()
        .text
        .clone();
    b.identity
        .entries()
        .filter(|(l, ..)| *l == label)
        .map(|(_, _, sp, s)| (src[sp.start as usize..sp.end as usize].to_string(), s))
        .collect()
}

#[test]
fn editing_props_keeps_every_identity() {
    let a = build(
        None,
        &[(
            "bar.strand",
            "bar Top {\n  row {\n    text \"a\" { opacity: 0.5 }\n    text \"b\"\n  }\n}\n",
        )],
    );
    let b = build(
        Some(&a),
        &[(
            "bar.strand",
            "// a comment\nbar Top {\n  height: 40\n  row { gap: 4\n    text \"a\" { opacity: 0.75 }\n    text \"b\"\n  }\n}\n",
        )],
    );
    for l in ["surface", "row", "text"] {
        assert_eq!(sids(&a, l), sids(&b, l), "{l}");
    }
    assert!(b.identity.warnings().is_empty());
}

#[test]
fn a_node_inserted_before_a_sibling_of_its_kind_is_new() {
    let a = build(
        None,
        &[("bar.strand", "bar Top {\n  text \"a\"\n  text \"b\"\n}\n")],
    );
    let b = build(
        Some(&a),
        &[(
            "bar.strand",
            "bar Top {\n  text \"new\"\n  text \"a\"\n  text \"b\"\n}\n",
        )],
    );
    let old: Vec<_> = texts_of(&a, "text");
    let new: Vec<_> = texts_of(&b, "text");
    assert_eq!(new[1], old[0]);
    assert_eq!(new[2], old[1]);
    assert!(!old.iter().any(|(_, s)| *s == new[0].1));
}

#[test]
fn wrapping_a_node_keeps_it() {
    let a = build(
        None,
        &[(
            "bar.strand",
            "bar Top {\n  Clock\n}\ncomponent Clock { state open = false\n  text \"x\" }\n",
        )],
    );
    let b = build(
        Some(&a),
        &[(
            "bar.strand",
            "bar Top {\n  row {\n    Clock\n  }\n}\ncomponent Clock { state open = false\n  text \"x\" }\n",
        )],
    );
    assert_eq!(sids(&a, "Clock"), sids(&b, "Clock"));
    assert_eq!(sids(&a, "text"), sids(&b, "text"));
}

#[test]
fn a_changed_kind_is_a_new_node() {
    let a = build(None, &[("bar.strand", "bar Top {\n  text \"a\"\n}\n")]);
    let b = build(Some(&a), &[("bar.strand", "bar Top {\n  icon \"a\"\n}\n")]);
    assert!(sids(&b, "text").is_empty());
    assert!(!sids(&a, "text").contains(&sids(&b, "icon")[0]));
    assert_eq!(sids(&a, "surface"), sids(&b, "surface"));
}

#[test]
fn moved_nodes_match_by_id_then_position() {
    let a = build(
        None,
        &[(
            "bar.strand",
            "bar Top {\n  box { id: one; width: 1 }\n  box { id: two; width: 2 }\n  text \"x\"\n}\n",
        )],
    );
    // `two` moves first: the diff keeps one of the two boxes, the other
    // matches by its `id:` name.
    let b = build(
        Some(&a),
        &[(
            "bar.strand",
            "bar Top {\n  box { id: two; width: 2 }\n  box { id: one; width: 1 }\n  text \"x\"\n}\n",
        )],
    );
    let old = texts_of(&a, "box");
    let new = texts_of(&b, "box");
    let find = |v: &[(String, Sid)], s: &str| v.iter().find(|(t, _)| t.contains(s)).unwrap().1;
    assert_eq!(find(&old, "one"), find(&new, "one"));
    assert_eq!(find(&old, "two"), find(&new, "two"));
}

#[test]
fn handler_hashes_follow_what_they_reach() {
    let src = |body: &str, f: &str| {
        format!(
            "fn step(x: int) -> int {{ {f} }}\nstate n = 0\nbar Top {{\n  text join(\"\", n) {{\n    on click {{ {body} }}\n    on secondary {{ n = 0 }}\n  }}\n}}\n"
        )
    };
    let a = build(None, &[("bar.strand", &src("n = step(n)", "x + 1"))]);
    let hashes = |b: &Build| -> Vec<u64> {
        b.identity
            .entries()
            .filter(|(l, ..)| l.starts_with("on "))
            .map(|(_, f, sp, _)| b.hashes.get(f, sp).unwrap())
            .collect()
    };
    // Reformatted and commented: no change.
    let b = build(
        Some(&a),
        &[(
            "bar.strand",
            &src("\n      n   =   step( n )   // again\n    ", "x + 1")
                .replace("state n", "// n\nstate n"),
        )],
    );
    assert_eq!(hashes(&a), hashes(&b));
    // The fn the click reaches changed: only the click handler differs.
    let c = build(Some(&b), &[("bar.strand", &src("n = step(n)", "x + 2"))]);
    let (hb, hc) = (hashes(&b), hashes(&c));
    assert_ne!(hb[0], hc[0]);
    assert_eq!(hb[1], hc[1]);
}

/// Mutually recursive `fn`s: a handler that names either one changes
/// when the other is edited, whichever the hashing reached first.
#[test]
fn handler_hashes_cover_a_whole_cycle() {
    let src = |k: &str| {
        format!(
            "fn a(x: int) -> int {{ x > 10 ? x : b(x + {k}) }}\nfn b(x: int) -> int {{ a(x * 2) }}\nstate n = 0\nbar Top {{\n  text join(\"\", n) {{\n    on click {{ n = a(n) }}\n    on secondary {{ n = b(n) }}\n  }}\n}}\n"
        )
    };
    let hashes = |b: &Build| -> Vec<u64> {
        b.identity
            .entries()
            .filter(|(l, ..)| l.starts_with("on "))
            .map(|(_, f, sp, _)| b.hashes.get(f, sp).unwrap())
            .collect()
    };
    let a = build(None, &[("bar.strand", &src("1"))]);
    let b = build(Some(&a), &[("bar.strand", &src("2"))]);
    let (ha, hb) = (hashes(&a), hashes(&b));
    assert_eq!(ha.len(), 2);
    assert_ne!(ha[0], hb[0], "the handler naming `a`");
    assert_ne!(ha[1], hb[1], "the handler naming `b`, which calls `a`");
}
