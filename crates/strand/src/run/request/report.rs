//! `strand report` (S-tools): compositor, protocols, scales, memory, GPU
//! and frame stats (`crate::report`). Logic asks main for its half
//! through the snapshot hop (`ToMain::Snapshot` with
//! `SnapshotKind::Report`), keeps the caller under the request's
//! `SnapshotId`, and answers once main's `ToLogic::Snapshot` comes back,
//! with its own half added. Main answers between frames and never waits
//! on logic. A socket request only, as `export` (architecture.md: the
//! D-Bus front end has no `Report`).

use serde_json::json;
use strand_core::ipc as proto;

use super::{Caller, Shell};
use crate::run::{Snapshot, SnapshotId, SnapshotKind, ToMain};

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn report_request(&mut self, from: Caller) {
        let reply = SnapshotId(self.next_snapshot);
        self.next_snapshot += 1;
        let asked = self.to_main.as_ref().is_some_and(|tx| {
            tx.send(ToMain::Snapshot {
                kind: SnapshotKind::Report,
                reply,
            })
            .is_ok()
        });
        if !asked {
            return self.answer(from, proto::refused("no display thread to report on"));
        }
        self.reports.push((reply, from));
    }

    /// Main's answer to a snapshot: the report it is for, answered.
    pub(in crate::run) fn snapshot_answered(&mut self, snap: Snapshot) {
        let Some(i) = self.reports.iter().position(|(id, _)| *id == snap.reply) else {
            return;
        };
        let (_, from) = self.reports.swap_remove(i);
        let report = crate::report::logic_part(snap.data);
        self.answer(from, json!({"ok": true, "report": report}));
    }
}
