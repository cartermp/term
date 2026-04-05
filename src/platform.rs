/// Platform-specific helpers.

// ── New-tab callback (drives the native + button) ─────────────────────────────

use std::sync::OnceLock;

static NEW_TAB_CB: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Register the function to call when the native tab bar "+" button is clicked.
/// Must be called before `setup_add_tab_button`.  Can only be set once.
pub fn set_new_tab_callback(f: impl Fn() + Send + Sync + 'static) {
    let _ = NEW_TAB_CB.set(Box::new(f));
}

// ObjC method: handles the `newWindowForTab:` action sent by the native "+" button.
extern "C" fn new_tab_action(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _sender: *mut objc2::runtime::AnyObject,
) {
    if let Some(cb) = NEW_TAB_CB.get() {
        cb();
    }
}

fn tab_target_class() -> Option<&'static objc2::runtime::AnyClass> {
    use objc2::runtime::{AnyClass, AnyObject, ClassBuilder};
    use objc2::sel;
    static CLASS: OnceLock<Option<&'static AnyClass>> = OnceLock::new();
    *CLASS.get_or_init(|| {
        // Subclass NSResponder so we can be inserted into the responder chain.
        let superclass = objc2::class!(NSResponder);
        match ClassBuilder::new("TermTabTarget", superclass) {
            Some(mut builder) => {
                unsafe {
                    builder.add_method(
                        sel!(newWindowForTab:),
                        new_tab_action
                            as extern "C" fn(*mut AnyObject, objc2::runtime::Sel, *mut AnyObject),
                    );
                }
                Some(builder.register())
            }
            None => AnyClass::get("TermTabTarget"),
        }
    })
}

/// Insert a TermTabTarget NSResponder into this window's responder chain so that
/// the native tab bar "+" button's `newWindowForTab:` action reaches our callback.
/// Called once per window after the window is fully set up.
#[cfg(target_os = "macos")]
pub fn setup_add_tab_button(ns_view: *mut std::ffi::c_void) {
    use objc2::{msg_send, runtime::AnyObject};
    let cls = match tab_target_class() {
        Some(c) => c,
        None => return,
    };
    unsafe {
        let view = ns_view as *mut AnyObject;
        let win: *mut AnyObject = msg_send![view, window];
        if win.is_null() {
            return;
        }
        // Allocate a fresh NSResponder for this window and retain it permanently.
        let obj: *mut AnyObject = msg_send![cls, alloc];
        let obj: *mut AnyObject = msg_send![obj, init];
        if obj.is_null() {
            return;
        }
        let _: *mut AnyObject = msg_send![obj, retain];
        // Splice into the chain: win → obj → old_next_responder.
        let old_next: *mut AnyObject = msg_send![win, nextResponder];
        let _: () = msg_send![obj, setNextResponder: old_next];
        let _: () = msg_send![win, setNextResponder: obj];
    }
}

// ── Clipboard ──────────────────────────────────────────────────────────────────

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

/// Add `new_win` as a tab in the same tabbed-window group as `existing_win`.
/// Both pointers must be NSView* obtained from winit's raw window handle.
/// Safe to call immediately after window creation.
#[cfg(target_os = "macos")]
pub fn add_window_as_tab(existing_ns_view: *mut std::ffi::c_void, new_ns_view: *mut std::ffi::c_void) {
    use objc2::{msg_send, runtime::AnyObject};
    unsafe {
        let ev = existing_ns_view as *mut AnyObject;
        let nv = new_ns_view  as *mut AnyObject;
        let ew: *mut AnyObject = msg_send![ev, window];
        let nw: *mut AnyObject = msg_send![nv, window];
        if !ew.is_null() && !nw.is_null() {
            // NSWindowOrderingMode::NSWindowAbove = 1
            let _: () = msg_send![ew, addTabbedWindow: nw ordered: 1i64];
        }
    }
}

/// Ask macOS to select the next tab in the window's tab group.
#[cfg(target_os = "macos")]
pub fn select_next_tab(ns_view: *mut std::ffi::c_void) {
    use objc2::{msg_send, runtime::AnyObject};
    unsafe {
        let view: *mut AnyObject = ns_view as *mut AnyObject;
        let win:  *mut AnyObject = msg_send![view, window];
        if !win.is_null() {
            let _: () = msg_send![win, selectNextTab: std::ptr::null::<AnyObject>()];
        }
    }
}

/// Ask macOS to select the previous tab in the window's tab group.
#[cfg(target_os = "macos")]
pub fn select_prev_tab(ns_view: *mut std::ffi::c_void) {
    use objc2::{msg_send, runtime::AnyObject};
    unsafe {
        let view: *mut AnyObject = ns_view as *mut AnyObject;
        let win:  *mut AnyObject = msg_send![view, window];
        if !win.is_null() {
            let _: () = msg_send![win, selectPreviousTab: std::ptr::null::<AnyObject>()];
        }
    }
}

/// Update the native macOS tab label for this window.
/// `NSWindowTab.title` is separate from `NSWindow.title` and doesn't
/// automatically follow it after the window is added to a tab group.
#[cfg(target_os = "macos")]
pub fn set_tab_title(ns_view: *mut std::ffi::c_void, title: &str) {
    use objc2::{msg_send, runtime::AnyObject};
    use std::ffi::CString;
    let Ok(c_title) = CString::new(title) else { return };
    unsafe {
        let view: *mut AnyObject = ns_view as *mut AnyObject;
        let win:  *mut AnyObject = msg_send![view, window];
        if win.is_null() { return; }
        let tab: *mut AnyObject = msg_send![win, tab];
        if tab.is_null() { return; }
        let ns_str: *mut AnyObject = msg_send![
            objc2::class!(NSString),
            stringWithUTF8String: c_title.as_ptr()
        ];
        if ns_str.is_null() { return; }
        let _: () = msg_send![tab, setTitle: ns_str];
    }
}
