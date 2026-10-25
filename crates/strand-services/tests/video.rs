//! (services-round) `video.streams` against a private PipeWire
//! (WirePlumber links the streams): a camera (a `Video/Source` with
//! `media.role` `Camera`) and a screen cast (a `Video/Source` of no
//! device, as the portal makes one), each taken by a program's stream;
//! the service lists each taker once linked, with its program's name and
//! where its video comes from, and drops it when it stops.

#![cfg(feature = "pipewire")]

mod pipewire;

use std::time::{Duration, Instant};

use ::pipewire as pw;
use pipewire::PipeWire;
use pw::spa;
use pw::spa::pod::Pod;
use strand_core::Runtime;
use strand_services::audio::{self, VideoSource};
use strand_services::video::Video;
use strand_services::{Builtin, Buses, Cells, Services};

/// Pump until `cond` holds (15 s at most).
fn until(rt: &Runtime, s: &Services, what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        s.pump(rt);
        rt.flush();
        if cond() {
            return;
        }
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn state(b: &Builtin, rt: &Runtime) -> Video {
    b.video.cells().snapshot(rt).unwrap()
}

/// What the peers' thread is asked.
enum Ask {
    /// Stop the stream of this program.
    Stop(&'static str),
    Quit,
}

/// A raw video format: fixed for a source, ranges for a taker.
fn format(fixed: bool) -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::utils::{Fraction, Rectangle};
    let obj = if fixed {
        spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::RGBx),
            spa::pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                Rectangle {
                    width: 64,
                    height: 48
                }
            ),
            spa::pod::property!(
                FormatProperties::VideoFramerate,
                Fraction,
                Fraction { num: 30, denom: 1 }
            ),
        )
    } else {
        spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(
                FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                VideoFormat::RGBx,
                VideoFormat::RGBx,
                VideoFormat::BGRx
            ),
            spa::pod::property!(
                FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                Rectangle {
                    width: 64,
                    height: 48
                },
                Rectangle {
                    width: 1,
                    height: 1
                },
                Rectangle {
                    width: 4096,
                    height: 4096
                }
            ),
            spa::pod::property!(
                FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                Fraction { num: 30, denom: 1 },
                Fraction { num: 0, denom: 1 },
                Fraction {
                    num: 1000,
                    denom: 1
                }
            ),
        )
    };
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .unwrap()
    .0
    .into_inner()
}

/// The camera, the screen cast and their takers, on a PipeWire loop of
/// their own (a thread): `meeting` takes the camera, `recorder` the
/// screen.
fn peers(pw_socket: String) -> (pw::channel::Sender<Ask>, std::thread::JoinHandle<()>) {
    let (tx, rx) = pw::channel::channel::<Ask>();
    let thread = std::thread::spawn(move || {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
        let context = pw::context::ContextRc::new(&mainloop, None).unwrap();
        let core = context
            .connect_rc(Some(pw::properties::properties! {
                "remote.name" => pw_socket.as_str(),
            }))
            .unwrap();
        let source = |name: &str, camera: bool| {
            let mut props = pw::properties::properties! {
                "media.class" => "Video/Source",
                "node.name" => name,
                "application.name" => "strand-test-source",
            };
            if camera {
                props.insert("media.role", "Camera");
            }
            let stream = pw::stream::StreamBox::new(&core, name, props).unwrap();
            let bytes = format(true);
            let mut params = [Pod::from_bytes(&bytes).unwrap()];
            stream
                .connect(
                    spa::utils::Direction::Output,
                    None,
                    pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::DRIVER,
                    &mut params,
                )
                .unwrap();
            stream
        };
        // The takers move into the channel's callback (`'static`), so
        // they hold the core themselves.
        let taker = |app: &str, target: &str| {
            let stream = pw::stream::StreamRc::new(
                core.clone(),
                app,
                pw::properties::properties! {
                    "media.type" => "Video",
                    "media.category" => "Capture",
                    "application.name" => app,
                    "target.object" => target,
                },
            )
            .unwrap();
            let bytes = format(false);
            let mut params = [Pod::from_bytes(&bytes).unwrap()];
            stream
                .connect(
                    spa::utils::Direction::Input,
                    None,
                    pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                    &mut params,
                )
                .unwrap();
            stream
        };
        let camera = source("strand-camera", true);
        let screen = source("strand-screen", false);
        let mut takers = vec![
            (
                "strand-meeting",
                Some(taker("strand-meeting", "strand-camera")),
            ),
            (
                "strand-recorder",
                Some(taker("strand-recorder", "strand-screen")),
            ),
        ];
        let takers_cell = std::rc::Rc::new(std::cell::RefCell::new(std::mem::take(&mut takers)));
        let ml = mainloop.clone();
        let cell = takers_cell.clone();
        let _rx = rx.attach(mainloop.loop_(), move |ask| match ask {
            Ask::Stop(app) => {
                for (name, s) in cell.borrow_mut().iter_mut() {
                    if *name == app
                        && let Some(s) = s.take()
                    {
                        let _ = s.disconnect();
                    }
                }
            }
            Ask::Quit => ml.quit(),
        });
        mainloop.run();
        drop(takers_cell);
        drop((camera, screen));
    });
    (tx, thread)
}

#[test]
fn video_streams_name_the_camera_and_the_screen() {
    let Some(pw) = PipeWire::start("video_streams_name_the_camera_and_the_screen") else {
        return;
    };
    pw.wait_for_defaults();
    audio::configure(Some(pw.config()));
    let rt = Runtime::new();
    let s = Services::new(&rt, Buses::none(), || {});
    let b = Builtin::register(&s, &rt);
    b.video.acquire(&rt);
    assert!(s.wait_ready(&rt, Duration::from_secs(10)), "the first read");
    assert!(state(&b, &rt).streams.is_empty(), "nothing takes video yet");

    let (ask, thread) = peers(pw.socket().display().to_string());
    let shown = |b: &Builtin| {
        let mut v: Vec<(String, VideoSource)> = state(b, &rt)
            .streams
            .into_iter()
            .map(|s| (s.app, s.source))
            .collect();
        v.sort();
        v
    };
    until(&rt, &s, "the camera's and the screen's takers", || {
        shown(&b).len() == 2
    });
    assert_eq!(
        shown(&b),
        [
            ("strand-meeting".to_string(), VideoSource::Camera),
            ("strand-recorder".to_string(), VideoSource::Screencast),
        ],
        "{}",
        pw.run("pw-link", &["-l"])
    );

    // A taker that stops leaves the list.
    let _ = ask.send(Ask::Stop("strand-meeting"));
    until(&rt, &s, "the meeting gone", || {
        shown(&b) == [("strand-recorder".to_string(), VideoSource::Screencast)]
    });

    let _ = ask.send(Ask::Quit);
    let _ = thread.join();
    until(&rt, &s, "every taker gone", || shown(&b).is_empty());
    b.video.release(&rt);
    s.shutdown();
    audio::configure(None);
}
