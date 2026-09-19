use cidre::cg;
use core_graphics_helmer_fork::display::{CGDisplay, CGDisplayMode};

pub trait DirectDisplayIdExt {
    fn display_mode(&self) -> Option<CGDisplayMode>;
    fn logical_bounds(&self) -> (f64, f64, f64, f64);
}

impl DirectDisplayIdExt for cg::DirectDisplayId {
    #[inline]
    fn display_mode(&self) -> Option<CGDisplayMode> {
        CGDisplay::new(self.0).display_mode()
    }

    #[inline]
    fn logical_bounds(&self) -> (f64, f64, f64, f64) {
        let bounds = CGDisplay::new(self.0).bounds();
        (
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.width,
            bounds.size.height,
        )
    }
}
