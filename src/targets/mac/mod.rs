use cidre::{cg, ns, sc};
use cocoa::appkit::{NSApp, NSScreen};
use cocoa::base::{id, nil};
use cocoa::foundation::{NSRect, NSString, NSUInteger};
use futures::executor::block_on;
use objc::{msg_send, sel, sel_impl};

use crate::engine::mac::ext::DirectDisplayIdExt;

use super::{Display, Target};

#[derive(Clone, Copy, Debug)]
struct DisplayMetrics {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
}

fn get_display_name(display_id: cg::DirectDisplayId) -> String {
    unsafe {
        // Get all screens
        let screens: id = NSScreen::screens(nil);
        let count: u64 = msg_send![screens, count];

        for i in 0..count {
            let screen: id = msg_send![screens, objectAtIndex: i];
            let device_description: id = msg_send![screen, deviceDescription];
            let display_id_number: id = msg_send![device_description, objectForKey: NSString::alloc(nil).init_str("NSScreenNumber")];
            let display_id_number: u32 = msg_send![display_id_number, unsignedIntValue];

            if display_id_number == display_id.0 {
                let localized_name: id = msg_send![screen, localizedName];
                let name: *const i8 = msg_send![localized_name, UTF8String];
                return std::ffi::CStr::from_ptr(name)
                    .to_string_lossy()
                    .into_owned();
            }
        }

        format!("Unknown Display {}", display_id.0)
    }
}

fn scale_factor_for_frame(frame: cg::Rect, displays: &[DisplayMetrics]) -> f64 {
    let center_x = frame.origin.x + frame.size.width / 2.0;
    let center_y = frame.origin.y + frame.size.height / 2.0;

    displays
        .iter()
        .find(|display| {
            center_x >= display.x
                && center_x < display.x + display.width
                && center_y >= display.y
                && center_y < display.y + display.height
        })
        .map(|display| display.scale)
        // A window can temporarily be outside every display while spaces or
        // displays are changing. One point is one pixel on non-Retina output,
        // making 1.0 the only safe fallback that cannot produce a 0x0 stream.
        .unwrap_or(1.0)
}

fn window_title(window: &sc::Window) -> String {
    let Some(title) = window.title() else {
        return String::new();
    };

    // SCWindow.title is nullable. cidre 0.10 can expose a null NSString as
    // Some on Intel macOS, so do not invoke NSString's Display conversion
    // until its UTF-8 pointer has been checked.
    let chars = unsafe { title.utf8_chars_ar() };
    if chars.is_null() {
        return String::new();
    }

    unsafe { std::ffi::CStr::from_ptr(chars) }
        .to_string_lossy()
        .into_owned()
}

pub fn get_all_targets() -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();

    let content = block_on(sc::ShareableContent::current()).unwrap();

    // ScreenCaptureKit window frames and CoreGraphics display bounds use the
    // same global coordinate space. AppKit NSScreen frames do not (notably the
    // vertical axis and menu-bar origin), and querying NSScreen once per
    // foreign-process window also caused native crashes on Intel macOS.
    let display_metrics = content
        .displays()
        .iter()
        .map(|display| {
            let id = display.display_id();
            let (x, y, width, height) = id.logical_bounds();
            let scale = id
                .display_mode()
                .filter(|mode| mode.width() > 0)
                .map(|mode| mode.pixel_width() as f64 / mode.width() as f64)
                .filter(|scale| scale.is_finite() && *scale > 0.0)
                .unwrap_or(1.0);
            DisplayMetrics {
                x,
                y,
                width,
                height,
                scale,
            }
        })
        .collect::<Vec<_>>();

    // Add displays to targets
    for display in content.displays().iter() {
        let id = display.display_id();

        let title = get_display_name(id);

        let target = Target::Display(super::Display {
            id: id.0,
            title,
            raw_handle: id,
        });

        targets.push(target);
    }

    // Add windows to targets
    for window in content.windows().iter() {
        let id = window.id();
        let frame = window.frame();

        let target = Target::Window(super::Window {
            id,
            title: window_title(window),
            raw_handle: id,
            frame,
            scale_factor: scale_factor_for_frame(frame, &display_metrics),
        });
        targets.push(target);
    }

    targets
}

pub fn get_main_display() -> Display {
    let id = cg::direct_display::Id::main();
    let title = get_display_name(id);

    Display {
        id: id.0,
        title,
        raw_handle: id,
    }
}

pub fn get_scale_factor(target: &Target) -> f64 {
    match target {
        Target::Window(window) => window.scale_factor,
        Target::Display(display) => {
            let mode = display.raw_handle.display_mode().unwrap();
            (mode.pixel_width() / mode.width()) as f64
        }
    }
}

pub fn get_target_dimensions(target: &Target) -> (u64, u64) {
    match target {
        Target::Window(window) => (
            window.frame.size.width as u64,
            window.frame.size.height as u64,
        ),
        Target::Display(display) => {
            let mode = display.raw_handle.display_mode().unwrap();
            (mode.width(), mode.height())
        }
    }
}

pub fn diagnose_appkit_window(window_id: cg::WindowId) -> Option<(NSRect, f64)> {
    unsafe {
        let ns_app: id = NSApp();
        let ns_window: id = msg_send![ns_app, windowWithWindowNumber: window_id as NSUInteger];
        if ns_window == nil {
            return None;
        }
        let frame: NSRect = msg_send![ns_window, frame];
        let scale: f64 = msg_send![ns_window, backingScaleFactor];
        Some((frame, scale))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_factor_uses_core_graphics_display_containing_window_center() {
        let displays = [
            DisplayMetrics {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
                scale: 1.0,
            },
            DisplayMetrics {
                x: 1920.0,
                y: 0.0,
                width: 1512.0,
                height: 982.0,
                scale: 2.0,
            },
        ];
        let frame = cg::Rect {
            origin: cg::Point {
                x: 2100.0,
                y: 100.0,
            },
            size: cg::Size {
                width: 800.0,
                height: 600.0,
            },
        };

        assert_eq!(scale_factor_for_frame(frame, &displays), 2.0);
    }

    #[test]
    fn scale_factor_falls_back_to_one_for_offscreen_window() {
        let frame = cg::Rect {
            origin: cg::Point {
                x: -5000.0,
                y: -5000.0,
            },
            size: cg::Size {
                width: 800.0,
                height: 600.0,
            },
        };

        assert_eq!(scale_factor_for_frame(frame, &[]), 1.0);
    }
}
