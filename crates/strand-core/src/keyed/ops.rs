//! Incremental `filter`, `map`, `take` and `sort_by`.
//!
//! Each operator is a small state machine that consumes the source's
//! [`VecDiff`]s and emits output diffs that keep the source keys. Applying
//! the emitted diffs to the previous output always gives the same list as
//! recomputing the operator from scratch on the new source (property-tested
//! in `tests/keyed_props.rs`). A malformed source diff returns an error and
//! the caller rebuilds from a full snapshot.
//!
//! Costs are O(n) per diff in the worst case (index bookkeeping on plain
//! vectors), with small constants: a few microseconds at 2,000 rows. A
//! derived collection that receives a bulk change (more than 128 diffs and
//! at least a quarter of its source) rebuilds and diffs its output by key
//! instead (see `reactive`), so a reshuffle never costs O(n) per diff.

use std::cmp::Ordering;

use super::{KeyedError, VecDiff};

/// An incremental list operator.
pub trait IncrementalOp<K, T> {
    /// Output item type.
    type Out;
    /// Consume one source diff, pushing output diffs to `out`.
    fn apply(
        &mut self,
        diff: &VecDiff<K, T>,
        out: &mut Vec<VecDiff<K, Self::Out>>,
    ) -> Result<(), KeyedError>;
}

fn oob(index: usize, len: usize) -> KeyedError {
    KeyedError::IndexOutOfRange { index, len }
}

/// `.filter(pred)`.
#[derive(Debug)]
pub struct Filter<P> {
    pred: P,
    mask: Vec<bool>,
}

impl<P> Filter<P> {
    /// A filter over an empty source; feed it a `Reset` first if the source
    /// is not empty.
    pub fn new(pred: P) -> Self {
        Self {
            pred,
            mask: Vec::new(),
        }
    }

    fn out_index(&self, i: usize) -> usize {
        self.mask[..i].iter().filter(|&&b| b).count()
    }
}

impl<K, T, P> IncrementalOp<K, T> for Filter<P>
where
    K: Clone,
    T: Clone,
    P: FnMut(&K, &T) -> bool,
{
    type Out = T;

    fn apply(
        &mut self,
        diff: &VecDiff<K, T>,
        out: &mut Vec<VecDiff<K, T>>,
    ) -> Result<(), KeyedError> {
        let len = self.mask.len();
        match diff {
            VecDiff::Reset { items } => {
                self.mask = items.iter().map(|(k, v)| (self.pred)(k, v)).collect();
                let kept = items
                    .iter()
                    .zip(&self.mask)
                    .filter(|(_, m)| **m)
                    .map(|(item, _)| item.clone())
                    .collect();
                out.push(VecDiff::Reset { items: kept });
            }
            VecDiff::Insert { index, key, value } => {
                if *index > len {
                    return Err(oob(*index, len));
                }
                let keep = (self.pred)(key, value);
                self.mask.insert(*index, keep);
                if keep {
                    out.push(VecDiff::Insert {
                        index: self.out_index(*index),
                        key: key.clone(),
                        value: value.clone(),
                    });
                }
            }
            VecDiff::Update { index, key, value } => {
                if *index >= len {
                    return Err(oob(*index, len));
                }
                let was = self.mask[*index];
                let keep = (self.pred)(key, value);
                self.mask[*index] = keep;
                let at = self.out_index(*index);
                let key = key.clone();
                match (was, keep) {
                    (true, true) => out.push(VecDiff::Update {
                        index: at,
                        key,
                        value: value.clone(),
                    }),
                    (true, false) => out.push(VecDiff::Remove { index: at, key }),
                    (false, true) => out.push(VecDiff::Insert {
                        index: at,
                        key,
                        value: value.clone(),
                    }),
                    (false, false) => {}
                }
            }
            VecDiff::Remove { index, key } => {
                if *index >= len {
                    return Err(oob(*index, len));
                }
                if self.mask[*index] {
                    out.push(VecDiff::Remove {
                        index: self.out_index(*index),
                        key: key.clone(),
                    });
                }
                self.mask.remove(*index);
            }
            VecDiff::Move { from, to, key } => {
                if *from >= len || *to >= len {
                    return Err(oob((*from).max(*to), len));
                }
                let keep = self.mask[*from];
                let old = self.out_index(*from);
                self.mask.remove(*from);
                self.mask.insert(*to, keep);
                let new = self.out_index(*to);
                if keep && old != new {
                    out.push(VecDiff::Move {
                        from: old,
                        to: new,
                        key: key.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// `.map(f)`; keys pass through.
#[derive(Debug)]
pub struct Map<F> {
    f: F,
    len: usize,
}

impl<F> Map<F> {
    /// A map over an empty source.
    pub fn new(f: F) -> Self {
        Self { f, len: 0 }
    }
}

impl<K, T, U, F> IncrementalOp<K, T> for Map<F>
where
    K: Clone,
    F: FnMut(&K, &T) -> U,
{
    type Out = U;

    fn apply(
        &mut self,
        diff: &VecDiff<K, T>,
        out: &mut Vec<VecDiff<K, U>>,
    ) -> Result<(), KeyedError> {
        let len = self.len;
        let d = match diff {
            VecDiff::Reset { items } => {
                self.len = items.len();
                VecDiff::Reset {
                    items: items
                        .iter()
                        .map(|(k, v)| (k.clone(), (self.f)(k, v)))
                        .collect(),
                }
            }
            VecDiff::Insert { index, key, value } => {
                if *index > len {
                    return Err(oob(*index, len));
                }
                self.len += 1;
                VecDiff::Insert {
                    index: *index,
                    key: key.clone(),
                    value: (self.f)(key, value),
                }
            }
            VecDiff::Update { index, key, value } => {
                if *index >= len {
                    return Err(oob(*index, len));
                }
                VecDiff::Update {
                    index: *index,
                    key: key.clone(),
                    value: (self.f)(key, value),
                }
            }
            VecDiff::Remove { index, key } => {
                if *index >= len {
                    return Err(oob(*index, len));
                }
                self.len -= 1;
                VecDiff::Remove {
                    index: *index,
                    key: key.clone(),
                }
            }
            VecDiff::Move { from, to, key } => {
                if *from >= len || *to >= len {
                    return Err(oob((*from).max(*to), len));
                }
                VecDiff::Move {
                    from: *from,
                    to: *to,
                    key: key.clone(),
                }
            }
        };
        out.push(d);
        Ok(())
    }
}

/// `.take(n)`: the first `n` items.
#[derive(Debug)]
pub struct Take<K, T> {
    n: usize,
    src: Vec<(K, T)>,
}

impl<K, T> Take<K, T> {
    /// The first `n` items of an empty source.
    pub fn new(n: usize) -> Self {
        Self { n, src: Vec::new() }
    }
}

impl<K: Clone + Eq, T: Clone> IncrementalOp<K, T> for Take<K, T> {
    type Out = T;

    fn apply(
        &mut self,
        diff: &VecDiff<K, T>,
        out: &mut Vec<VecDiff<K, T>>,
    ) -> Result<(), KeyedError> {
        let n = self.n;
        match diff {
            VecDiff::Reset { items } => {
                self.src = items.clone();
                out.push(VecDiff::Reset {
                    items: items.iter().take(n).cloned().collect(),
                });
            }
            VecDiff::Insert { index, key, value } => {
                diff.apply(&mut self.src)?;
                if *index < n {
                    out.push(VecDiff::Insert {
                        index: *index,
                        key: key.clone(),
                        value: value.clone(),
                    });
                    if self.src.len() > n {
                        out.push(VecDiff::Remove {
                            index: n,
                            key: self.src[n].0.clone(),
                        });
                    }
                }
            }
            VecDiff::Update { index, key, value } => {
                diff.apply(&mut self.src)?;
                if *index < n {
                    out.push(VecDiff::Update {
                        index: *index,
                        key: key.clone(),
                        value: value.clone(),
                    });
                }
            }
            VecDiff::Remove { index, key } => {
                diff.apply(&mut self.src)?;
                if *index < n {
                    out.push(VecDiff::Remove {
                        index: *index,
                        key: key.clone(),
                    });
                    if self.src.len() >= n {
                        let (k, v) = self.src[n - 1].clone();
                        out.push(VecDiff::Insert {
                            index: n - 1,
                            key: k,
                            value: v,
                        });
                    }
                }
            }
            VecDiff::Move { from, to, key } => {
                diff.apply(&mut self.src)?;
                match (*from < n, *to < n) {
                    (true, true) => {
                        if from != to {
                            out.push(VecDiff::Move {
                                from: *from,
                                to: *to,
                                key: key.clone(),
                            });
                        }
                    }
                    (true, false) => {
                        out.push(VecDiff::Remove {
                            index: *from,
                            key: key.clone(),
                        });
                        let (k, v) = self.src[n - 1].clone();
                        out.push(VecDiff::Insert {
                            index: n - 1,
                            key: k,
                            value: v,
                        });
                    }
                    (false, true) => {
                        let value = self.src[*to].1.clone();
                        out.push(VecDiff::Insert {
                            index: *to,
                            key: key.clone(),
                            value,
                        });
                        out.push(VecDiff::Remove {
                            index: n,
                            key: self.src[n].0.clone(),
                        });
                    }
                    (false, false) => {}
                }
            }
        }
        Ok(())
    }
}

/// A stable bottom-up merge sort that never panics, whatever `cmp` returns.
/// `slice::sort_by` may panic when the comparator is not a total order
/// (a float comparator meeting NaN from service data); this one only ever
/// asks "is `b` strictly less than `a`?", which is always answerable.
fn merge_sort(v: &mut Vec<usize>, mut cmp: impl FnMut(usize, usize) -> Ordering) {
    let n = v.len();
    if n < 2 {
        return;
    }
    let mut buf = v.clone();
    let mut width = 1;
    while width < n {
        let mut start = 0;
        while start < n {
            let mid = (start + width).min(n);
            let end = (start + 2 * width).min(n);
            let (mut a, mut b) = (start, mid);
            for slot in &mut buf[start..end] {
                let take_b = a >= mid || b < end && cmp(v[b], v[a]) == Ordering::Less;
                if take_b {
                    *slot = v[b];
                    b += 1;
                } else {
                    *slot = v[a];
                    a += 1;
                }
            }
            start = end;
        }
        std::mem::swap(v, &mut buf);
        width *= 2;
    }
}

/// `.sort_by(cmp)`: a stable sort (ties keep source order). A comparator
/// that is not a total order (NaN) never panics; the order is then
/// unspecified but still a permutation that keeps every key.
#[derive(Debug)]
pub struct SortBy<K, T, C> {
    cmp: C,
    src: Vec<(K, T)>,
    /// Output position -> source index.
    order: Vec<usize>,
}

impl<K, T, C> SortBy<K, T, C>
where
    C: FnMut(&T, &T) -> Ordering,
{
    /// A sort over an empty source.
    pub fn new(cmp: C) -> Self {
        Self {
            cmp,
            src: Vec::new(),
            order: Vec::new(),
        }
    }

    /// Where source index `i` belongs among `order` (which must not
    /// contain `i`).
    fn position(&mut self, i: usize) -> usize {
        let Self { cmp, src, order } = self;
        order.partition_point(|&j| cmp(&src[j].1, &src[i].1).then(j.cmp(&i)) == Ordering::Less)
    }

    fn pos_of(&self, i: usize) -> Result<usize, KeyedError> {
        self.order
            .iter()
            .position(|&j| j == i)
            .ok_or(oob(i, self.src.len()))
    }

    /// Remove source index `i` from `order`, shifting later indices down.
    fn unlink(&mut self, i: usize) -> Result<usize, KeyedError> {
        let pos = self.pos_of(i)?;
        self.order.remove(pos);
        for j in &mut self.order {
            if *j > i {
                *j -= 1;
            }
        }
        Ok(pos)
    }

    /// Make room for a source item inserted at `i`.
    fn shift_up(&mut self, i: usize) {
        for j in &mut self.order {
            if *j >= i {
                *j += 1;
            }
        }
    }
}

impl<K, T, C> IncrementalOp<K, T> for SortBy<K, T, C>
where
    K: Clone + Eq,
    T: Clone,
    C: FnMut(&T, &T) -> Ordering,
{
    type Out = T;

    fn apply(
        &mut self,
        diff: &VecDiff<K, T>,
        out: &mut Vec<VecDiff<K, T>>,
    ) -> Result<(), KeyedError> {
        match diff {
            VecDiff::Reset { items } => {
                self.src = items.clone();
                let mut order: Vec<usize> = (0..items.len()).collect();
                let Self { cmp, src, .. } = self;
                merge_sort(&mut order, |a, b| cmp(&src[a].1, &src[b].1));
                self.order = order;
                out.push(VecDiff::Reset {
                    items: self.order.iter().map(|&j| self.src[j].clone()).collect(),
                });
            }
            VecDiff::Insert { index, key, value } => {
                diff.apply(&mut self.src)?;
                self.shift_up(*index);
                let pos = self.position(*index);
                self.order.insert(pos, *index);
                out.push(VecDiff::Insert {
                    index: pos,
                    key: key.clone(),
                    value: value.clone(),
                });
            }
            VecDiff::Remove { index, key } => {
                diff.apply(&mut self.src)?;
                // `src` already lost the item; fix `order` the same way.
                let pos = self.pos_of(*index)?;
                self.order.remove(pos);
                for j in &mut self.order {
                    if *j > *index {
                        *j -= 1;
                    }
                }
                out.push(VecDiff::Remove {
                    index: pos,
                    key: key.clone(),
                });
            }
            VecDiff::Update { index, key, value } => {
                diff.apply(&mut self.src)?;
                let pos = self.pos_of(*index)?;
                self.order.remove(pos);
                let new = self.position(*index);
                self.order.insert(new, *index);
                out.push(VecDiff::Update {
                    index: pos,
                    key: key.clone(),
                    value: value.clone(),
                });
                if new != pos {
                    out.push(VecDiff::Move {
                        from: pos,
                        to: new,
                        key: key.clone(),
                    });
                }
            }
            VecDiff::Move { from, to, key } => {
                let len = self.src.len();
                if *from >= len || *to >= len {
                    return Err(oob((*from).max(*to), len));
                }
                if self.src[*from].0 != *key {
                    return Err(KeyedError::KeyMismatch { index: *from });
                }
                let pos = self.unlink(*from)?;
                let item = self.src.remove(*from);
                self.src.insert(*to, item);
                self.shift_up(*to);
                let new = self.position(*to);
                self.order.insert(new, *to);
                if new != pos {
                    out.push(VecDiff::Move {
                        from: pos,
                        to: new,
                        key: key.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}
