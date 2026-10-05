//! The M0 benchmark graph has the advertised shape and stays correct
//! under writes (the bench measures it; this proves it).

#[path = "../benches/support/graph_builder.rs"]
#[allow(dead_code)]
mod graph_builder;

use graph_builder::{DEPTH, NODES, SIGNALS, build};

#[test]
fn benchmark_graph_shape_and_values() {
    let g = build(7);
    assert_eq!(g.flat.len(), NODES);
    assert_eq!(g.layers.len(), DEPTH);
    assert_eq!(g.layers[0].len(), SIGNALS);
    assert!(g.watches > 400);
    // Mixed fan-in: 1 to 5 inputs (layer 1 also reads the root).
    let min = g.inputs.iter().map(Vec::len).min().unwrap();
    let max = g.inputs.iter().map(Vec::len).max().unwrap();
    assert_eq!((min, max), (1, 5));
    // Fan-out: some node is read by many.
    let mut readers = vec![0usize; NODES];
    for ins in &g.inputs {
        for &i in ins {
            readers[i] += 1;
        }
    }
    assert!(*readers.iter().max().unwrap() > 50);
    // Values match naive recomputation after writes.
    let rt = &g.rt;
    g.signals[3].set(rt, 1000).unwrap();
    g.root.set(rt, -5).unwrap();
    let tick = rt.flush();
    assert!(tick.errors.is_empty());
    assert_eq!(tick.changed.len(), g.watches, "root reaches every leaf");
    let naive = g.naive();
    for (i, n) in g.flat.iter().enumerate() {
        assert_eq!(n.get(rt), naive[i], "node {i}");
    }
    // Idle afterwards.
    assert!(rt.is_idle());
    assert_eq!(rt.next_deadline(), None);
}
