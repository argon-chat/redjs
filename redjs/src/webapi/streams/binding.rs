pub(super) use super::super::class_binding::*;

pub(super) unsafe fn env(g: *mut c_void) -> Env {
    let classes = super::classes(g);
    Env::new(g, classes.slot, classes.interfaces)
}

macro_rules! stream_fn {
    ($($tokens:tt)*) => { native_callback!(env, $($tokens)*); };
}
