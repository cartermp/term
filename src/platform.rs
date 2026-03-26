/// Platform-specific window setup.
///
/// On macOS this wires up NSVisualEffectView (vibrancy/blur) so that terminal
/// background pixels — which are rendered with alpha=0 — show the blurred
/// desktop through them.  Everything else (glyphs, tab bar, cursor) is
/// rendered with alpha=0xFF and appears fully opaque.
///
/// On other platforms this is a no-op.

#[cfg(target_os = "macos")]
pub fn setup_vibrancy(window: &winit::window::Window) {
    use objc2::{class, msg_send, runtime::AnyObject};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    // Get the NSView from winit's raw window handle
    let ns_view = match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::AppKit(h) => h.ns_view.as_ptr() as *mut AnyObject,
        _ => return,
    };

    // Window size in points (logical coordinates — NSRect uses points, not pixels)
    let scale = window.scale_factor();
    let phys = window.inner_size();
    let w = phys.width as f64 / scale;
    let h = phys.height as f64 / scale;

    unsafe {
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        if ns_window.is_null() {
            return;
        }

        // Make the window itself non-opaque with a clear background so
        // alpha=0 pixels show the desktop through them.
        let _: () = msg_send![ns_window, setOpaque: false];
        let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![ns_window, setBackgroundColor: clear];

        // Make the content view's CALayer non-opaque so its alpha=0 pixels
        // are actually transparent in compositing (not filled with black).
        let _: () = msg_send![ns_view, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![ns_view, layer];
        if !layer.is_null() {
            let _: () = msg_send![layer, setOpaque: false];
            let clear2: *mut AnyObject = msg_send![class!(NSColor), clearColor];
            let cg_color: *mut AnyObject = msg_send![clear2, CGColor];
            let _: () = msg_send![layer, setBackgroundColor: cg_color];
        }

        // ── Insert NSVisualEffectView as a sibling BEHIND ns_view ─────────────
        //
        // We deliberately do NOT call setContentView: because that would fire
        // viewDidMoveToWindow: nil on WinitView, clearing winit's internal state
        // and causing a panic in mouse_moved.
        //
        // Instead we insert VEV into ns_view's superview (NSThemeFrame) at a
        // lower z-position. When ns_view's layer renders alpha=0 pixels for
        // terminal background cells, those transparent holes reveal the VEV
        // blur beneath via Core Animation's standard alpha compositing.

        // ns_view's superview inside NSWindow is NSThemeFrame (the internal
        // window chrome view that also parents the title bar).
        let frame_view: *mut AnyObject = msg_send![ns_view, superview];
        if frame_view.is_null() {
            return;
        }

        let vev: *mut AnyObject = msg_send![class!(NSVisualEffectView), new];

        // NSRect as [f64; 4] = [origin.x, origin.y, size.width, size.height].
        // [f64; 4] has the same memory layout as NSRect and implements Encode,
        // so it satisfies msg_send!'s type requirements without needing CGRect.
        let rect: [f64; 4] = [0.0, 0.0, w, h];
        let _: () = msg_send![vev, setFrame: rect];

        // NSViewWidthSizable(2) | NSViewHeightSizable(16) = 18
        let _: () = msg_send![vev, setAutoresizingMask: 18usize];
        // NSVisualEffectMaterial.underPageBackground = 21 — dark adaptive blur
        let _: () = msg_send![vev, setMaterial: 21usize];
        // NSVisualEffectBlendingMode.behindWindow = 0
        let _: () = msg_send![vev, setBlendingMode: 0usize];
        // NSVisualEffectState.active = 1 — always-on blur
        let _: () = msg_send![vev, setState: 1usize];

        // addSubview:positioned:relativeTo: with NSWindowBelow(-1) places vev
        // directly beneath ns_view in the frame_view subview stack.
        let _: () = msg_send![frame_view, addSubview: vev positioned: -1i64 relativeTo: ns_view];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn setup_vibrancy(_window: &winit::window::Window) {}
