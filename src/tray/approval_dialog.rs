use super::*;

#[path = "approval_dialog/animation.rs"]
mod animation;
#[path = "approval_dialog/hit_test.rs"]
mod hit_test;
#[path = "approval_dialog/paint.rs"]
mod paint;
#[path = "approval_dialog/window.rs"]
mod window;

#[allow(unused_imports)]
pub(super) use animation::{
    approval_ease, approval_lerp, approval_scale, hide_approval_popup, refresh_approval_popup,
};
pub(super) use hit_test::{approval_allow_rect, approval_deny_rect};
pub(super) use window::approval_window_proc;
