/// Platform-specific helpers.
// ── Native callbacks + menu actions ────────────────────────────────────────────
use crate::config::{BackgroundAppearance, Color};
use std::sync::OnceLock;

static NEW_TAB_CB: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
static SHOW_BACKGROUND_PANEL_CB: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
static CHECK_FOR_UPDATES_CB: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
static BACKGROUND_CHANGED_CB: OnceLock<Box<dyn Fn(BackgroundAppearance) + Send + Sync>> =
    OnceLock::new();

/// Register the function to call when the native tab bar "+" button is clicked.
/// Must be called before `setup_add_tab_button`. Can only be set once.
pub fn set_new_tab_callback(f: impl Fn() + Send + Sync + 'static) {
    let _ = NEW_TAB_CB.set(Box::new(f));
}

pub fn set_show_background_panel_callback(f: impl Fn() + Send + Sync + 'static) {
    let _ = SHOW_BACKGROUND_PANEL_CB.set(Box::new(f));
}

pub fn set_check_for_updates_callback(f: impl Fn() + Send + Sync + 'static) {
    let _ = CHECK_FOR_UPDATES_CB.set(Box::new(f));
}

pub fn set_background_changed_callback(f: impl Fn(BackgroundAppearance) + Send + Sync + 'static) {
    let _ = BACKGROUND_CHANGED_CB.set(Box::new(f));
}

extern "C" fn new_tab_action(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _sender: *mut objc2::runtime::AnyObject,
) {
    if let Some(cb) = NEW_TAB_CB.get() {
        cb();
    }
}

extern "C" fn show_background_panel_action(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _sender: *mut objc2::runtime::AnyObject,
) {
    if let Some(cb) = SHOW_BACKGROUND_PANEL_CB.get() {
        cb();
    }
}

extern "C" fn check_for_updates_action(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _sender: *mut objc2::runtime::AnyObject,
) {
    if let Some(cb) = CHECK_FOR_UPDATES_CB.get() {
        cb();
    }
}

extern "C" fn background_changed_action(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    sender: *mut objc2::runtime::AnyObject,
) {
    if let (Some(cb), Some(background)) = (BACKGROUND_CHANGED_CB.get(), panel_background(sender)) {
        cb(background);
    }
}

fn action_target_class() -> Option<&'static objc2::runtime::AnyClass> {
    use objc2::runtime::{AnyClass, AnyObject, ClassBuilder};
    use objc2::sel;
    static CLASS: OnceLock<Option<&'static AnyClass>> = OnceLock::new();
    *CLASS.get_or_init(|| {
        let superclass = objc2::class!(NSResponder);
        match ClassBuilder::new("TermActionTarget", superclass) {
            Some(mut builder) => {
                unsafe {
                    builder.add_method(
                        sel!(newWindowForTab:),
                        new_tab_action
                            as extern "C" fn(*mut AnyObject, objc2::runtime::Sel, *mut AnyObject),
                    );
                    builder.add_method(
                        sel!(openBackgroundAppearancePanel:),
                        show_background_panel_action
                            as extern "C" fn(*mut AnyObject, objc2::runtime::Sel, *mut AnyObject),
                    );
                    builder.add_method(
                        sel!(checkForUpdates:),
                        check_for_updates_action
                            as extern "C" fn(*mut AnyObject, objc2::runtime::Sel, *mut AnyObject),
                    );
                    builder.add_method(
                        sel!(changeColor:),
                        background_changed_action
                            as extern "C" fn(*mut AnyObject, objc2::runtime::Sel, *mut AnyObject),
                    );
                }
                Some(builder.register())
            }
            None => AnyClass::get("TermActionTarget"),
        }
    })
}

#[cfg(target_os = "macos")]
fn make_action_target() -> Option<*mut objc2::runtime::AnyObject> {
    use objc2::{msg_send, runtime::AnyObject};
    let cls = action_target_class()?;
    unsafe {
        let obj: *mut AnyObject = msg_send![cls, alloc];
        let obj: *mut AnyObject = msg_send![obj, init];
        if obj.is_null() {
            return None;
        }
        Some(obj)
    }
}

#[cfg(target_os = "macos")]
fn shared_action_target() -> Option<*mut objc2::runtime::AnyObject> {
    static TARGET: OnceLock<Option<usize>> = OnceLock::new();
    TARGET
        .get_or_init(|| make_action_target().map(|obj| obj as usize))
        .map(|ptr| ptr as *mut objc2::runtime::AnyObject)
}

#[cfg(target_os = "macos")]
fn ns_string(text: &str) -> Option<*mut objc2::runtime::AnyObject> {
    use objc2::{class, msg_send, runtime::AnyObject};
    use std::ffi::CString;
    let c_text = CString::new(text).ok()?;
    unsafe {
        let s: *mut AnyObject = msg_send![class!(NSString), stringWithUTF8String: c_text.as_ptr()];
        (!s.is_null()).then_some(s)
    }
}

#[cfg(target_os = "macos")]
fn ns_string_value(obj: *mut objc2::runtime::AnyObject) -> Option<String> {
    use objc2::{msg_send, runtime::AnyObject};
    use std::ffi::CStr;
    use std::os::raw::c_char;
    if obj.is_null() {
        return None;
    }
    unsafe {
        let s: *mut AnyObject = msg_send![obj, title];
        if s.is_null() {
            return None;
        }
        let c_str: *const c_char = msg_send![s, UTF8String];
        if c_str.is_null() {
            return None;
        }
        Some(CStr::from_ptr(c_str).to_string_lossy().into_owned())
    }
}

#[cfg(target_os = "macos")]
fn find_menu_item(
    menu: *mut objc2::runtime::AnyObject,
    title: &str,
) -> Option<*mut objc2::runtime::AnyObject> {
    use objc2::{msg_send, runtime::AnyObject};
    if menu.is_null() {
        return None;
    }
    unsafe {
        let count: usize = msg_send![menu, numberOfItems];
        for i in 0..count {
            let item: *mut AnyObject = msg_send![menu, itemAtIndex: i];
            if ns_string_value(item).as_deref() == Some(title) {
                return Some(item);
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn app_menu_submenu(
    main_menu: *mut objc2::runtime::AnyObject,
) -> Option<*mut objc2::runtime::AnyObject> {
    use objc2::{msg_send, runtime::AnyObject};
    if main_menu.is_null() {
        return None;
    }
    unsafe {
        let count: usize = msg_send![main_menu, numberOfItems];
        if count == 0 {
            return None;
        }
        let app_item: *mut AnyObject = msg_send![main_menu, itemAtIndex: 0usize];
        if app_item.is_null() {
            return None;
        }
        let submenu: *mut AnyObject = msg_send![app_item, submenu];
        (!submenu.is_null()).then_some(submenu)
    }
}

#[cfg(target_os = "macos")]
fn main_menu() -> Option<*mut objc2::runtime::AnyObject> {
    use objc2::{class, msg_send, runtime::AnyObject};
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return None;
        }
        let main_menu: *mut AnyObject = msg_send![app, mainMenu];
        (!main_menu.is_null()).then_some(main_menu)
    }
}

#[cfg(target_os = "macos")]
fn update_menu_item() -> Option<*mut objc2::runtime::AnyObject> {
    let submenu = app_menu_submenu(main_menu()?)?;
    find_menu_item(submenu, "Check for Updates...")
        .or_else(|| find_menu_item(submenu, "Checking for Updates..."))
}

#[cfg(target_os = "macos")]
fn panel_background(sender: *mut objc2::runtime::AnyObject) -> Option<BackgroundAppearance> {
    use objc2::{class, msg_send, runtime::AnyObject};
    unsafe {
        if sender.is_null() {
            return None;
        }
        let color: *mut AnyObject = msg_send![sender, color];
        if color.is_null() {
            return None;
        }
        let srgb_space: *mut AnyObject = msg_send![class!(NSColorSpace), sRGBColorSpace];
        let converted: *mut AnyObject = msg_send![color, colorUsingColorSpace: srgb_space];
        let color = if converted.is_null() {
            color
        } else {
            converted
        };
        let r: f64 = msg_send![color, redComponent];
        let g: f64 = msg_send![color, greenComponent];
        let b: f64 = msg_send![color, blueComponent];
        let a: f64 = msg_send![color, alphaComponent];
        Some(BackgroundAppearance::new(
            Color::new(
                (r.clamp(0.0, 1.0) * 255.0).round() as u8,
                (g.clamp(0.0, 1.0) * 255.0).round() as u8,
                (b.clamp(0.0, 1.0) * 255.0).round() as u8,
            ),
            (a.clamp(0.0, 1.0) * 255.0).round() as u8,
        ))
    }
}

/// Insert a TermActionTarget NSResponder into this window's responder chain so that
/// the native tab bar "+" button's `newWindowForTab:` action reaches our callback.
/// Called once per window after the window is fully set up.
#[cfg(target_os = "macos")]
static WINDOW_ACTION_TARGET_KEY: u8 = 0;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn objc_setAssociatedObject(
        object: *mut std::ffi::c_void,
        key: *const std::ffi::c_void,
        value: *mut std::ffi::c_void,
        policy: usize,
    );
}

#[cfg(target_os = "macos")]
pub fn setup_add_tab_button(ns_view: *mut std::ffi::c_void) {
    use objc2::{msg_send, runtime::AnyObject};
    // Retain the responder for exactly the NSWindow lifetime.
    const OBJC_ASSOCIATION_RETAIN_NONATOMIC: usize = 1;
    let Some(obj) = make_action_target() else {
        return;
    };
    unsafe {
        let view = ns_view as *mut AnyObject;
        let win: *mut AnyObject = msg_send![view, window];
        if win.is_null() {
            let _: () = msg_send![obj, release];
            return;
        }
        let old_next: *mut AnyObject = msg_send![win, nextResponder];
        let _: () = msg_send![obj, setNextResponder: old_next];
        let _: () = msg_send![win, setNextResponder: obj];
        objc_setAssociatedObject(
            win.cast(),
            std::ptr::addr_of!(WINDOW_ACTION_TARGET_KEY).cast(),
            obj.cast(),
            OBJC_ASSOCIATION_RETAIN_NONATOMIC,
        );
        let _: () = msg_send![obj, release];
    }
}

#[cfg(target_os = "macos")]
pub fn configure_window_background(ns_view: *mut std::ffi::c_void) {
    use objc2::{class, msg_send, runtime::AnyObject};
    unsafe {
        let view = ns_view as *mut AnyObject;
        let win: *mut AnyObject = msg_send![view, window];
        if win.is_null() {
            return;
        }
        let _: () = msg_send![win, setOpaque: false];
        let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
        if !clear.is_null() {
            let _: () = msg_send![win, setBackgroundColor: clear];
        }
    }
}

#[cfg(target_os = "macos")]
pub fn install_update_menu_item() {
    use objc2::{class, msg_send, runtime::AnyObject, sel};
    unsafe {
        let Some(main_menu) = main_menu() else {
            return;
        };
        let Some(submenu) = app_menu_submenu(main_menu) else {
            return;
        };
        if update_menu_item().is_some() {
            return;
        }

        let title = match ns_string("Check for Updates...") {
            Some(s) => s,
            None => return,
        };
        let empty = match ns_string("") {
            Some(s) => s,
            None => return,
        };
        let item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let item: *mut AnyObject = msg_send![
            item,
            initWithTitle: title
            action: sel!(checkForUpdates:)
            keyEquivalent: empty
        ];
        if item.is_null() {
            return;
        }
        let Some(target) = shared_action_target() else {
            let _: () = msg_send![item, release];
            return;
        };
        let _: () = msg_send![item, setTarget: target];
        let count: usize = msg_send![submenu, numberOfItems];
        let insert_at = count.min(1usize);
        let _: () = msg_send![submenu, insertItem: item atIndex: insert_at];
        let _: () = msg_send![item, release];
    }
}

#[cfg(target_os = "macos")]
pub fn install_background_menu_item() {
    use objc2::{class, msg_send, runtime::AnyObject, sel};
    unsafe {
        let Some(main_menu) = main_menu() else {
            return;
        };

        let appearance_title = match ns_string("Appearance") {
            Some(s) => s,
            None => return,
        };
        let background_title = match ns_string("Background...") {
            Some(s) => s,
            None => return,
        };
        let empty = match ns_string("") {
            Some(s) => s,
            None => return,
        };

        let appearance_item = if let Some(existing) = find_menu_item(main_menu, "Appearance") {
            existing
        } else {
            let item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
            let item: *mut AnyObject = msg_send![
                item,
                initWithTitle: appearance_title
                action: std::ptr::null::<std::ffi::c_void>()
                keyEquivalent: empty
            ];
            if item.is_null() {
                return;
            }
            let submenu: *mut AnyObject = msg_send![class!(NSMenu), alloc];
            let submenu: *mut AnyObject = msg_send![submenu, initWithTitle: appearance_title];
            if submenu.is_null() {
                let _: () = msg_send![item, release];
                return;
            }
            let _: () = msg_send![item, setSubmenu: submenu];
            let _: () = msg_send![main_menu, addItem: item];
            let _: () = msg_send![submenu, release];
            let _: () = msg_send![item, release];
            item
        };

        let submenu: *mut AnyObject = msg_send![appearance_item, submenu];
        if submenu.is_null() || find_menu_item(submenu, "Background...").is_some() {
            return;
        }

        let item: *mut AnyObject = msg_send![class!(NSMenuItem), alloc];
        let item: *mut AnyObject = msg_send![
            item,
            initWithTitle: background_title
            action: sel!(openBackgroundAppearancePanel:)
            keyEquivalent: empty
        ];
        if item.is_null() {
            return;
        }
        let Some(target) = shared_action_target() else {
            let _: () = msg_send![item, release];
            return;
        };
        let _: () = msg_send![item, setTarget: target];
        let _: () = msg_send![submenu, addItem: item];
        let _: () = msg_send![item, release];
    }
}

#[cfg(target_os = "macos")]
pub fn show_background_panel(background: BackgroundAppearance) {
    use objc2::{class, msg_send, runtime::AnyObject, sel};
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let panel: *mut AnyObject = msg_send![class!(NSColorPanel), sharedColorPanel];
        if panel.is_null() {
            return;
        }
        let Some(target) = shared_action_target() else {
            return;
        };
        let color: *mut AnyObject = msg_send![
            class!(NSColor),
            colorWithSRGBRed: background.color.r as f64 / 255.0
            green: background.color.g as f64 / 255.0
            blue: background.color.b as f64 / 255.0
            alpha: background.alpha as f64 / 255.0
        ];
        if !color.is_null() {
            let _: () = msg_send![panel, setColor: color];
        }
        let _: () = msg_send![panel, setShowsAlpha: true];
        let _: () = msg_send![panel, setTarget: target];
        let _: () = msg_send![panel, setAction: sel!(changeColor:)];
        let _: () = msg_send![panel, orderFront: std::ptr::null::<AnyObject>()];
        let _: () = msg_send![app, activateIgnoringOtherApps: true];
    }
}

#[cfg(target_os = "macos")]
pub fn set_update_menu_checking(checking: bool) {
    use objc2::msg_send;
    let Some(item) = update_menu_item() else {
        return;
    };
    let Some(title) = ns_string(if checking {
        "Checking for Updates..."
    } else {
        "Check for Updates..."
    }) else {
        return;
    };
    unsafe {
        let _: () = msg_send![item, setTitle: title];
        let _: () = msg_send![item, setEnabled: !checking];
    }
}

#[cfg(target_os = "macos")]
fn run_alert(title: &str, message: &str, second_button: Option<&str>) -> Option<i64> {
    use objc2::{class, msg_send, runtime::AnyObject};
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !app.is_null() {
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
        }

        let alert: *mut AnyObject = msg_send![class!(NSAlert), alloc];
        let alert: *mut AnyObject = msg_send![alert, init];
        if alert.is_null() {
            return None;
        }

        let result = (|| {
            let title = ns_string(title)?;
            let message = ns_string(message)?;
            let ok = ns_string("OK")?;
            let _: () = msg_send![alert, setMessageText: title];
            let _: () = msg_send![alert, setInformativeText: message];
            let _: *mut AnyObject = msg_send![alert, addButtonWithTitle: ok];

            if let Some(second_button) = second_button {
                let second = ns_string(second_button)?;
                let _: *mut AnyObject = msg_send![alert, addButtonWithTitle: second];
            }

            Some(msg_send![alert, runModal])
        })();
        let _: () = msg_send![alert, release];
        result
    }
}

#[cfg(target_os = "macos")]
pub fn show_info_alert(title: &str, message: &str) {
    let _ = run_alert(title, message, None);
}

#[cfg(target_os = "macos")]
pub fn show_error_alert(title: &str, message: &str) {
    let _ = run_alert(title, message, None);
}

#[cfg(target_os = "macos")]
pub fn confirm_update_install(current_version: &str, latest_version: &str) -> bool {
    let message = format!(
        "A newer Term release is available.\n\nCurrent: {current_version}\nLatest: {latest_version}\n\nTerm will download the update in the background, then quit and relaunch when it is ready."
    );
    run_alert("Update Available", &message, Some("Later")) == Some(1000)
}

// ── Clipboard ──────────────────────────────────────────────────────────────────

/// Read the macOS clipboard and return PNG-encoded image bytes, or `None` if
/// the clipboard holds no image.  Screenshots are stored as TIFF internally;
/// we convert them to PNG via NSBitmapImageRep so callers always get PNG.
#[cfg(target_os = "macos")]
pub fn clipboard_png() -> Option<Vec<u8>> {
    use objc2::{class, msg_send, runtime::AnyObject};
    unsafe {
        let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];

        // Try PNG directly.
        let png_type: *mut AnyObject = msg_send![
            class!(NSString),
            stringWithUTF8String: c"public.png".as_ptr()
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
            stringWithUTF8String: c"public.tiff".as_ptr()
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
            stringWithUTF8String: c"public.utf8-plain-text".as_ptr()
        ];
        let ns_str: *mut AnyObject = msg_send![pb, stringForType: str_type];
        if ns_str.is_null() {
            return None;
        }
        let c_str: *const c_char = msg_send![ns_str, UTF8String];
        if c_str.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(c_str)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Add `new_win` as a tab in the same tabbed-window group as `existing_win`.
/// Both pointers must be NSView* obtained from winit's raw window handle.
/// Safe to call immediately after window creation.
/// The new tab is always appended at the end of the tab bar.
#[cfg(target_os = "macos")]
pub fn add_window_as_tab(
    existing_ns_view: *mut std::ffi::c_void,
    new_ns_view: *mut std::ffi::c_void,
) {
    use objc2::{msg_send, runtime::AnyObject};
    unsafe {
        let ev = existing_ns_view as *mut AnyObject;
        let nv = new_ns_view as *mut AnyObject;
        let ew: *mut AnyObject = msg_send![ev, window];
        let nw: *mut AnyObject = msg_send![nv, window];
        if ew.is_null() || nw.is_null() {
            return;
        }

        // Find the last window in the tab group so we append after it,
        // rather than inserting after whichever window we happened to pick.
        let tabs: *mut AnyObject = msg_send![ew, tabbedWindows];
        let last_win: *mut AnyObject = if !tabs.is_null() {
            let count: usize = msg_send![tabs, count];
            if count > 0 {
                msg_send![tabs, objectAtIndex: count - 1]
            } else {
                ew
            }
        } else {
            ew
        };

        // NSWindowOrderingMode::NSWindowAbove = 1 → insert after `last_win`
        let _: () = msg_send![last_win, addTabbedWindow: nw ordered: 1i64];
    }
}

/// Ask macOS to select the next tab in the window's tab group.
#[cfg(target_os = "macos")]
pub fn select_next_tab(ns_view: *mut std::ffi::c_void) {
    use objc2::{msg_send, runtime::AnyObject};
    unsafe {
        let view: *mut AnyObject = ns_view as *mut AnyObject;
        let win: *mut AnyObject = msg_send![view, window];
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
        let win: *mut AnyObject = msg_send![view, window];
        if !win.is_null() {
            let _: () = msg_send![win, selectPreviousTab: std::ptr::null::<AnyObject>()];
        }
    }
}

/// Ask macOS to select tab at 1-based index `n` in the window's tab group.
/// If `n` is out of range, does nothing.
#[cfg(target_os = "macos")]
pub fn select_tab_at_index(ns_view: *mut std::ffi::c_void, n: usize) {
    use objc2::{msg_send, runtime::AnyObject};
    unsafe {
        let view: *mut AnyObject = ns_view as *mut AnyObject;
        let win: *mut AnyObject = msg_send![view, window];
        if win.is_null() {
            return;
        }
        let tabs: *mut AnyObject = msg_send![win, tabbedWindows];
        if tabs.is_null() {
            return;
        }
        let count: usize = msg_send![tabs, count];
        let idx = n.saturating_sub(1);
        if idx >= count {
            return;
        }
        let target: *mut AnyObject = msg_send![tabs, objectAtIndex: idx];
        if !target.is_null() {
            let _: () = msg_send![target, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        }
    }
}

/// Return the 1-based index of this window in its tab group, plus the total
/// number of tabs.  Returns `(1, 1)` when the window is not part of a group.
#[cfg(target_os = "macos")]
pub fn tab_index_and_count(ns_view: *mut std::ffi::c_void) -> (usize, usize) {
    use objc2::{msg_send, runtime::AnyObject};
    unsafe {
        let view: *mut AnyObject = ns_view as *mut AnyObject;
        let win: *mut AnyObject = msg_send![view, window];
        if win.is_null() {
            return (1, 1);
        }
        let tabs: *mut AnyObject = msg_send![win, tabbedWindows];
        if tabs.is_null() {
            return (1, 1);
        }
        let count: usize = msg_send![tabs, count];
        if count == 0 {
            return (1, 1);
        }
        for i in 0..count {
            let w: *mut AnyObject = msg_send![tabs, objectAtIndex: i];
            // Compare by pointer identity (both are NSWindow*).
            if std::ptr::eq(w, win) {
                return (i + 1, count);
            }
        }
        (1, count)
    }
}

/// Update the native macOS tab label for this window.
/// `NSWindowTab.title` is separate from `NSWindow.title` and doesn't
/// automatically follow it after the window is added to a tab group.
#[cfg(target_os = "macos")]
pub fn set_tab_title(ns_view: *mut std::ffi::c_void, title: &str) {
    use objc2::{msg_send, runtime::AnyObject};
    use std::ffi::CString;
    let Ok(c_title) = CString::new(title) else {
        return;
    };
    unsafe {
        let view: *mut AnyObject = ns_view as *mut AnyObject;
        let win: *mut AnyObject = msg_send![view, window];
        if win.is_null() {
            return;
        }
        let tab: *mut AnyObject = msg_send![win, tab];
        if tab.is_null() {
            return;
        }
        let ns_str: *mut AnyObject = msg_send![
            objc2::class!(NSString),
            stringWithUTF8String: c_title.as_ptr()
        ];
        if ns_str.is_null() {
            return;
        }
        let _: () = msg_send![tab, setTitle: ns_str];
    }
}
