//! Identifiers shared across threads.

/// A Wayland surface the render thread paints (one per output for a `bar`).
/// Allocated by the surface manager.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SurfaceId(pub u32);

/// A generational node id in the retained scene tree. The logic thread
/// allocates ids (see [`NodeIdAllocator`]); a slot's generation is bumped
/// whenever it is reused, so an op naming a removed node is detectably stale
/// instead of silently hitting its successor.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId {
    pub index: u32,
    pub generation: u32,
}

impl NodeId {
    pub const fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }
}

/// Allocates generational [`NodeId`]s, reusing freed slots with a bumped
/// generation.
#[derive(Clone, Debug, Default)]
pub struct NodeIdAllocator {
    generations: Vec<u32>,
    live: Vec<bool>,
    free: Vec<u32>,
}

impl NodeIdAllocator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn alloc(&mut self) -> NodeId {
        if let Some(index) = self.free.pop() {
            let i = index as usize;
            self.live[i] = true;
            NodeId::new(index, self.generations[i])
        } else {
            let index = self.generations.len() as u32;
            self.generations.push(0);
            self.live.push(true);
            NodeId::new(index, 0)
        }
    }

    /// Frees `id`. Returns false (and does nothing) if `id` is stale.
    pub fn free(&mut self, id: NodeId) -> bool {
        if !self.is_live(id) {
            return false;
        }
        let i = id.index as usize;
        self.live[i] = false;
        self.generations[i] = self.generations[i].wrapping_add(1);
        self.free.push(id.index);
        true
    }

    pub fn is_live(&self, id: NodeId) -> bool {
        let i = id.index as usize;
        self.live.get(i).copied().unwrap_or(false) && self.generations[i] == id.generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_bumps_generation() {
        let mut a = NodeIdAllocator::new();
        let x = a.alloc();
        let y = a.alloc();
        assert_ne!(x, y);
        assert!(a.free(x));
        assert!(!a.free(x));
        let z = a.alloc();
        assert_eq!(z.index, x.index);
        assert_ne!(z.generation, x.generation);
        assert!(!a.is_live(x));
        assert!(a.is_live(z));
    }
}
