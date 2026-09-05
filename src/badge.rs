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
        badge_png(base, Badge::Dot, 15.0, 2.0, 7.0, 7.0),
        badge_png(base, Badge::Square, 15.0, 2.0, 7.0, 7.0),
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

#[allow(deprecated)]
fn badge_png(base: &[u8], shape: Badge, x: f64, y: f64, w: f64, h: f64) -> Result<Vec<u8>, String> {
    let data =
        unsafe { NSData::dataWithBytes_length(base.as_ptr().cast(), base.len() as NSUInteger) };
    let alloc: Allocated<NSImage> = unsafe { msg_send![class!(NSImage), alloc] };
    let image: Retained<NSImage> = NSImage::initWithData(alloc, &data)
        .ok_or_else(|| "invalid base icon".to_string())?;

    let color = match shape {
        Badge::Dot => NSColor::systemGreenColor(),
        Badge::Square => NSColor::systemRedColor(),
    };
    image.lockFocus();
    color.setFill();
    let rect = NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
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

    #[test]
    fn badge_png_encodes_24x24_png() {
        for shape in [Badge::Dot, Badge::Square] {
            let png = badge_png(base(), shape, 15.0, 2.0, 7.0, 7.0).expect("badge ok");
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            let w = u32::from_be_bytes(png[16..20].try_into().unwrap());
            let h = u32::from_be_bytes(png[20..24].try_into().unwrap());
            assert_eq!(w, h, "badge should stay square");
            assert!(w >= 24, "badge should at least cover the source pixels, got {w}");
        }
    }

    #[test]
    fn state_icons_are_distinct() {
        let icons = icons_for_states(base());
        assert_ne!(icons[0], icons[1]);
        assert_ne!(icons[2], icons[1]);
        assert_ne!(icons[0], icons[2]);
    }
}