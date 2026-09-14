pub fn is_supported() -> bool {
    crate::capturer::engine::linux::is_supported()
}

pub fn has_permission() -> bool {
    crate::capturer::engine::linux::has_permission()
}
