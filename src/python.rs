use numpy::{PyArray1, PyArrayMethods};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use crate::capturer::{Area, Capturer, Options, Point, Resolution, Size};
use crate::frame::{AudioFormat, AudioFrame, Frame, FrameType, VideoFrame};
use crate::{get_all_targets, has_permission, is_supported, request_permission, Target};

fn timestamp(value: std::time::SystemTime) -> PyResult<f64> {
    value
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| PyValueError::new_err("frame timestamp predates the Unix epoch"))
}

fn array(py: Python<'_>, data: Vec<u8>, shape: &[usize]) -> PyResult<Py<PyAny>> {
    let array = PyArray1::from_vec(py, data);
    let array = array
        .reshape(shape)
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(array.into_any().unbind())
}

#[pyclass(module = "scap", unsendable)]
#[derive(Clone)]
pub struct TargetInfo {
    target: Target,
    #[pyo3(get)]
    pub id: u32,
    #[pyo3(get)]
    pub title: String,
    #[pyo3(get)]
    pub kind: String,
}

#[pymethods]
impl TargetInfo {
    fn __repr__(&self) -> String {
        format!("TargetInfo(kind='{}', id={}, title={:?})", self.kind, self.id, self.title)
    }
}

impl TargetInfo {
    fn from_target(target: Target) -> Self {
        let (id, title, kind) = match &target {
            Target::Display(display) => (display.id, display.title.clone(), "display"),
            Target::Window(window) => (window.id, window.title.clone(), "window"),
        };
        Self {
            target,
            id,
            title,
            kind: kind.to_string(),
        }
    }
}

#[pyclass(module = "scap", unsendable)]
pub struct CaptureOptions {
    pub(crate) options: Options,
}

#[pymethods]
impl CaptureOptions {
    #[new]
    #[pyo3(signature = (fps=30, show_cursor=true, show_highlight=false, target=None, crop_area=None, output_type="bgra", output_resolution="captured", captures_audio=false, exclude_current_process_audio=false))]
    fn new(
        fps: u32,
        show_cursor: bool,
        show_highlight: bool,
        target: Option<PyRef<'_, TargetInfo>>,
        crop_area: Option<(f64, f64, f64, f64)>,
        output_type: &str,
        output_resolution: &str,
        captures_audio: bool,
        exclude_current_process_audio: bool,
    ) -> PyResult<Self> {
        if fps == 0 {
            return Err(PyValueError::new_err("fps must be greater than zero"));
        }
        let output_type = match output_type.to_ascii_lowercase().as_str() {
            "bgra" | "bgraframe" | "bgr0" => FrameType::BGRAFrame,
            "rgb" => FrameType::RGB,
            "yuv" | "yuvframe" => FrameType::YUVFrame,
            _ => return Err(PyValueError::new_err("output_type must be 'bgra', 'rgb', or 'yuv'")),
        };
        let output_resolution = match output_resolution.to_ascii_lowercase().as_str() {
            "captured" => Resolution::Captured,
            "480p" => Resolution::_480p,
            "720p" => Resolution::_720p,
            "1080p" => Resolution::_1080p,
            "1440p" => Resolution::_1440p,
            "2160p" => Resolution::_2160p,
            "4320p" => Resolution::_4320p,
            _ => return Err(PyValueError::new_err("unknown output_resolution")),
        };
        Ok(Self {
            options: Options {
                fps,
                show_cursor,
                show_highlight,
                target: target.map(|value| value.target.clone()),
                crop_area: crop_area.map(|(x, y, width, height)| Area {
                    origin: Point { x, y },
                    size: Size { width, height },
                }),
                output_type,
                output_resolution,
                excluded_targets: None,
                captures_audio,
                exclude_current_process_audio,
            },
        })
    }
}

#[pyclass(module = "scap")]
pub struct VideoFrameInfo {
    #[pyo3(get)]
    pub data: Py<PyAny>,
    #[pyo3(get)]
    pub width: usize,
    #[pyo3(get)]
    pub height: usize,
    #[pyo3(get)]
    pub format: String,
    #[pyo3(get)]
    pub timestamp: f64,
}

#[pyclass(module = "scap")]
pub struct AudioFrameInfo {
    #[pyo3(get)]
    pub data: Py<PyAny>,
    #[pyo3(get)]
    pub channels: usize,
    #[pyo3(get)]
    pub rate: usize,
    #[pyo3(get)]
    pub sample_count: usize,
    #[pyo3(get)]
    pub format: String,
    #[pyo3(get)]
    pub planar: bool,
    #[pyo3(get)]
    pub timestamp: f64,
}

fn video_frame(py: Python<'_>, frame: VideoFrame) -> PyResult<VideoFrameInfo> {
    let (data, width, height, format, time) = match frame {
        VideoFrame::YUVFrame(_) => return Err(PyValueError::new_err(
            "YUV frames are not supported by the Python binding yet; use output_type='bgra' or 'rgb'",
        )),
        VideoFrame::RGB(frame) => (frame.data, frame.width, frame.height, "rgb", frame.display_time),
        VideoFrame::RGBx(frame) => (frame.data, frame.width, frame.height, "rgbx", frame.display_time),
        VideoFrame::XBGR(frame) => (frame.data, frame.width, frame.height, "xbgr", frame.display_time),
        VideoFrame::BGRx(frame) => (frame.data, frame.width, frame.height, "bgrx", frame.display_time),
        VideoFrame::BGR0(frame) => (frame.data, frame.width, frame.height, "bgr0", frame.display_time),
        VideoFrame::BGRA(frame) => (frame.data, frame.width, frame.height, "bgra", frame.display_time),
    };
    let pixels = width as usize * height as usize;
    if pixels == 0 || data.len() % pixels != 0 {
        return Err(PyValueError::new_err("captured frame has invalid dimensions"));
    }
    let channels = data.len() / pixels;
    Ok(VideoFrameInfo {
        data: array(py, data, &[height as usize, width as usize, channels])?,
        width: width as usize,
        height: height as usize,
        format: format.to_string(),
        timestamp: timestamp(time)?,
    })
}

fn audio_format(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::I8 => "int8",
        AudioFormat::I16 => "int16",
        AudioFormat::I32 => "int32",
        AudioFormat::I64 => "int64",
        AudioFormat::U8 => "uint8",
        AudioFormat::U16 => "uint16",
        AudioFormat::U32 => "uint32",
        AudioFormat::U64 => "uint64",
        AudioFormat::F32 => "float32",
        AudioFormat::F64 => "float64",
    }
}

fn audio_frame(py: Python<'_>, frame: AudioFrame) -> PyResult<AudioFrameInfo> {
    let format = audio_format(frame.format()).to_string();
    let channels = frame.channels() as usize;
    let sample_count = frame.sample_count();
    let data = frame.raw_data().to_vec();
    let data_len = data.len();
    Ok(AudioFrameInfo {
        data: array(py, data, &[data_len])?,
        channels,
        rate: frame.rate() as usize,
        sample_count,
        format,
        planar: frame.is_planar(),
        timestamp: timestamp(frame.time())?,
    })
}

#[pyclass(name = "Capturer", module = "scap", unsendable)]
pub struct PyCapturer {
    capturer: Option<Capturer>,
    started: bool,
}

#[pymethods]
impl PyCapturer {
    #[new]
    #[pyo3(signature = (options=None))]
    fn new(options: Option<PyRef<'_, CaptureOptions>>) -> PyResult<Self> {
        let options = options.map(|value| value.options.clone()).unwrap_or_default();
        let capturer = Capturer::build(options).map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        Ok(Self { capturer: Some(capturer), started: false })
    }

    fn start(&mut self) -> PyResult<()> {
        if self.started {
            return Err(PyRuntimeError::new_err("capture has already started"));
        }
        self.capturer.as_mut().unwrap().start_capture();
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) {
        if self.started {
            self.capturer.as_mut().unwrap().stop_capture();
            self.started = false;
        }
    }

    fn output_size(&mut self) -> (u32, u32) {
        let size = self.capturer.as_mut().unwrap().get_output_frame_size();
        (size[0], size[1])
    }

    fn next_frame<'py>(&mut self, py: Python<'py>) -> PyResult<Py<PyAny>> {
        if !self.started {
            return Err(PyRuntimeError::new_err("capture has not started"));
        }
        let frame = self.capturer.as_ref().unwrap().get_next_frame()
            .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
        match frame {
            Frame::Video(frame) => Ok(Py::new(py, video_frame(py, frame)?)?.into_any()),
            Frame::Audio(frame) => Ok(Py::new(py, audio_frame(py, frame)?)?.into_any()),
        }
    }
}

#[pymodule]
fn scap(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(is_supported_py, module)?)?;
    module.add_function(wrap_pyfunction!(has_permission_py, module)?)?;
    module.add_function(wrap_pyfunction!(request_permission_py, module)?)?;
    module.add_function(wrap_pyfunction!(targets, module)?)?;
    module.add_class::<TargetInfo>()?;
    module.add_class::<CaptureOptions>()?;
    module.add_class::<VideoFrameInfo>()?;
    module.add_class::<AudioFrameInfo>()?;
    module.add_class::<PyCapturer>()?;
    Ok(())
}

#[pyfunction(name = "is_supported")]
fn is_supported_py() -> bool { is_supported() }

#[pyfunction(name = "has_permission")]
fn has_permission_py() -> bool { has_permission() }

#[pyfunction(name = "request_permission")]
fn request_permission_py() -> bool { request_permission() }

#[pyfunction]
fn targets() -> Vec<TargetInfo> {
    get_all_targets().into_iter().map(TargetInfo::from_target).collect()
}