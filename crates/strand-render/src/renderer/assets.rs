//! (M6, the owner's approval of 2026-10-14) Local files a config draws
//! (an `image`'s, `svg`'s or `lottie`'s source) saved while the shell
//! runs: `strand run`'s watcher reports them (`Role::Asset`) and they are
//! read again and drawn, with no reload and no state change, as an edit
//! in the config's `icons/` folder is ([`Renderer::icons_changed`]).

use std::collections::HashSet;
use std::path::PathBuf;

use strand_scene::SurfaceId;

use super::Renderer;
use crate::media::Source;

impl Renderer {
    /// Local files changed on disk (absolute paths, as the sources name
    /// them once the compiler made a relative one absolute): each `image`
    /// decode of one is read again (the old one drawn until it is in), and
    /// each `svg` or `lottie` whose source is one reads its file again;
    /// the surfaces showing them repaint whole (a new decode may reuse a
    /// freed pixmap's address, which the frame records hash). Nothing else
    /// changes: no layout, no state.
    pub fn assets_changed(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let mut repaint: HashSet<SurfaceId> = self
            .extras
            .images
            .invalidate_files(paths)
            .into_iter()
            .collect();
        let nodes: Vec<_> = self
            .extras
            .media
            .sources
            .iter()
            .filter(|(_, src)| match src {
                Source::Svg(s) => s.file_changed(paths),
                Source::Lottie(l) => l.file_changed(paths),
                _ => false,
            })
            .map(|(id, _)| *id)
            .collect();
        for node in nodes {
            repaint.extend(self.surfaces_of(node));
        }
        for id in repaint {
            if let Some(s) = self.surfaces.get_mut(&id) {
                s.valid = false;
                s.mark_dirty();
            }
        }
    }
}
