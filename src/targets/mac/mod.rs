use cidre::{cg, ns, sc};
use cocoa::appkit::{NSApp, NSScreen};
use cocoa::base::{id, nil};
use cocoa::foundation::{NSRect, NSString, NSUInteger};
use futures::executor::block_on;
use objc::{msg_send, sel, sel_impl};

use crate::engine::mac::ext::DirectDisplayIdExt;

use super::{Display, Target};

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

fn scale_factor_for_frame(frame: cg::Rect) -> f64 {
    unsafe {
        let screens: id = NSScreen::screens(nil);
        let count: u64 = msg_send![screens, count];
        let center = cocoa::foundation::NSPoint {
            x: (frame.origin.x + frame.size.width / 2.0) as f64,
            y: (frame.origin.y + frame.size.height / 2.0) as f64,
        };
        for i in 0..count {
            let screen: id = msg_send![screens, objectAtIndex: i];
            let screen_frame: NSRect = msg_send![screen, frame];
            if center.x >= screen_frame.origin.x
                && center.x < screen_frame.origin.x + screen_frame.size.width
                && center.y >= screen_frame.origin.y
                && center.y < screen_frame.origin.y + screen_frame.size.height
            {
                let scale: f64 = msg_send![screen, backingScaleFactor];
                return scale;
            }
        }
    }
    0.0
}

pub fn get_all_targets() -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();

    let content = block_on(sc::ShareableContent::current()).unwrap();

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
        let title = window
            .title()
            // on intel chips we can have Some but also a null pointer for some reason
            .filter(|v| !unsafe { v.utf8_chars_ar().is_null() });

        let target = Target::Window(super::Window {
            id,
            title: title.map(|v| v.to_string()).unwrap_or_default(),
            raw_handle: id,
            frame,
            scale_factor: scale_factor_for_frame(frame),
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
