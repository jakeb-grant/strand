//! The M0 benchmark graph: 10,000 reactive nodes, depth 20, mixed fan-in
//! and fan-out. Shared by `benches/graph.rs` and `tests/bench_graph.rs`
//! (which checks its shape and values). Lives in a subdirectory so cargo
//! does not treat it as a bench target of its own.

use strand_core::{Memo, NodeId, Runtime, Signal};

/// Total signals + memos.
pub const NODES: usize = 10_000;
/// Layers including the signal layer.
pub const DEPTH: usize = 20;
/// Signals in layer 0 (one of them is the global `root`).
pub const SIGNALS: usize = 100;

#[derive(Copy, Clone, Debug)]
pub enum Node {
    S(Signal<i64>),
    M(Memo<i64>),
}

impl Node {
    pub fn get(self, rt: &Runtime) -> i64 {
        match self {
            Node::S(s) => s.get(rt).unwrap_or(0),
            Node::M(m) => m.get(rt).unwrap_or(0),
        }
    }
    pub fn id(self) -> NodeId {
        match self {
            Node::S(s) => s.id(),
            Node::M(m) => m.id(),
        }
    }
}

#[derive(Debug)]
pub struct Graph {
    pub rt: Runtime,
    /// Read by every layer-1 memo, so everything depends on it.
    pub root: Signal<i64>,
    /// The other signals.
    pub signals: Vec<Signal<i64>>,
    /// All nodes by layer.
    pub layers: Vec<Vec<Node>>,
    /// For each memo (in creation order), the indices into `flat` it reads.
    pub inputs: Vec<Vec<usize>>,
    /// All nodes in creation order.
    pub flat: Vec<Node>,
    /// Watches on the last layer (the "props" the scene emitter reads).
    pub watches: usize,
}

/// xorshift64*: deterministic, no dependency.
#[derive(Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Build the graph. Each memo reads 1–4 inputs: mostly the previous layer,
/// some from any earlier layer; a few hub nodes per layer take a large
/// share of reads (fan-out). Memo values are wrapping sums, so a write
/// propagates without cut-off.
pub fn build(seed: u64) -> Graph {
    let rt = Runtime::new();
    let mut rng = Rng::new(seed);
    let root = rt.signal(1i64);
    let mut signals = Vec::new();
    let mut flat = vec![Node::S(root)];
    let mut layer0 = vec![Node::S(root)];
    for i in 1..SIGNALS {
        let s = rt.signal(i as i64);
        signals.push(s);
        flat.push(Node::S(s));
        layer0.push(Node::S(s));
    }
    let mut layers = vec![layer0];
    let mut layer_start = vec![0usize];
    let mut inputs = Vec::new();
    let memos = NODES - SIGNALS;
    let per_layer = memos / (DEPTH - 1);
    let mut made = 0;
    for layer in 1..DEPTH {
        let count = if layer == DEPTH - 1 {
            memos - made
        } else {
            per_layer
        };
        let prev_start = layer_start[layer - 1];
        let prev_len = layers[layer - 1].len();
        let this_start = flat.len();
        layer_start.push(this_start);
        let mut nodes = Vec::with_capacity(count);
        for _ in 0..count {
            let fan_in = 1 + rng.below(4);
            let mut ins: Vec<usize> = Vec::with_capacity(fan_in + 1);
            if layer == 1 {
                ins.push(0); // root
            }
            for _ in 0..fan_in {
                let r = rng.below(100);
                let pick = if r < 15 {
                    // Hub: one of the first 4 nodes of the previous layer.
                    prev_start + rng.below(prev_len.min(4))
                } else if r < 85 {
                    prev_start + rng.below(prev_len)
                } else {
                    rng.below(this_start)
                };
                if !ins.contains(&pick) {
                    ins.push(pick);
                }
            }
            let srcs: Vec<Node> = ins.iter().map(|&i| flat[i]).collect();
            let m = rt.memo(move |rt| {
                let mut sum = 0i64;
                for s in &srcs {
                    sum = sum.wrapping_add(match *s {
                        Node::S(s) => s.get(rt)?,
                        Node::M(m) => m.get(rt)?,
                    });
                }
                Ok(sum)
            });
            inputs.push(ins);
            nodes.push(Node::M(m));
            flat.push(Node::M(m));
        }
        made += count;
        layers.push(nodes);
    }
    let mut watches = 0;
    for n in &layers[DEPTH - 1] {
        if rt.watch(n.id()).is_ok() {
            watches += 1;
        }
    }
    rt.flush();
    Graph {
        rt,
        root,
        signals,
        layers,
        inputs,
        flat,
        watches,
    }
}

impl Graph {
    /// Naive recomputation of every node from current signal values.
    pub fn naive(&self) -> Vec<i64> {
        let mut v: Vec<i64> = Vec::with_capacity(self.flat.len());
        for n in &self.flat[..SIGNALS] {
            v.push(n.get(&self.rt));
        }
        for ins in &self.inputs {
            let mut sum = 0i64;
            for &i in ins {
                sum = sum.wrapping_add(v[i]);
            }
            v.push(sum);
        }
        v
    }
}
