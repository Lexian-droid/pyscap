use cidre::{cg, ns, sc};
use cocoa::appkit::NSScreen;
use cocoa::base::{id, nil};
use cocoa::foundation::NSString;
use futures::executor::block_on;
use objc::{msg_send, sel, sel_impl};
use std::collections::HashMap;

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

#[derive(Clone, Debug)]
struct WindowMetadata {
    frame: cg::Rect,
    title: String,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> id;
}

fn cg_window_metadata() -> HashMap<cg::WindowId, WindowMetadata> {
    const K_CG_WINDOW_LIST_OPTION_ALL: u32 = 0;
    const K_CG_NULL_WINDOW_ID: u32 = 0;

    unsafe {
        let windows: id = CGWindowListCopyWindowInfo(
            K_CG_WINDOW_LIST_OPTION_ALL,
            K_CG_NULL_WINDOW_ID,
        );
        if windows == nil {
            return HashMap::new();
        }

        let number_key = NSString::alloc(nil).init_str("kCGWindowNumber");
        let bounds_key = NSString::alloc(nil).init_str("kCGWindowBounds");
        let name_key = NSString::alloc(nil).init_str("kCGWindowName");
        let x_key = NSString::alloc(nil).init_str("X");
        let y_key = NSString::alloc(nil).init_str("Y");
        let width_key = NSString::alloc(nil).init_str("Width");
        let height_key = NSString::alloc(nil).init_str("Height");

        let count: u64 = msg_send![windows, count];
        let mut metadata = HashMap::with_capacity(count as usize);
        for index in 0..count {
            let window: id = msg_send![windows, objectAtIndex: index];
            let number: id = msg_send![window, objectForKey: number_key];
            let bounds: id = msg_send![window, objectForKey: bounds_key];
            if number == nil || bounds == nil {
                continue;
            }

            let x: id = msg_send![bounds, objectForKey: x_key];
            let y: id = msg_send![bounds, objectForKey: y_key];
            let width: id = msg_send![bounds, objectForKey: width_key];
            let height: id = msg_send![bounds, objectForKey: height_key];
            if x == nil || y == nil || width == nil || height == nil {
                continue;
            }

            let x: f64 = msg_send![x, doubleValue];
            let y: f64 = msg_send![y, doubleValue];
            let width: f64 = msg_send![width, doubleValue];
            let height: f64 = msg_send![height, doubleValue];
            let name: id = msg_send![window, objectForKey: name_key];
            let title = if name == nil {
                String::new()
            } else {
                let chars: *const i8 = msg_send![name, UTF8String];
                if chars.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(chars).to_string_lossy().into_owned()
                }
            };

            metadata.insert(
                msg_send![number, unsignedIntValue],
                WindowMetadata {
                    frame: cg::Rect {
                        origin: cg::Point { x, y },
                        size: cg::Size { width, height },
                    },
                    title,
                },
            );
        }
        let _: () = msg_send![windows, release];
        metadata
    }
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

pub fn get_all_targets() -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();

    eprintln!("[PyScap macOS targets] request SCShareableContent");
    let content = block_on(sc::ShareableContent::current()).unwrap();
    eprintln!("[PyScap macOS targets] received SCShareableContent");

    // ScreenCaptureKit window frames and CoreGraphics display bounds use the
    // same global coordinate space. AppKit NSScreen frames do not (notably the
    // vertical axis and menu-bar origin), and querying NSScreen once per
    // foreign-process window also caused native crashes on Intel macOS.
    let displays = content.displays();
    eprintln!(
        "[PyScap macOS targets] enumerate {} ScreenCaptureKit displays",
        displays.len()
    );
    let display_metrics = displays
        .iter()
        .map(|display| {
            let id = display.display_id();
            eprintln!("[PyScap macOS targets] display {}: CoreGraphics bounds", id.0);
            let (x, y, width, height) = id.logical_bounds();
            eprintln!("[PyScap macOS targets] display {}: display mode", id.0);
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
    eprintln!("[PyScap macOS targets] display metrics complete");
    let window_metadata = cg_window_metadata();
    eprintln!(
        "[PyScap macOS targets] enumerate {} CoreGraphics windows",
        window_metadata.len()
    );

    // Add displays to targets
    for display in displays.iter() {
        let id = display.display_id();

        eprintln!("[PyScap macOS targets] display {}: AppKit name", id.0);
        let title = get_display_name(id);
        eprintln!("[PyScap macOS targets] display {}: target complete", id.0);

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
        let Some(metadata) = window_metadata.get(&id) else {
            eprintln!("[PyScap macOS targets] window {}: no CoreGraphics metadata", id);
            continue;
        };
        let frame = metadata.frame;
        let title = metadata.title.clone();
        eprintln!("[PyScap macOS targets] window {}: target complete", id);

        let target = Target::Window(super::Window {
            id,
            title,
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
            if mode.width() == 0 {
                1.0
            } else {
                mode.pixel_width() as f64 / mode.width() as f64
            }
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
