//! `strand reload [--hard]`: the compiler worker rescans; the answer
//! comes with that load's event (`Shell::after_step`), to a socket client
//! or (M5, S-ipc) through the request channel's reply (a D-Bus
//! `Reload`), which waits for its load the same way.

use serde_json::{Value as Json, json};

use super::{Caller, Shell};
use crate::ipc::ClientId;
use crate::live::Job;

/// Marks the client ids of the request channel's reloads: a load names
/// the clients it answers by id (`Job::Reload`), and the socket numbers
/// its clients up from 1, so a bus reload's id has this bit set and never
/// meets one.
pub(in crate::run) const BUS_RELOAD: ClientId = 1 << 63;

impl Shell {
    #[cold]
    #[inline(never)]
    pub(super) fn reload_request(&mut self, from: Caller, hard: bool) {
        let client = match from {
            Caller::Socket(id) => id,
            Caller::Bus(reply) => {
                // The lowest id no waiting bus reload holds.
                let mut id = BUS_RELOAD;
                while self.bus_reloads.iter().any(|(b, _)| *b == id) {
                    id += 1;
                }
                self.bus_reloads.push((id, reply));
                id
            }
        };
        let sent = self.jobs.as_ref().is_some_and(|j| {
            j.send(Job::Reload {
                hard,
                client: Some(client),
            })
            .is_ok()
        });
        if !sent {
            self.reloaded(
                client,
                json!({"ok": false, "error": "this shell does not reload"}),
            );
        }
    }

    /// Answer reload `client`: a socket client, or a bus reload's id.
    pub(in crate::run) fn reloaded(&mut self, client: ClientId, answer: Json) {
        if client & BUS_RELOAD == 0 {
            if let Some(s) = &mut self.server {
                s.answer(client, &answer);
            }
        } else if let Some(i) = self.bus_reloads.iter().position(|(b, _)| *b == client) {
            self.bus_reloads.swap_remove(i).1.send(answer);
        }
    }
}
