// Controlled side: when the controller is a mobile device, only the central
// `ASPECT_RATIO` region of each display is captured, scaled by `SCALE`, and
// advertised to the peer with the scaled size. Coordinates are mapped back here.

use hbb_common::message_proto::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub const SCALE: f32 = 0.75;
pub const ASPECT_RATIO: f32 = 16.0 / 9.0;

static MOBILE_CONNS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScaleRect {
    pub crop_x: usize,
    pub crop_y: usize,
    pub crop_w: usize,
    pub crop_h: usize,
    pub out_w: usize,
    pub out_h: usize,
}

#[inline]
pub fn is_active() -> bool {
    MOBILE_CONNS.load(Ordering::SeqCst) > 0
}

pub fn compute(width: usize, height: usize) -> ScaleRect {
    let even = |v: usize| (v & !1).max(2);
    let (mut crop_w, mut crop_h) = (width, height);
    if width as f32 / height as f32 > ASPECT_RATIO {
        crop_w = (height as f32 * ASPECT_RATIO).round() as usize;
    } else {
        crop_h = (width as f32 / ASPECT_RATIO).round() as usize;
    }
    let crop_w = even(crop_w.min(width));
    let crop_h = even(crop_h.min(height));
    ScaleRect {
        crop_x: (width - crop_w) / 2,
        crop_y: (height - crop_h) / 2,
        crop_w,
        crop_h,
        out_w: even((crop_w as f32 * SCALE).round() as usize),
        out_h: even((crop_h as f32 * SCALE).round() as usize),
    }
}

// `None` when no mobile controller is connected, so the capturer runs unchanged.
pub fn current(width: usize, height: usize) -> Option<ScaleRect> {
    if !is_active() || width == 0 || height == 0 {
        return None;
    }
    Some(compute(width, height))
}

pub fn is_mobile_platform(platform: &str) -> bool {
    let p = platform.to_lowercase();
    p == "android" || p == "ios"
}

// Per-connection state, held by `Connection`.
#[derive(Debug, Default)]
pub struct MobileScale {
    enabled: bool,
    displays: Vec<DisplayInfo>,
}

impl MobileScale {
    pub fn enable(&mut self) {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if !self.enabled {
            self.enabled = true;
            MOBILE_CONNS.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    pub fn set_displays(&mut self, displays: &[DisplayInfo]) {
        if self.enabled {
            self.displays = displays.to_vec();
        }
    }

    fn rect_of(&self, current: usize) -> Option<(&DisplayInfo, ScaleRect)> {
        let d = self.displays.get(current)?;
        if d.width <= 0 || d.height <= 0 {
            return None;
        }
        Some((d, compute(d.width as usize, d.height as usize)))
    }

    // `original_resolution` is set to the scaled size too, so the peer never
    // records a custom resolution and asks us to change the real display.
    fn scale_display(d: &mut DisplayInfo) {
        if d.width > 0 && d.height > 0 {
            let r = compute(d.width as usize, d.height as usize);
            d.width = r.out_w as _;
            d.height = r.out_h as _;
            d.original_resolution = Some(Resolution {
                width: d.width,
                height: d.height,
                ..Default::default()
            })
            .into();
        }
    }

    // Rewrite the display geometry the peer sees. Returns whether `msg` was changed.
    pub fn scale_msg(&self, msg: &mut Message, current: usize) -> bool {
        if !self.enabled {
            return false;
        }
        match &mut msg.union {
            Some(message::Union::LoginResponse(lr)) => match &mut lr.union {
                Some(login_response::Union::PeerInfo(pi)) => {
                    pi.displays.iter_mut().for_each(Self::scale_display);
                    true
                }
                _ => false,
            },
            Some(message::Union::PeerInfo(pi)) => {
                pi.displays.iter_mut().for_each(Self::scale_display);
                true
            }
            Some(message::Union::Misc(misc)) => match &mut misc.union {
                Some(misc::Union::SwitchDisplay(sd)) => {
                    if sd.width > 0 && sd.height > 0 {
                        let r = compute(sd.width as usize, sd.height as usize);
                        sd.width = r.out_w as _;
                        sd.height = r.out_h as _;
                        sd.original_resolution = Some(Resolution {
                            width: sd.width,
                            height: sd.height,
                            ..Default::default()
                        })
                        .into();
                    }
                    true
                }
                _ => false,
            },
            Some(message::Union::CursorPosition(pos)) => {
                let Some((d, r)) = self.rect_of(current) else {
                    return false;
                };
                let sx = r.out_w as f64 / r.crop_w as f64;
                let sy = r.out_h as f64 / r.crop_h as f64;
                pos.x = d.x + ((pos.x - d.x - r.crop_x as i32) as f64 * sx).round() as i32;
                pos.y = d.y + ((pos.y - d.y - r.crop_y as i32) as f64 * sy).round() as i32;
                true
            }
            _ => false,
        }
    }

    pub fn scale_shared(&self, msg: Arc<Message>, current: usize) -> Arc<Message> {
        if !self.enabled {
            return msg;
        }
        match &msg.union {
            Some(message::Union::PeerInfo(_)) | Some(message::Union::CursorPosition(_)) => {}
            Some(message::Union::Misc(misc)) => match &misc.union {
                Some(misc::Union::SwitchDisplay(_)) => {}
                _ => return msg,
            },
            _ => return msg,
        }
        let mut cloned = (*msg).clone();
        if self.scale_msg(&mut cloned, current) {
            Arc::new(cloned)
        } else {
            msg
        }
    }

    // Map absolute peer coordinates back onto the real display.
    pub fn on_mouse_event(&self, e: &mut MouseEvent, current: usize) {
        if !self.enabled {
            return;
        }
        let evt_type = e.mask & crate::input::MOUSE_TYPE_MASK;
        if evt_type == crate::input::MOUSE_TYPE_WHEEL
            || evt_type == crate::input::MOUSE_TYPE_TRACKPAD
            || evt_type == crate::input::MOUSE_TYPE_MOVE_RELATIVE
        {
            return;
        }
        let Some((d, r)) = self.rect_of(current) else {
            return;
        };
        let sx = r.crop_w as f64 / r.out_w as f64;
        let sy = r.crop_h as f64 / r.out_h as f64;
        e.x = d.x + r.crop_x as i32 + ((e.x - d.x) as f64 * sx).round() as i32;
        e.y = d.y + r.crop_y as i32 + ((e.y - d.y) as f64 * sy).round() as i32;
    }
}

impl Drop for MobileScale {
    fn drop(&mut self) {
        if self.enabled {
            MOBILE_CONNS.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_rects() {
        let r = compute(3840, 2160);
        assert_eq!((r.crop_x, r.crop_y, r.crop_w, r.crop_h), (0, 0, 3840, 2160));
        assert_eq!((r.out_w, r.out_h), (2880, 1620));
        let r = compute(3440, 1440);
        assert_eq!((r.crop_x, r.crop_y, r.crop_w, r.crop_h), (440, 0, 2560, 1440));
        assert_eq!((r.out_w, r.out_h), (1920, 1080));
        let r = compute(1080, 1920);
        assert_eq!((r.crop_w, r.crop_h), (1080, 608));
    }
}
