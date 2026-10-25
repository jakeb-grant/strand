//! (services-round) The programs taking video over PipeWire (the
//! `video` service): a camera or a screen-cast indicator.
//!
//! The same PipeWire loop as `audio` ([`crate::audio`], its
//! `strand-pipewire` code: one connection, reconnecting with backoff when
//! the daemon restarts) runs on the service's own `strand-video` thread,
//! following only what video needs ([`crate::audio`]'s `Follow::Video`):
//! the video sources (`Video/Source`, `Video/Source/Virtual`,
//! `Stream/Output/Video`), the streams that take video
//! (`Stream/Input/Video`) and the links between them. Each stream with a
//! link from a source is an item of `video.streams`; its `source` is
//! `camera` when any of those sources is a camera (`media.role`
//! `Camera`, a node of a device, a V4L2 or libcamera node: what the
//! session manager makes of a camera), else `screencast` (the portal's
//! stream of a screen or a window). A stream not linked yet takes
//! nothing and is not listed.
//!
//! A program that opens a camera without PipeWire (`/dev/video*`
//! directly) is not seen. The service binds no audio node, card or
//! metadata, and costs nothing while nobody reads it (it starts on the
//! first read and stops 5 s after the last reader leaves, as every
//! service). It connects where `audio` does ([`crate::audio::configure`]).

use crate::audio::model::{AudioChange, VideoStream};
use crate::audio::{Cmd, Follow, Host, thread};
use crate::{Cx, Msg, ServiceError, Store, service};

/// The schema the `video` service serves.
pub const SCHEMA: &str = strand_services_schema::VIDEO;

/// `video`: see the module docs.
#[service(name = "video", schema = SCHEMA, thread)]
#[derive(Store, Clone, Debug, Default, PartialEq)]
pub struct Video {
    /// The programs taking video now, keyed by `id`: a camera indicator
    /// (`video.streams.any(s => s.source == camera)`) and a screen-cast
    /// one (`… == screencast`), with the programs' names. A program that
    /// opens a camera without PipeWire is not seen.
    #[store(keyed)]
    pub streams: Vec<VideoStream>,
}

/// The `video` store's side of the loop.
struct VideoHost {
    cx: Cx<Video>,
    stopped: bool,
}

impl Host for VideoHost {
    fn changes(&mut self, batch: Vec<AudioChange>) {
        let mut patches = Vec::new();
        for change in batch {
            match change {
                AudioChange::Connected(c) => log::info!("video: PipeWire connected: {c}"),
                AudioChange::Video(d) => patches.push(VideoPatch::Streams(d)),
                // The audio loop's; a video loop follows none of it.
                _ => {}
            }
        }
        if !patches.is_empty() && !self.cx.send(patches) {
            self.stopped = true;
        }
        // The first batch holds every field.
        if !self.stopped && !self.cx.ready() {
            self.stopped = true;
        }
    }

    fn poll(&mut self) -> Vec<Cmd> {
        loop {
            match self.cx.try_recv() {
                // Nothing is writable: a write is answered as it is.
                Ok(Msg::Write(w)) => {
                    if !self.cx.report(&w, |_| {}) {
                        self.stopped = true;
                    }
                }
                Ok(Msg::Action(a)) => match a {},
                Ok(Msg::Call(c, _)) => match c {},
                Ok(Msg::Visible(_) | Msg::Watch { .. } | Msg::Keep(_)) => {}
                Err(false) => break,
                Err(true) => {
                    self.stopped = true;
                    break;
                }
            }
        }
        if self.stopped {
            vec![Cmd::Stop]
        } else {
            Vec::new()
        }
    }
}

impl Video {
    fn run(cx: Cx<Self>) -> Result<(), ServiceError> {
        let (tx, rx) = pipewire::channel::channel::<Cmd>();
        let poke = tx.clone();
        cx.set_notify(move || {
            let _ = poke.send(Cmd::Poke);
        });
        let host = VideoHost { cx, stopped: false };
        // Messages sent before the notify hook was set are read now.
        let _ = tx.send(Cmd::Poke);
        thread::run(
            crate::audio::service_config(),
            Follow::Video,
            Box::new(host),
            rx,
        )
        .map_err(ServiceError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Keyed, Service};

    #[test]
    fn the_store_is_the_schema_record() {
        assert_eq!(<Video as Service>::NAME, "video");
        let names: Vec<&str> = Video::FIELDS.iter().map(|f| f.name).collect();
        assert_eq!(names, ["streams"]);
        assert_eq!((Video::FIELDS[0].ty)(), "[VideoStream]");
        assert_eq!(VideoStream::KEY_FIELD, "id");
    }
}
