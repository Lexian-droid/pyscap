use std::{
    ptr::NonNull,
    sync::mpsc::{self, Receiver, Sender},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use x11rb::{
    connection::Connection,
    protocol::{
        shm::{self, ConnectionExt as _},
        xproto::{ConnectionExt as _, ImageFormat, ImageOrder},
    },
    rust_connection::RustConnection,
};

use crate::{
    capturer::{Options, Resolution},
    frame::{BGRAFrame, BGRFrame, Frame, FrameType, RGBFrame, VideoFrame},
};

use super::error::LinCapError;

#[derive(Clone)]
struct ImageLayout {
    root: u32,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    depth: u8,
    bits_per_pixel: u8,
    scanline_pad: u8,
    little_endian: bool,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
}

impl ImageLayout {
    fn stride(&self) -> usize {
        let bits = self.width as usize * self.bits_per_pixel as usize;
        let pad = self.scanline_pad as usize;
        bits.div_ceil(pad) * (pad / 8)
    }

    fn byte_len(&self) -> usize {
        self.stride() * self.height as usize
    }
}

pub struct X11Capturer {
    options: Options,
    tx: mpsc::Sender<Frame>,
    layout: ImageLayout,
    stop_tx: Option<Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl X11Capturer {
    pub fn new(options: &Options, tx: mpsc::Sender<Frame>) -> Result<Self, LinCapError> {
        if matches!(options.output_type, FrameType::YUVFrame) {
            return Err(LinCapError::new(
                "X11 backend does not support YUV output; use BGRAFrame, BGR0, or RGB".to_string(),
            ));
        }
        let (connection, screen_number) = connect()?;
        let layout = layout(&connection, screen_number, options)?;
        Ok(Self {
            options: options.clone(),
            tx,
            layout,
            stop_tx: None,
            join_handle: None,
        })
    }

    pub fn start_capture(&mut self) {
        if self.join_handle.is_some() {
            return;
        }
        let options = self.options.clone();
        let layout = self.layout.clone();
        let tx = self.tx.clone();
        let (stop_tx, stop_rx) = mpsc::channel();
        self.stop_tx = Some(stop_tx);
        self.join_handle = Some(std::thread::spawn(move || {
            if let Err(error) = capture_loop(options, layout, tx, stop_rx) {
                eprintln!("X11 capture stopped: {error}");
            }
        }));
    }

    pub fn stop_capture(&mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }

    pub fn output_size(&self) -> [u32; 2] {
        [self.layout.width as u32, self.layout.height as u32]
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

fn connect() -> Result<(RustConnection, usize), LinCapError> {
    let display = std::env::var("DISPLAY")
        .map_err(|_| LinCapError::new("X11 backend unavailable: DISPLAY is not set".to_string()))?;
    x11rb::connect(Some(&display)).map_err(|error| {
        LinCapError::new(format!(
            "X11 backend unavailable: cannot connect to X server at DISPLAY={display}: {error}"
        ))
    })
}

fn layout(
    connection: &RustConnection,
    screen_number: usize,
    options: &Options,
) -> Result<ImageLayout, LinCapError> {
    if !matches!(options.output_resolution, Resolution::Captured) {
        return Err(LinCapError::new(
            "X11 backend currently supports output_resolution='captured' only".to_string(),
        ));
    }
    let setup = connection.setup();
    let screen = setup
        .roots
        .get(screen_number)
        .ok_or_else(|| LinCapError::new(format!("X11 server has no screen {screen_number}")))?;
    let format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == screen.root_depth)
        .ok_or_else(|| {
            LinCapError::new(format!(
                "X11 root depth {} has no matching pixmap format",
                screen.root_depth
            ))
        })?;
    if !matches!(format.bits_per_pixel, 16 | 24 | 32) {
        return Err(LinCapError::new(format!(
            "unsupported X11 root pixel size: {} bits per pixel",
            format.bits_per_pixel
        )));
    }
    if !matches!(format.scanline_pad, 8 | 16 | 32) {
        return Err(LinCapError::new(format!(
            "unsupported X11 scanline padding: {} bits",
            format.scanline_pad
        )));
    }
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|visual| visual.visual_id == screen.root_visual)
        .ok_or_else(|| LinCapError::new("X11 root visual was not found".to_string()))?;

    let (x, y, width, height) = crop(screen.width_in_pixels, screen.height_in_pixels, options)?;
    Ok(ImageLayout {
        root: screen.root,
        x,
        y,
        width,
        height,
        depth: screen.root_depth,
        bits_per_pixel: format.bits_per_pixel,
        scanline_pad: format.scanline_pad,
        little_endian: setup.image_byte_order == ImageOrder::LSB_FIRST,
        red_mask: visual.red_mask,
        green_mask: visual.green_mask,
        blue_mask: visual.blue_mask,
    })
}

fn crop(
    screen_width: u16,
    screen_height: u16,
    options: &Options,
) -> Result<(i16, i16, u16, u16), LinCapError> {
    let Some(area) = &options.crop_area else {
        return Ok((0, 0, screen_width, screen_height));
    };
    let values = [
        area.origin.x,
        area.origin.y,
        area.size.width,
        area.size.height,
    ];
    if values.iter().any(|value| !value.is_finite())
        || area.origin.x < 0.0
        || area.origin.y < 0.0
        || area.size.width <= 0.0
        || area.size.height <= 0.0
    {
        return Err(LinCapError::new("invalid X11 crop area".to_string()));
    }
    let x = area.origin.x as u32;
    let y = area.origin.y as u32;
    let width = area.size.width as u32;
    let height = area.size.height as u32;
    if x + width > screen_width as u32 || y + height > screen_height as u32 {
        return Err(LinCapError::new(format!(
            "X11 crop area exceeds the {}x{} root window",
            screen_width, screen_height
        )));
    }
    Ok((x as i16, y as i16, width as u16, height as u16))
}

fn capture_loop(
    options: Options,
    expected_layout: ImageLayout,
    tx: mpsc::Sender<Frame>,
    stop_rx: Receiver<()>,
) -> Result<(), LinCapError> {
    let (connection, screen_number) = connect()?;
    let current_layout = layout(&connection, screen_number, &options)?;
    if current_layout.width != expected_layout.width
        || current_layout.height != expected_layout.height
    {
        return Err(LinCapError::new(
            "X11 display dimensions changed before capture started".to_string(),
        ));
    }

    let mut shared_image = SharedImage::new(&connection, current_layout.byte_len()).ok();
    let frame_interval = Duration::from_secs_f64(1.0 / options.fps.max(1) as f64);
    loop {
        let started = Instant::now();
        let shared_result = shared_image
            .as_mut()
            .map(|image| image.capture(&connection, &current_layout));
        let raw = match shared_result {
            Some(Ok(raw)) => raw,
            Some(Err(error)) => {
                eprintln!("MIT-SHM capture failed; falling back to X11 GetImage: {error}");
                shared_image = None;
                get_image(&connection, &current_layout)?
            }
            None => get_image(&connection, &current_layout)?,
        };
        let frame = convert_frame(&raw, &current_layout, options.output_type)?;
        if tx.send(Frame::Video(frame)).is_err() {
            break;
        }
        let remaining = frame_interval.saturating_sub(started.elapsed());
        if stop_rx.recv_timeout(remaining).is_ok() {
            break;
        }
    }
    Ok(())
}

fn get_image(connection: &RustConnection, layout: &ImageLayout) -> Result<Vec<u8>, LinCapError> {
    let reply = connection
        .get_image(
            ImageFormat::Z_PIXMAP,
            layout.root,
            layout.x,
            layout.y,
            layout.width,
            layout.height,
            u32::MAX,
        )
        .map_err(|error| LinCapError::new(format!("X11 GetImage request failed: {error}")))?
        .reply()
        .map_err(|error| LinCapError::new(format!("X11 GetImage failed: {error}")))?;
    if reply.data.len() < layout.byte_len() {
        return Err(LinCapError::new(format!(
            "X11 returned {} image bytes; expected at least {}",
            reply.data.len(),
            layout.byte_len()
        )));
    }
    Ok(reply.data)
}

struct SharedImage {
    segment: shm::Seg,
    address: NonNull<u8>,
    size: usize,
}

// The worker owns this mapping and never shares it with another Rust thread.
unsafe impl Send for SharedImage {}

impl SharedImage {
    fn new(connection: &RustConnection, size: usize) -> Result<Self, LinCapError> {
        let query = connection
            .shm_query_version()
            .map_err(|error| LinCapError::new(format!("MIT-SHM unavailable: {error}")))?;
        query
            .reply()
            .map_err(|error| LinCapError::new(format!("MIT-SHM unavailable: {error}")))?;
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            return Err(LinCapError::new(format!(
                "MIT-SHM allocation failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let raw = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if raw == (-1_isize) as *mut libc::c_void {
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err(LinCapError::new(format!(
                "MIT-SHM attach failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let address = NonNull::new(raw.cast::<u8>()).unwrap();
        let segment = connection.generate_id().map_err(|error| {
            unsafe {
                libc::shmdt(raw);
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            }
            LinCapError::new(format!("MIT-SHM resource allocation failed: {error}"))
        })?;
        let attach = connection
            .shm_attach(segment, shmid as u32, false)
            .map_err(|error| LinCapError::new(error.to_string()));
        if let Err(error) = attach.and_then(|cookie| {
            cookie
                .check()
                .map_err(|error| LinCapError::new(error.to_string()))
        }) {
            unsafe {
                libc::shmdt(raw);
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            }
            return Err(LinCapError::new(format!(
                "MIT-SHM server attach failed: {error}"
            )));
        }
        // Mark it for deletion now; the mapping remains valid until both peers detach.
        unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
        Ok(Self {
            segment,
            address,
            size,
        })
    }

    fn capture(
        &mut self,
        connection: &RustConnection,
        layout: &ImageLayout,
    ) -> Result<Vec<u8>, LinCapError> {
        connection
            .shm_get_image(
                layout.root,
                layout.x,
                layout.y,
                layout.width,
                layout.height,
                u32::MAX,
                u8::from(ImageFormat::Z_PIXMAP),
                self.segment,
                0,
            )
            .map_err(|error| LinCapError::new(format!("MIT-SHM request failed: {error}")))?
            .reply()
            .map_err(|error| LinCapError::new(format!("MIT-SHM capture failed: {error}")))?;
        Ok(unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.size) }.to_vec())
    }
}

impl Drop for SharedImage {
    fn drop(&mut self) {
        // The X server detaches when its connection closes; this drops our local mapping.
        unsafe { libc::shmdt(self.address.as_ptr().cast()) };
    }
}

fn convert_frame(
    raw: &[u8],
    layout: &ImageLayout,
    output: FrameType,
) -> Result<VideoFrame, LinCapError> {
    if matches!(output, FrameType::YUVFrame) {
        return Err(LinCapError::new(
            "X11 backend does not support YUV output; use BGRAFrame, BGR0, or RGB".to_string(),
        ));
    }
    let channels = if matches!(output, FrameType::RGB) {
        3
    } else {
        4
    };
    let mut converted =
        Vec::with_capacity(layout.width as usize * layout.height as usize * channels);
    let bytes_per_pixel = layout.bits_per_pixel as usize / 8;
    let stride = layout.stride();
    for row in raw.chunks(stride).take(layout.height as usize) {
        for pixel in row.chunks(bytes_per_pixel).take(layout.width as usize) {
            let value = pixel_value(pixel, layout.little_endian);
            let red = component(value, layout.red_mask);
            let green = component(value, layout.green_mask);
            let blue = component(value, layout.blue_mask);
            match output {
                FrameType::RGB => converted.extend_from_slice(&[red, green, blue]),
                FrameType::BGRAFrame => converted.extend_from_slice(&[blue, green, red, 255]),
                FrameType::BGR0 => converted.extend_from_slice(&[blue, green, red, 0]),
                FrameType::YUVFrame => unreachable!(),
            }
        }
    }
    let display_time = SystemTime::now();
    let width = layout.width as i32;
    let height = layout.height as i32;
    Ok(match output {
        FrameType::RGB => VideoFrame::RGB(RGBFrame {
            display_time,
            width,
            height,
            data: converted,
        }),
        FrameType::BGRAFrame => VideoFrame::BGRA(BGRAFrame {
            display_time,
            width,
            height,
            data: converted,
        }),
        FrameType::BGR0 => VideoFrame::BGR0(BGRFrame {
            display_time,
            width,
            height,
            data: converted,
        }),
        FrameType::YUVFrame => unreachable!(),
    })
}

fn pixel_value(bytes: &[u8], little_endian: bool) -> u32 {
    if little_endian {
        bytes.iter().enumerate().fold(0, |value, (index, byte)| {
            value | ((*byte as u32) << (index * 8))
        })
    } else {
        bytes
            .iter()
            .fold(0, |value, byte| (value << 8) | *byte as u32)
    }
}

fn component(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let maximum = mask >> shift;
    ((((pixel & mask) >> shift) as u64 * 255) / maximum as u64) as u8
}

pub fn probe() -> Result<(), LinCapError> {
    let (connection, screen_number) = connect()?;
    if connection.setup().roots.get(screen_number).is_none() {
        return Err(LinCapError::new(format!(
            "X11 server has no screen {screen_number}"
        )));
    }
    Ok(())
}

pub fn output_size(options: &Options) -> Result<[u32; 2], LinCapError> {
    let (connection, screen_number) = connect()?;
    let layout = layout(&connection, screen_number, options)?;
    Ok([layout.width as u32, layout.height as u32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_common_little_endian_xrgb_pixel() {
        assert_eq!(pixel_value(&[0x33, 0x22, 0x11, 0], true), 0x0011_2233);
        assert_eq!(component(0x0011_2233, 0x00ff_0000), 0x11);
        assert_eq!(component(0x0011_2233, 0x0000_ff00), 0x22);
        assert_eq!(component(0x0011_2233, 0x0000_00ff), 0x33);
    }

    #[test]
    fn scales_rgb565_components() {
        assert_eq!(component(0xf800, 0xf800), 255);
        assert_eq!(component(0x07e0, 0x07e0), 255);
        assert_eq!(component(0x001f, 0x001f), 255);
    }
}
