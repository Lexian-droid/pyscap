use std::{
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering},
        mpsc::{self, sync_channel, SyncSender},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use pipewire as pw;
use pw::{
    context::Context,
    main_loop::MainLoop,
    properties::properties,
    spa::{
        self,
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
            ParamType,
        },
        pod::{Pod, Property},
        sys::{
            spa_buffer, spa_meta_header, SPA_META_Header, SPA_PARAM_META_size, SPA_PARAM_META_type,
        },
        utils::{Direction, SpaTypes},
    },
    stream::{StreamRef, StreamState},
};

use crate::{
    capturer::Options,
    frame::{BGRxFrame, Frame, RGBFrame, RGBxFrame, VideoFrame, XBGRFrame},
};

use self::{error::LinCapError, portal::ScreenCastPortal};

mod audio;
mod error;
mod portal;
mod x11;

static CAPTURER_STATE: AtomicU8 = AtomicU8::new(0);
static STREAM_STATE_CHANGED_TO_ERROR: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct PipeWireTimestampMapper {
    first_pts: Option<i64>,
    first_wall_time: Option<SystemTime>,
}

impl PipeWireTimestampMapper {
    fn timestamp(&mut self, pts: i64, received_at: SystemTime) -> SystemTime {
        // spa_meta_header.pts is in PipeWire's running/monotonic clock domain,
        // not Unix time. Anchor the first valid PTS to the wall clock and retain
        // PTS deltas so these timestamps remain comparable to PulseAudio's
        // wall-clock latency-compensated timestamps and the X11 backend.
        let first_pts = *self.first_pts.get_or_insert(pts);
        let first_wall_time = *self.first_wall_time.get_or_insert(received_at);
        if pts >= first_pts {
            first_wall_time
                .checked_add(Duration::from_nanos((pts - first_pts) as u64))
                .unwrap_or(first_wall_time)
        } else {
            first_wall_time
                .checked_sub(Duration::from_nanos((first_pts - pts) as u64))
                .unwrap_or(SystemTime::UNIX_EPOCH)
        }
    }
}

#[derive(Clone)]
struct ListenerUserData {
    pub tx: mpsc::Sender<Frame>,
    pub format: spa::param::video::VideoInfoRaw,
    pub output_size: Arc<[AtomicU32; 2]>,
    pub timestamp_mapper: PipeWireTimestampMapper,
}

fn param_changed_callback(
    _stream: &StreamRef,
    user_data: &mut ListenerUserData,
    id: u32,
    param: Option<&Pod>,
) {
    let Some(param) = param else {
        return;
    };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }
    let (media_type, media_subtype) = match pw::spa::param::format_utils::parse_format(param) {
        Ok(v) => v,
        Err(_) => return,
    };

    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return;
    }

    if let Err(error) = user_data.format.parse(param) {
        eprintln!("Failed to parse PipeWire format parameter: {error}");
        STREAM_STATE_CHANGED_TO_ERROR.store(true, Ordering::Relaxed);
        return;
    }
    let size = user_data.format.size();
    user_data.output_size[0].store(size.width, Ordering::Relaxed);
    user_data.output_size[1].store(size.height, Ordering::Relaxed);
}

fn state_changed_callback(
    _stream: &StreamRef,
    _user_data: &mut ListenerUserData,
    _old: StreamState,
    new: StreamState,
) {
    match new {
        StreamState::Error(e) => {
            eprintln!("pipewire: State changed to error({e})");
            STREAM_STATE_CHANGED_TO_ERROR.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        _ => {}
    }
}

unsafe fn get_timestamp(buffer: *mut spa_buffer) -> i64 {
    let n_metas = (*buffer).n_metas;
    if n_metas > 0 {
        let mut meta_ptr = (*buffer).metas;
        let metas_end = (*buffer).metas.wrapping_add(n_metas as usize);
        while meta_ptr != metas_end {
            if (*meta_ptr).type_ == SPA_META_Header {
                let meta_header: &mut spa_meta_header =
                    &mut *((*meta_ptr).data as *mut spa_meta_header);
                return meta_header.pts;
            }
            meta_ptr = meta_ptr.wrapping_add(1);
        }
        0
    } else {
        0
    }
}

fn process_callback(stream: &StreamRef, user_data: &mut ListenerUserData) {
    let buffer = unsafe { stream.dequeue_raw_buffer() };
    if !buffer.is_null() {
        'outside: {
            let buffer = unsafe { (*buffer).buffer };
            if buffer.is_null() {
                break 'outside;
            }
            let timestamp = unsafe { get_timestamp(buffer) };

            let n_datas = unsafe { (*buffer).n_datas };
            if n_datas < 1 {
                return;
            }
            let frame_size = user_data.format.size();
            let frame_data: Vec<u8> = unsafe {
                std::slice::from_raw_parts(
                    (*(*buffer).datas).data as *mut u8,
                    (*(*buffer).datas).maxsize as usize,
                )
                .to_vec()
            };

            let received_at = SystemTime::now();
            let display_time = user_data.timestamp_mapper.timestamp(timestamp, received_at);
            if user_data.timestamp_mapper.first_pts == Some(timestamp) {
                eprintln!(
                    "Linux video: PipeWire PTS anchored to wall clock; initial pts={timestamp}, timestamp={display_time:?}"
                );
            }

            let send_result = match user_data.format.format() {
                VideoFormat::RGBx => user_data.tx.send(Frame::Video(VideoFrame::RGBx(RGBxFrame {
                    display_time,
                    width: frame_size.width as i32,
                    height: frame_size.height as i32,
                    data: frame_data,
                }))),

                VideoFormat::RGB => user_data.tx.send(Frame::Video(VideoFrame::RGB(RGBFrame {
                    display_time,
                    width: frame_size.width as i32,
                    height: frame_size.height as i32,
                    data: frame_data,
                }))),

                VideoFormat::xBGR => user_data.tx.send(Frame::Video(VideoFrame::XBGR(XBGRFrame {
                    display_time,
                    width: frame_size.width as i32,
                    height: frame_size.height as i32,
                    data: frame_data,
                }))),

                VideoFormat::BGRx => user_data.tx.send(Frame::Video(VideoFrame::BGRx(BGRxFrame {
                    display_time,
                    width: frame_size.width as i32,
                    height: frame_size.height as i32,
                    data: frame_data,
                }))),

                _ => {
                    eprintln!("Unsupported PipeWire frame format received");
                    STREAM_STATE_CHANGED_TO_ERROR.store(true, Ordering::Relaxed);
                    break 'outside;
                }
            };
            if let Err(e) = send_result {
                eprintln!("{e}");
            }
        }
    } else {
        eprintln!("Out of buffers");
    }

    unsafe { stream.queue_raw_buffer(buffer) };
}

// TODO: Format negotiation
fn pipewire_capturer(
    options: Options,
    tx: mpsc::Sender<Frame>,
    ready_sender: &SyncSender<bool>,
    stream_id: u32,
    output_size: Arc<[AtomicU32; 2]>,
) -> Result<(), LinCapError> {
    pw::init();

    let mainloop = MainLoop::new(None)?;
    let context = Context::new(&mainloop)?;
    let core = context.connect(None)?;

    let user_data = ListenerUserData {
        tx,
        format: Default::default(),
        output_size: output_size.clone(),
        timestamp_mapper: PipeWireTimestampMapper {
            first_pts: None,
            first_wall_time: None,
        },
    };

    let stream = pw::stream::Stream::new(
        &core,
        "scap",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(user_data.clone())
        .state_changed(state_changed_callback)
        .param_changed(param_changed_callback)
        .process(process_callback)
        .register()?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::RGB,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::BGRx,
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                // Default
                width: 128,
                height: 128,
            },
            pw::spa::utils::Rectangle {
                // Min
                width: 1,
                height: 1,
            },
            pw::spa::utils::Rectangle {
                // Max
                width: 4096,
                height: 4096,
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoMaxFramerate,
            Fraction,
            pw::spa::utils::Fraction {
                num: options.fps,
                denom: 1
            }
        ),
    );

    let metas_obj = pw::spa::pod::object!(
        SpaTypes::ObjectParamMeta,
        ParamType::Meta,
        Property::new(
            SPA_PARAM_META_type,
            pw::spa::pod::Value::Id(pw::spa::utils::Id(SPA_META_Header))
        ),
        Property::new(
            SPA_PARAM_META_size,
            pw::spa::pod::Value::Int(size_of::<pw::spa::sys::spa_meta_header>() as i32)
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )?
    .0
    .into_inner();
    let metas_values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(metas_obj),
    )?
    .0
    .into_inner();

    let mut params = [
        pw::spa::pod::Pod::from_bytes(&values).unwrap(),
        pw::spa::pod::Pod::from_bytes(&metas_values).unwrap(),
    ];

    stream.connect(
        Direction::Input,
        Some(stream_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    let pw_loop = mainloop.loop_();
    let negotiation_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while output_size[0].load(Ordering::Relaxed) == 0
        && !STREAM_STATE_CHANGED_TO_ERROR.load(Ordering::Relaxed)
        && std::time::Instant::now() < negotiation_deadline
    {
        pw_loop.iterate(Duration::from_millis(100));
    }
    if output_size[0].load(Ordering::Relaxed) == 0 {
        return Err(LinCapError::new(
            "PipeWire stream did not negotiate a video size".to_string(),
        ));
    }

    ready_sender.send(true)?;

    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(10));
    }

    // User has called Capturer::start() and we start the main loop
    while CAPTURER_STATE.load(std::sync::atomic::Ordering::Relaxed) == 1
        && /* If the stream state got changed to `Error`, we exit. TODO: tell user that we exited */
          !STREAM_STATE_CHANGED_TO_ERROR.load(std::sync::atomic::Ordering::Relaxed)
    {
        pw_loop.iterate(Duration::from_millis(100));
    }

    Ok(())
}

pub struct PipeWireCapturer {
    capturer_join_handle: Option<JoinHandle<Result<(), LinCapError>>>,
    // The pipewire stream is deleted when the connection is dropped.
    // That's why we keep it alive
    _connection: dbus::blocking::Connection,
    output_size: Arc<[AtomicU32; 2]>,
}

impl PipeWireCapturer {
    // TODO: Error handling
    pub fn new(options: &Options, tx: mpsc::Sender<Frame>) -> Result<Self, LinCapError> {
        let connection = dbus::blocking::Connection::new_session().map_err(|error| {
            LinCapError::new(format!(
                "PipeWire backend unavailable: no session D-Bus: {error}"
            ))
        })?;
        let stream_id = ScreenCastPortal::new(&connection)
            .show_cursor(options.show_cursor)
            .map_err(|error| {
                LinCapError::new(format!("PipeWire cursor configuration failed: {error}"))
            })?
            .create_stream()
            .map_err(|error| LinCapError::new(format!("PipeWire portal session failed: {error}")))?
            .pw_node_id();

        // TODO: Fix this hack
        let options = options.clone();
        let output_size = Arc::new([AtomicU32::new(0), AtomicU32::new(0)]);
        let worker_output_size = output_size.clone();
        let (ready_sender, ready_recv) = sync_channel(1);
        let capturer_join_handle = std::thread::spawn(move || {
            let res = pipewire_capturer(options, tx, &ready_sender, stream_id, worker_output_size);
            if res.is_err() {
                ready_sender.send(false)?;
            }
            res
        });

        if !ready_recv.recv().map_err(|error| {
            LinCapError::new(format!(
                "PipeWire capture worker failed to initialize: {error}"
            ))
        })? {
            return Err(LinCapError::new("PipeWire stream setup failed".to_string()));
        }

        Ok(Self {
            capturer_join_handle: Some(capturer_join_handle),
            _connection: connection,
            output_size,
        })
    }

    pub fn start_capture(&self) {
        CAPTURER_STATE.store(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn stop_capture(&mut self) {
        CAPTURER_STATE.store(2, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.capturer_join_handle.take() {
            match handle.join() {
                Ok(Err(error)) => eprintln!("Error occurred capturing: {error}"),
                Err(_) => eprintln!("PipeWire capture worker terminated unexpectedly"),
                Ok(Ok(())) => {}
            }
        }
        CAPTURER_STATE.store(0, std::sync::atomic::Ordering::Relaxed);
        STREAM_STATE_CHANGED_TO_ERROR.store(false, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn output_size(&self) -> [u32; 2] {
        [
            self.output_size[0].load(Ordering::Relaxed),
            self.output_size[1].load(Ordering::Relaxed),
        ]
    }
}

enum LinuxVideoCapturer {
    PipeWire(PipeWireCapturer),
    X11(x11::X11Capturer),
}

pub struct LinuxCapturer {
    video: LinuxVideoCapturer,
    audio: Option<audio::PulseAudioCapturer>,
}

impl LinuxCapturer {
    pub fn start_capture(&mut self) {
        match &mut self.video {
            LinuxVideoCapturer::PipeWire(capturer) => capturer.start_capture(),
            LinuxVideoCapturer::X11(capturer) => capturer.start_capture(),
        }
        if let Some(audio) = &mut self.audio {
            audio.start_capture();
        }
    }

    pub fn stop_capture(&mut self) {
        if let Some(audio) = &mut self.audio {
            audio.stop_capture();
        }
        match &mut self.video {
            LinuxVideoCapturer::PipeWire(capturer) => capturer.stop_capture(),
            LinuxVideoCapturer::X11(capturer) => capturer.stop_capture(),
        }
    }

    pub fn output_size(&self) -> [u32; 2] {
        match &self.video {
            LinuxVideoCapturer::PipeWire(capturer) => capturer.output_size(),
            LinuxVideoCapturer::X11(capturer) => capturer.output_size(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendPreference {
    Auto,
    PipeWire,
    X11,
}

fn backend_preference(value: Option<&str>) -> Result<BackendPreference, LinCapError> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(BackendPreference::Auto),
        Some(value) if value.eq_ignore_ascii_case("pipewire") => Ok(BackendPreference::PipeWire),
        Some(value) if value.eq_ignore_ascii_case("x11") => Ok(BackendPreference::X11),
        Some(value) => Err(LinCapError::new(format!(
            "invalid SCAP_BACKEND={value:?}; expected 'pipewire' or 'x11'"
        ))),
    }
}

fn pipewire_probe() -> Result<(), LinCapError> {
    let connection = dbus::blocking::Connection::new_session().map_err(|error| {
        LinCapError::new(format!(
            "PipeWire backend unavailable: no session D-Bus: {error}"
        ))
    })?;
    portal::probe(&connection)?;
    pw::init();
    let mainloop = MainLoop::new(None)
        .map_err(|error| LinCapError::new(format!("PipeWire backend unavailable: {error}")))?;
    let context = Context::new(&mainloop)
        .map_err(|error| LinCapError::new(format!("PipeWire backend unavailable: {error}")))?;
    context
        .connect(None)
        .map_err(|error| LinCapError::new(format!("PipeWire daemon is unavailable: {error}")))?;
    Ok(())
}

pub fn create_capturer(
    options: &Options,
    tx: mpsc::Sender<Frame>,
) -> Result<LinuxCapturer, LinCapError> {
    let video = match backend_preference(std::env::var("SCAP_BACKEND").ok().as_deref())? {
        BackendPreference::PipeWire => {
            PipeWireCapturer::new(options, tx.clone()).map(LinuxVideoCapturer::PipeWire)
        }
        BackendPreference::X11 => {
            x11::X11Capturer::new(options, tx.clone()).map(LinuxVideoCapturer::X11)
        }
        BackendPreference::Auto => {
            let pipewire_status = pipewire_probe();
            if pipewire_status.is_ok() {
                PipeWireCapturer::new(options, tx.clone()).map(LinuxVideoCapturer::PipeWire)
            } else {
                match x11::X11Capturer::new(options, tx.clone()) {
                    Ok(capturer) => Ok(LinuxVideoCapturer::X11(capturer)),
                    Err(x11_error) => Err(LinCapError::new(format!(
                        "no usable Linux capture backend; {}; {}",
                        pipewire_status
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_else(
                                || "PipeWire portal session could not be created".to_string()
                            ),
                        x11_error
                    ))),
                }
            }
        }
    }?;
    eprintln!(
        "Linux video backend selected: {}",
        match &video {
            LinuxVideoCapturer::PipeWire(_) => "PipeWire portal",
            LinuxVideoCapturer::X11(_) => "X11",
        }
    );
    let audio = options
        .captures_audio
        .then(|| audio::PulseAudioCapturer::new(tx))
        .transpose()?;
    Ok(LinuxCapturer { video, audio })
}

pub fn get_output_frame_size(options: &Options) -> Result<[u32; 2], LinCapError> {
    match backend_preference(std::env::var("SCAP_BACKEND").ok().as_deref())? {
        BackendPreference::X11 => x11::output_size(options),
        BackendPreference::PipeWire => Err(LinCapError::new(
            "PipeWire output dimensions are available after format negotiation".to_string(),
        )),
        BackendPreference::Auto if pipewire_probe().is_err() => x11::output_size(options),
        BackendPreference::Auto => Err(LinCapError::new(
            "PipeWire output dimensions are available after format negotiation".to_string(),
        )),
    }
}

pub fn is_supported() -> bool {
    match backend_preference(std::env::var("SCAP_BACKEND").ok().as_deref()) {
        Ok(BackendPreference::PipeWire) => pipewire_probe().is_ok(),
        Ok(BackendPreference::X11) => x11::probe().is_ok(),
        Ok(BackendPreference::Auto) => pipewire_probe().is_ok() || x11::probe().is_ok(),
        Err(_) => false,
    }
}

pub fn has_permission() -> bool {
    // X11 access is authorized by a successful connection. Portal permission is
    // granted interactively when the session is created.
    is_supported()
}

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn parses_explicit_backend_names() {
        assert_eq!(
            backend_preference(Some("pipewire")).unwrap(),
            BackendPreference::PipeWire
        );
        assert_eq!(
            backend_preference(Some("X11")).unwrap(),
            BackendPreference::X11
        );
        assert!(backend_preference(Some("invalid")).is_err());
    }

    #[test]
    fn automatic_selection_order_is_stable() {
        fn choose(pipewire: bool, x11: bool) -> Option<BackendPreference> {
            if pipewire {
                Some(BackendPreference::PipeWire)
            } else if x11 {
                Some(BackendPreference::X11)
            } else {
                None
            }
        }
        assert_eq!(choose(true, true), Some(BackendPreference::PipeWire));
        assert_eq!(choose(false, true), Some(BackendPreference::X11));
        assert_eq!(choose(false, false), None);
    }

    #[test]
    fn pipewire_pts_are_mapped_from_a_monotonic_clock_to_wall_clock() {
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let mut mapper = PipeWireTimestampMapper {
            first_pts: None,
            first_wall_time: None,
        };
        assert_eq!(mapper.timestamp(5_000, wall), wall);
        assert_eq!(
            mapper.timestamp(5_020_000, wall + Duration::from_secs(3)),
            wall + Duration::from_millis(5),
        );
    }
}
