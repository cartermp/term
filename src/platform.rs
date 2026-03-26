/// Platform-specific helpers.

/// Read the macOS clipboard and return PNG-encoded image bytes, or `None` if
/// the clipboard holds no image.  Screenshots are stored as TIFF internally;
/// we convert them to PNG via NSBitmapImageRep so callers always get PNG.
#[cfg(target_os = "macos")]
pub fn clipboard_png() -> Option<Vec<u8>> {
    use objc2::{class, msg_send, runtime::AnyObject};
    use std::os::raw::c_char;
    unsafe {
        let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];

        // Try PNG directly.
        let png_type: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: b"public.png\0".as_ptr() as *const c_char
        ];
        let data: *mut AnyObject = msg_send![pb, dataForType: png_type];
        if !data.is_null() {
            let len: usize = msg_send![data, length];
            if len > 0 {
                let bytes: *const u8 = msg_send![data, bytes];
                return Some(std::slice::from_raw_parts(bytes, len).to_vec());
            }
        }

        // Try TIFF (screenshots, most copy-as-image operations on macOS).
        let tiff_type: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: b"public.tiff\0".as_ptr() as *const c_char
        ];
        let tiff_data: *mut AnyObject = msg_send![pb, dataForType: tiff_type];
        if tiff_data.is_null() {
            return None;
        }

        // NSImage → TIFF representation → NSBitmapImageRep → PNG data.
        let img: *mut AnyObject = msg_send![class!(NSImage), alloc];
        let img: *mut AnyObject = msg_send![img, initWithData: tiff_data];
        if img.is_null() {
            return None;
        }
        let tiff_rep: *mut AnyObject = msg_send![img, TIFFRepresentation];
        let _: () = msg_send![img, release];
        if tiff_rep.is_null() {
            return None;
        }

        let bmp: *mut AnyObject = msg_send![class!(NSBitmapImageRep), alloc];
        let bmp: *mut AnyObject = msg_send![bmp, initWithData: tiff_rep];
        if bmp.is_null() {
            return None;
        }

        // NSBitmapImageFileTypePNG = 4, empty properties dict.
        let props: *mut AnyObject = msg_send![class!(NSDictionary), dictionary];
        let png_data: *mut AnyObject =
            msg_send![bmp, representationUsingType: 4usize properties: props];
        let _: () = msg_send![bmp, release];
        if png_data.is_null() {
            return None;
        }

        let len: usize = msg_send![png_data, length];
        if len == 0 {
            return None;
        }
        let bytes: *const u8 = msg_send![png_data, bytes];
        Some(std::slice::from_raw_parts(bytes, len).to_vec())
    }
}

/// Read plain-text content from the macOS clipboard, or `None` if empty.
#[cfg(target_os = "macos")]
pub fn clipboard_text() -> Option<String> {
    use objc2::{class, msg_send, runtime::AnyObject};
    use std::os::raw::c_char;
    unsafe {
        let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
        let str_type: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: b"public.utf8-plain-text\0".as_ptr() as *const c_char
        ];
        let ns_str: *mut AnyObject = msg_send![pb, stringForType: str_type];
        if ns_str.is_null() {
            return None;
        }
        let c_str: *const c_char = msg_send![ns_str, UTF8String];
        if c_str.is_null() {
            return None;
        }
        Some(std::ffi::CStr::from_ptr(c_str).to_string_lossy().into_owned())
    }
}
