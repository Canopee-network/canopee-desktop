use objc2::ffi::NSUInteger;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::{
    NSBezierPath, NSBitmapImageFileType, NSBitmapImageRep, NSColor, NSImage,
};
use objc2_foundation::{NSData, NSDictionary, NSPoint, NSRect, NSString, NSSize};
use std::ptr::NonNull;
use trayicon::Icon;

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconState {
    Running = 0,
    Stopped = 1,
    Error = 2,
}

#[derive(Clone, Copy)]
enum Badge {
    Dot,
    Square,
}

pub fn icons_for_states(base: &'static [u8]) -> [Icon; 3] {
    let plain = || {
        let mut icon = Icon::from_buffer(base, None, None).expect("failed to decode base icon");
        icon.set_template(true);
        icon
    };
    match (
        badge_png(base, Badge::Dot),
        badge_png(base, Badge::Square),
    ) {
        (Ok(running), Ok(error)) => [
            icon_from_bytes(running),
            plain(),
            icon_from_bytes(error),
        ],
        _ => [plain(), plain(), plain()],
    }
}

fn icon_from_bytes(png: Vec<u8>) -> Icon {
    let bytes: &'static [u8] = Box::leak(png.into_boxed_slice());
    let mut icon = Icon::from_buffer(bytes, None, None).expect("failed to decode badge icon");
    icon.set_template(true);
    icon
}

/// Where the status badge sits, as fractions of the icon's size.
///
/// These must be relative, not absolute points. An `NSImage` built from a
/// 24x24 PNG at 96 DPI reports a size of **18x18 points** (points =
/// pixels x 72 / dpi), so hard-coded point coordinates silently draw the
/// badge partly off the canvas and get clipped — which is exactly what
/// `badge_png_encodes_24x24_png` caught.
#[derive(Debug)]
struct BadgeRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Anchors the badge to the bottom-right corner, inset from both edges.
fn badge_rect(icon: NSSize) -> BadgeRect {
    let side = icon.width.min(icon.height);
    let d = side * BADGE_FRACTION;
    let m = side * BADGE_MARGIN_FRACTION;
    BadgeRect {
        x: (icon.width - d - m).max(0.0),
        y: m,
        w: d,
        h: d,
    }
}

const BADGE_FRACTION: f64 = 0.36;
const BADGE_MARGIN_FRACTION: f64 = 0.08;

#[allow(deprecated)]
fn badge_png(base: &[u8], shape: Badge) -> Result<Vec<u8>, String> {
    let data =
        unsafe { NSData::dataWithBytes_length(base.as_ptr().cast(), base.len() as NSUInteger) };
    let alloc: Allocated<NSImage> = unsafe { msg_send![class!(NSImage), alloc] };
    let image: Retained<NSImage> = NSImage::initWithData(alloc, &data)
        .ok_or_else(|| "invalid base icon".to_string())?;

    let icon_size = image.size();
    if icon_size.width <= 0.0 || icon_size.height <= 0.0 {
        return Err("base icon has no drawable size".to_string());
    }
    let r = badge_rect(icon_size);

    let color = match shape {
        Badge::Dot => NSColor::systemGreenColor(),
        Badge::Square => NSColor::systemRedColor(),
    };
    image.lockFocus();
    color.setFill();
    let rect = NSRect::new(NSPoint::new(r.x, r.y), NSSize::new(r.w, r.h));
    match shape {
        Badge::Dot => NSBezierPath::bezierPathWithOvalInRect(rect).fill(),
        Badge::Square => NSBezierPath::fillRect(rect),
    }
    image.unlockFocus();

    let tiff = image
        .TIFFRepresentation()
        .ok_or_else(|| "TIFFRepresentation failed".to_string())?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)
        .ok_or_else(|| "imageRepWithData failed".to_string())?;
    let properties = NSDictionary::<NSString, AnyObject>::new();
    let png = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) }
        .ok_or_else(|| "PNG encoding failed".to_string())?;

    let mut out = vec![0u8; png.length() as usize];
    unsafe { png.getBytes_length(NonNull::from(&mut out[0]).cast(), out.len() as NSUInteger) };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> &'static [u8] {
        Box::leak(include_bytes!("./icons/constellation_24_b.png").to_vec().into_boxed_slice())
    }

    fn png_dimensions(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        (
            u32::from_be_bytes(png[16..20].try_into().unwrap()),
            u32::from_be_bytes(png[20..24].try_into().unwrap()),
        )
    }

    #[test]
    fn badge_png_encodes_the_icon_canvas() {
        for shape in [Badge::Dot, Badge::Square] {
            let png = badge_png(base(), shape).expect("badge ok");
            let (w, h) = png_dimensions(&png);
            assert_eq!(w, h, "badge should stay square");
            // The encoded size is the NSImage size in points, which is not the
            // PNG's pixel count (a 24px/96dpi PNG is 18pt). Assert it's a real
            // canvas rather than a hard-coded 24.
            assert!(w >= 16, "badge canvas too small to draw into, got {w}");
        }
    }

    #[test]
    fn badge_rect_stays_inside_the_icon() {
        // Regression guard for the clipped-badge bug: absolute point
        // coordinates overflowed the 18pt canvas of a 24px/96dpi icon.
        for size in [NSSize::new(18.0, 18.0), NSSize::new(24.0, 24.0), NSSize::new(48.0, 48.0)] {
            let r = badge_rect(size);
            assert!(r.x >= 0.0, "badge starts off-canvas at {size:?}: {r:?}");
            assert!(r.y >= 0.0, "badge starts off-canvas at {size:?}: {r:?}");
            assert!(
                r.x + r.w <= size.width + f64::EPSILON,
                "badge overflows the right edge at {size:?}: {r:?}"
            );
            assert!(
                r.y + r.h <= size.height + f64::EPSILON,
                "badge overflows the top edge at {size:?}: {r:?}"
            );
            assert!(r.w > 0.0 && r.h > 0.0, "badge has no area at {size:?}");
        }
    }

    #[test]
    fn badge_scales_with_the_icon() {
        let small = badge_rect(NSSize::new(18.0, 18.0));
        let large = badge_rect(NSSize::new(72.0, 72.0));
        assert!(
            large.w > small.w,
            "badge should scale with the icon, got {small:?} vs {large:?}"
        );
    }

    #[test]
    fn state_icons_are_distinct() {
        let icons = icons_for_states(base());
        assert_ne!(icons[0], icons[1]);
        assert_ne!(icons[2], icons[1]);
        assert_ne!(icons[0], icons[2]);
    }
}