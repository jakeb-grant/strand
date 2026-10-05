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

/// Densely mutually recursive `fn`s hash in linear time (one strongly
/// connected component, hashed once): 30 of them, each calling all the
/// others, compile in well under a second; an edit to any member still
/// changes the handler that names only the first.
#[test]
fn a_dense_cycle_of_fns_hashes_quickly() {
    let n = 30;
    let src = |k: usize| {
        let mut s = String::new();
        for i in 0..n {
            let calls: Vec<String> = (0..n).map(|j| format!("f{j}(x - 1)")).collect();
            let step = if i == n - 1 { k } else { 0 };
            s.push_str(&format!(
                "fn f{i}(x: int) -> int {{ x < {step} ? 0 : {} }}\n",
                calls.join(" + ")
            ));
        }
        s.push_str(
            "state v = 0\nbar Top {\n  text join(\"\", v) {\n    on click { v = f0(3) }\n  }\n}\n",
        );
        s
    };
    let compile = |prev: Option<&Build>, k: usize| {
        let t = std::time::Instant::now();
        let b = build(prev, &[("bar.strand", &src(k))]);
        (b, t.elapsed())
    };
    let (a, ta) = compile(None, 0);
    let (b, tb) = compile(Some(&a), 1);
    let check = std::time::Instant::now();
    let _ = strand_compiler::compile(&{
        let mut m = SourceMap::new();
        m.add("bar.strand", src(1));
        m
    });
    let tc = check.elapsed();
    let hash = |b: &Build| -> u64 {
        b.identity
            .entries()
            .find(|(l, ..)| l.starts_with("on "))
            .map(|(_, f, sp, _)| b.hashes.get(f, sp).unwrap())
            .unwrap()
    };
    assert_ne!(hash(&a), hash(&b), "an edit to the last member");
    // The whole build (check, lower, identity, hashes) against the check
    // alone: the hashes add little (they were factorial in the cycle).
    let budget = tc * 3 + std::time::Duration::from_millis(100);
    assert!(ta < budget && tb < budget, "{ta:?} {tb:?} (check {tc:?})");
}

/// A long chain of `fn`s (each calling the next, the last reading a
/// state) lowers in time linear in its length: the read sets are one
/// union per strongly connected component, not a walk per chunk (which
/// took 3.4 s at 2000), and the handler at the top still reads the state
/// at the bottom.
#[test]
fn a_long_chain_of_fns_lowers_quickly() {
    let n = 2000;
    let mut s = String::from("state deep = 1\nstate v = 0\n");
    for i in 0..n {
        if i == n - 1 {
            s.push_str(&format!("fn f{i}(x: int) -> int {{ x + deep }}\n"));
        } else {
            s.push_str(&format!("fn f{i}(x: int) -> int {{ f{}(x + 1) }}\n", i + 1));
        }
    }
    s.push_str("bar Top {\n  text join(\"\", v, f0(0))\n}\n");
    let check = std::time::Instant::now();
    let _ = strand_compiler::compile(&{
        let mut m = SourceMap::new();
        m.add("bar.strand", s.clone());
        m
    });
    let tc = check.elapsed();
    let t = std::time::Instant::now();
    let b = build(None, &[("bar.strand", &s)]);
    let tb = t.elapsed();
    let prog = &b.program;
    let deep = (0..prog.defs.len())
        .find(|&i| prog.defs[i].name == "deep")
        .expect("the state");
    let text = prog
        .reads
        .iter()
        .filter(|r| r.defs.iter().any(|d| d.0 as usize == deep))
        .count();
    // f{n-1}, every fn above it, and the text binding.
    assert!(text > n, "{text} chunks read `deep`");
    let budget = tc * 3 + std::time::Duration::from_millis(100);
    assert!(tb < budget, "{tb:?} (check {tc:?})");
}
