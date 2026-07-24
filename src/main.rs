//! MinkaCap — per-window Wayland capture for MinkaDE.
//!
//! Captures a toplevel's OWN buffer (occlusion-free, no raising, no focus
//! theft) by driving three staging protocols directly:
//!   ext_foreign_toplevel_list_v1                       — window identity
//!   ext_foreign_toplevel_image_capture_source_manager  — window -> source
//!   ext_image_copy_capture_manager_v1                  — source -> frame
//!
//! Quickshell's ScreencopyView wires ext-image-copy-capture to OUTPUTS only;
//! its toplevel path needs hyprland-toplevel-export, which ShojiWM does not
//! implement. This helper is the route to occlusion-free window capture on
//! ShojiWM — MinkaShot shells out to it for `shot win`, retiring the
//! raise-then-freeze workaround.
//!
//! Usage:
//!   MinkaCap list                          — one "app_id\ttitle" per line
//!   MinkaCap grab <selector> <out.png>     — capture matching window to PNG
//!
//! `selector` matches a toplevel whose app_id or title equals it (exact),
//! else whose title or app_id contains it (substring). MinkaShot passes the
//! exact title of the window it resolved from the compositor's view.
//!
//! Exit codes: 2 no toplevel-list protocol, 3 no match, 4 session stopped
//! before constraints, 5 no usable shm format, 6 frame failed.

use std::os::fd::AsFd;

use wayland_client::protocol::{
    wl_buffer,
    wl_registry, 
    wl_shm,
    wl_shm_pool,
};
use wayland_client::{
    Connection, 
    Dispatch,
    QueueHandle, 
    WEnum,
};

use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{
        self, 
        ExtForeignToplevelHandleV1,
    },
    ext_foreign_toplevel_list_v1::{
        self, 
        ExtForeignToplevelListV1,
    },
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{
        self, 
        ExtImageCopyCaptureFrameV1,
    },
    ext_image_copy_capture_manager_v1::{
        self, 
        ExtImageCopyCaptureManagerV1,
    },
    ext_image_copy_capture_session_v1::{
        self, 
        ExtImageCopyCaptureSessionV1,
    },
};

#[derive(Default)]
struct Toplevel {
    title: String,
    app_id: String,
}

#[derive(Default)]
struct State {
    shm: Option<wl_shm::WlShm>,
    toplevel_list: Option<ExtForeignToplevelListV1>,
    source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    capture_manager: Option<ExtImageCopyCaptureManagerV1>,

    toplevels: Vec<(ExtForeignToplevelHandleV1, Toplevel)>,

    // Session/frame constraints and completion flags.
    buffer_size: Option<(u32, u32)>,
    shm_formats: Vec<u32>,
    session_done: bool,
    frame_ready: bool,
    frame_failed: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface
                .as_str() { "wl_shm" => {
                    state.shm = Some(
                        registry.bind::<wl_shm::WlShm,
                            _,
                            _>(name, 1, qh, ()),
                    );
                }
                "ext_foreign_toplevel_list_v1" => {
                    state.toplevel_list =
                        Some(registry.bind::<ExtForeignToplevelListV1, _, _>(
                            name,
                            version
                                .min(1),
                            qh,
                            (),
                        ));
                }
                "ext_foreign_toplevel_image_capture_source_manager_v1" => {
                    state.source_manager = Some(
                        registry
                            .bind::<ExtForeignToplevelImageCaptureSourceManagerV1, _, _>(
                                name,
                                version
                                    .min(1),
                                qh,
                                (),
                            ),
                    );
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.capture_manager =
                        Some(registry.bind::<ExtImageCopyCaptureManagerV1, _, _>(
                            name,
                            version
                                .min(1),
                            qh,
                            (),
                        ));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            state.toplevels
                .push(
                    (
                        toplevel, 
                        Toplevel::default(),
                    ),
                );
        }
    }

    wayland_client::event_created_child!(
        State, 
        ExtForeignToplevelListV1,
        [
            ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
        ]
    );
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(
            (
                _, 
                entry,
            ),
        ) = state.toplevels.iter_mut().find(|(h, _)| h == handle,) else {
            return;
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Title { 
                title,
            } => entry.title = title,
            ext_foreign_toplevel_handle_v1::Event::AppId { 
                app_id,
            } => entry.app_id = app_id,
            _ => {},
        }
    }
}

impl Dispatch<
    ExtImageCopyCaptureSessionV1,
    (),
> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { 
                width, 
                height,
            } => {
                state.buffer_size = Some(
                    (
                        width,
                        height,
                    ),
                );
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { 
                format,
            } => {
                if let WEnum::Value(f) = format {
                    state.shm_formats
                        .push(
                            f as u32,
                        );
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => state.session_done = true,
            ext_image_copy_capture_session_v1::Event::Stopped => state.frame_failed = true,
            _ => {}
        }
    }
}

impl Dispatch<
    ExtImageCopyCaptureFrameV1,
    (),
> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => state.frame_ready = true,
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => state.frame_failed = true,
            _ => {}
        }
    }
}

// Interfaces whose events we don't act on.
macro_rules! no_events {
    ($($iface:ty),* $(,)?) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(
                _: &mut Self, _: &$iface, _: <$iface as wayland_client::Proxy>::Event,
                _: &(), _: &Connection, _: &QueueHandle<Self>,
            ) {}
        }
    )*};
}

no_events!(
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    ExtForeignToplevelImageCaptureSourceManagerV1,
    ExtImageCaptureSourceV1,
    ExtImageCopyCaptureManagerV1,
);

fn die(
    code: i32, 
    msg: &str,
) -> ! {
    eprintln!(
        "MinkaCap: {msg}",
    );
    std::process::exit(
        code,
    );
}

fn main() {
    let args: Vec<String> = std::env::args()
        .collect();
    let mode = args
        .get(1)
        .map(
            String::as_str,
        ).unwrap_or("");

    let conn = Connection::connect_to_env()
        .expect(
            "wayland connect",
        );
    let mut queue = conn
        .new_event_queue();
    let qh = queue
        .handle();
    conn
        .display()
        .get_registry(
            &qh, 
            (),
        );

    let mut state = State::default();
    // Two roundtrips: bind globals, then receive toplevel handles + metadata.
    queue
        .roundtrip(
            &mut state,
        ).expect("roundtrip");
    queue
        .roundtrip(
            &mut state,
        ).expect("roundtrip");

    if state.toplevel_list.is_none() {
        die(
            2, 
            "compositor lacks ext_foreign_toplevel_list_v1",
        );
    }

    match mode {
        "list" => {
            for (
                _,
                t,
            ) in &state.toplevels {
                println!(
                    "{}\t{}",
                    t.app_id, 
                    t.title,
                );
            }
        }
        "grab" => {
            let selector = args
                .get(2)
                .unwrap_or_else(
                    || die(
                        1, 
                        "usage: MinkaCap grab <selector> <out.png>",
                    ),
                );
            let out_path = args
                .get(3)
                .unwrap_or_else(
                    || die(
                        1,
                        "usage: MinkaCap grab <selector> <out.png>",
                    ),
                );

            // Exact match wins over substring so a caller passing a precise
            // title can't be shadowed by a longer window that contains it.
            let found = state
                .toplevels
                .iter()
                .find(|(_, t)| t.app_id == *selector || t.title == *selector)
                .or_else(|| {
                    state
                        .toplevels
                        .iter()
                        .find(|(_, t)| t.title.contains(selector.as_str())
                            || t.app_id.contains(selector.as_str()))
                });
            let Some(
                (
                    handle,
                    info,
                ),
            ) =
                found.map(|(h, t)| (h.clone(), format!("{} / {}", t.app_id, t.title)))
            else {
                die(
                    3, 
                    &format!(
                        "no toplevel matching '{selector}'",
                    ),
                );
            };
            eprintln!(
                "MinkaCap: capturing {info}",
            );

            let source_manager = state
                .source_manager
                .as_ref()
                .unwrap_or_else(
                    || die(
                        2, 
                        "no toplevel image-capture-source manager",
                    ),
                );
            let capture_manager = state
                .capture_manager
                .as_ref()
                .unwrap_or_else(
                    || die(
                        2,
                        "no ext_image_copy_capture_manager_v1",
                    ),
                );

            let source: ExtImageCaptureSourceV1 =
                source_manager
                    .create_source(
                        &handle,
                        &qh, 
                        (),
                    );
            let session: ExtImageCopyCaptureSessionV1 = capture_manager.create_session(
                &source,
                ext_image_copy_capture_manager_v1::Options::empty(),
                &qh,
                (),
            );

            while !state.session_done && !state.frame_failed {
                queue
                    .blocking_dispatch(
                        &mut state,
                    ).expect(
                    "dispatch",
                );
            }
            if state.frame_failed {
                die(
                    4, 
                    "capture session stopped before constraints arrived",
                );
            }

            let (
                width,
                height,
            ) = state.buffer_size
                .expect(
                    "session sent no buffer size",
                );
            // wl_shm format codes: 0 = argb8888, 1 = xrgb8888.
            let fmt = if state.shm_formats
                .contains(
                    &1,
                ) {
                wl_shm::Format::Xrgb8888
            } else if state.shm_formats
                .contains(
                    &0,
                ) {
                wl_shm::Format::Argb8888
            } else {
                die(
                    5, 
                    &format!(
                        "no usable shm format in {:?}", 
                        state.shm_formats,
                    ),
                );
            };

            let stride = width * 4;
            let size = (
                stride * height
            ) as usize;

            let memfd = rustix::fs::memfd_create(
                "minkacap", 
                rustix::fs::MemfdFlags::CLOEXEC,
            )
                .expect(
                    "memfd_create",
                );
            rustix::fs::ftruncate(
                &memfd,
                size as u64,
            ).expect(
                "ftruncate",
            );

            let shm = state.shm
                .as_ref()
                .expect(
                    "no wl_shm",
                );
            let pool = shm
                .create_pool(
                    memfd
                        .as_fd(),
                    size as i32,
                    &qh, 
                    (),
                );
            let buffer =
                pool
                    .create_buffer(
                        0,
                        width as i32,
                        height as i32,
                        stride as i32, 
                        fmt,
                        &qh,
                        (),
                    );

            let frame: ExtImageCopyCaptureFrameV1 = session
                .create_frame(
                    &qh,
                    (),
                );
            frame
                .attach_buffer(
                    &buffer,
                );
            frame
                .capture();

            while !state.frame_ready && !state.frame_failed {
                queue
                    .blocking_dispatch(
                        &mut state,
                    ).expect("dispatch");
            }
            if state.frame_failed {
                die(
                    6, 
                    "frame capture failed",
                );
            }

            // The compositor wrote into our shm; the Ready event means it's
            // finished, so a SHARED read-mapping of the same fd is safe.
            let map = unsafe {
                rustix::mm::mmap(
                    std::ptr::null_mut(),
                    size,
                    rustix::mm::ProtFlags::READ,
                    rustix::mm::MapFlags::SHARED,
                    &memfd,
                    0,
                )
            }
            .expect(
                "mmap",
            );
            let pixels = unsafe {
                std::slice::from_raw_parts(
                    map as *const u8,
                    size,
                )
            };

            // xrgb/argb8888 are little-endian 32-bit words in memory: B,G,R,X.
            // Force opaque alpha — a screenshot of a translucent window should
            // show its content, not composite the desktop through it.
            let mut rgba = Vec::with_capacity(
                size,
            );
            for px in pixels
                .chunks_exact(
                    4,
                ) {
                rgba
                    .extend_from_slice(
                        &[px[2], px[1], px[0], 0xff],
                    );
            }

            let file = std::fs::File::create(
                out_path,
            )
                .unwrap_or_else(
                    |e| die(
                        1, 
                        &format!(
                            "cannot create {out_path}: {e}",
                        ),
                    ),
                );
            let mut enc = png::Encoder::new(
                std::io::BufWriter::new(
                    file,
                ), 
                width,
                height,
            );
            enc
                .set_color(
                    png::ColorType::Rgba,
                );
            enc
                .set_depth(
                    png::BitDepth::Eight,
                );
            enc
                .write_header()
                .expect(
                    "png header",
                )
                .write_image_data(
                    &rgba,
                )
                .expect(
                    "png data",
                );

            eprintln!(
                "MinkaCap: saved {out_path} ({width}x{height})",
            );
        }
        _ => die(
            1, 
            "usage: MinkaCap list | grab <selector> <out.png>",
        ),
    }
}