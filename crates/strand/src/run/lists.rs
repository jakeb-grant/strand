//! The logic side of virtualised lists: render asks for the rows a
//! `list` shows (its view plus overscan, `ToLogic::ListWindow`), and the
//! instance mounts them by key (`Instance::set_list_window`).

use strand_compiler::instantiate::{Instance, MAX_LIST_WINDOW};
use strand_scene::NodeId;

/// Mounts rows `first .. first + count` of virtualised list `list` (at
/// most [`MAX_LIST_WINDOW`] of them). A list that is gone (unmounted
/// since render asked) is ignored.
pub(super) fn set_window(inst: &Instance, list: NodeId, first: u32, count: u32) {
    let count = count.min(MAX_LIST_WINDOW as u32);
    if !inst.set_list_window(list, first, count) {
        log::debug!("list window for {list:?}: not a mounted virtualised list");
    }
}
