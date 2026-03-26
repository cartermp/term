/// Platform-specific window setup.
///
/// On macOS this wires up NSVisualEffectView (vibrancy/blur) and a custom
/// presentation layer that renders pixels with proper per-pixel alpha.
///
/// Softbuffer's macOS backend uses `CGImageAlphaInfo::NoneSkipFirst`, which
/// means the alpha byte we write is silently ignored — every pixel ends up
/// fully opaque.  We bypass softbuffer's `present()` and instead create our
/// own `CGImage` with `CGImageAlphaInfo::First` (non-premultiplied straight
/// alpha) so that alpha=0 terminal-background pixels become genuinely
/// transparent and let the NSVisualEffectView blur show through beneath them.

#[cfg(target_os = "macos")]
mod cg_sys {
    use std::os::raw::c_void;

    // Opaque Core Graphics types
    pub enum CGColorSpaceOpaque {}
    pub type CGColorSpaceRef = *mut CGColorSpaceOpaque;

    pub enum CGDataProviderOpaque {}
    pub type CGDataProviderRef = *mut CGDataProviderOpaque;

    pub enum CGImageOpaque {}
    pub type CGImageRef = *mut CGImageOpaque;

    // CGImageAlphaInfo::First (4) — non-premultiplied, alpha in the most-significant
    // byte of a 32-bit little-endian word (i.e. byte[3] of [B,G,R,A] in memory).
    // CGBitmapByteOrder32Little (2 << 12 = 0x2000) — little-endian 32-bit pixels.
    //
    // Combined: our u32 layout 0xAA_RR_GG_BB = alpha=AA, R=RR, G=GG, B=BB. ✓
    pub const BITMAP_INFO_ALPHA_FIRST_32LE: u32 = 4 | (2 << 12);

    // kCGRenderingIntentDefault = 0
    pub const RENDERING_INTENT_DEFAULT: i32 = 0;

    pub type ReleaseDataCallback =
        unsafe extern "C" fn(*mut c_void, *const c_void, usize);

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGColorSpaceCreateDeviceRGB() -> CGColorSpaceRef;
        pub fn CGColorSpaceRelease(cs: CGColorSpaceRef);
        pub fn CGDataProviderCreateWithData(
            info: *mut c_void,
            data: *const c_void,
            size: usize,
            release_data: Option<ReleaseDataCallback>,
        ) -> CGDataProviderRef;
        pub fn CGDataProviderRelease(provider: CGDataProviderRef);
        pub fn CGImageCreate(
            width: usize,
            height: usize,
            bits_per_component: usize,
            bits_per_pixel: usize,
            bytes_per_row: usize,
            color_space: CGColorSpaceRef,
            bitmap_info: u32,
            provider: CGDataProviderRef,
            decode: *const f64,
            should_interpolate: bool,
            intent: i32,
        ) -> CGImageRef;
        pub fn CGImageRelease(image: CGImageRef);
    }

    /// Release callback invoked by CGDataProvider when it's done with the buffer.
    /// The `data` pointer is the raw `Box<[u32]>` we leaked into the provider.
    pub unsafe extern "C" fn release_pixels(
        _info: *mut c_void,
        data: *const c_void,
        size: usize,
    ) {
        unsafe {
            let count = size / 4;
            let ptr = data as *mut u32;
            drop(Box::from_raw(std::slice::from_raw_parts_mut(ptr, count)));
        }
    }
}

/// A `PresentLayer` owns a `CALayer` and a `CGColorSpace` and is responsible
/// for uploading rendered `u32` framebuffer data to the screen with correct
/// per-pixel alpha transparency.
#[cfg(target_os = "macos")]
pub struct PresentLayer {
    /// The CALayer we manage (a sublayer of the view's root layer).
    layer: *mut objc2::runtime::AnyObject,
    color_space: cg_sys::CGColorSpaceRef,
}

// CALayer usage is confined to the main thread via winit's guarantee.
#[cfg(target_os = "macos")]
unsafe impl Send for PresentLayer {}

#[cfg(target_os = "macos")]
impl PresentLayer {
    /// Create the layer, add it to the view's layer hierarchy, and initialise
    /// the color space.  Must be called after softbuffer has already called
    /// `setWantsLayer:YES` on the view (so `[view layer]` is non-null).
    pub fn new(window: &winit::window::Window) -> Option<Self> {
        use objc2::{class, msg_send, runtime::AnyObject};
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let ns_view = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::AppKit(h) => h.ns_view.as_ptr() as *mut AnyObject,
            _ => return None,
        };

        // Window size in points for the initial layer frame.
        let scale = window.scale_factor();
        let phys = window.inner_size();
        let w = phys.width as f64 / scale;
        let h = phys.height as f64 / scale;

        unsafe {
            // The view should already be layer-backed (softbuffer enables this).
            let root_layer: *mut AnyObject = msg_send![ns_view, layer];
            if root_layer.is_null() {
                return None;
            }

            // Make the root layer non-opaque so transparent pixels propagate.
            let _: () = msg_send![root_layer, setOpaque: false];

            // Create our custom layer.
            let layer: *mut AnyObject = msg_send![class!(CALayer), new];

            // Place it at the top-left, mirroring softbuffer's geometry.
            let anchor: [f64; 2] = [0.0, 0.0];
            let _: () = msg_send![layer, setAnchorPoint: anchor];
            let _: () = msg_send![layer, setGeometryFlipped: true];

            // Non-opaque so alpha=0 pixels in our CGImage are transparent.
            let _: () = msg_send![layer, setOpaque: false];

            // kCAGravityTopLeft — contents are not scaled.
            let gravity: *mut AnyObject = msg_send![
                class!(NSString),
                stringWithUTF8String: b"topLeft\0".as_ptr() as *const std::os::raw::c_char
            ];
            let _: () = msg_send![layer, setContentsGravity: gravity];

            // Match the display's HiDPI scale factor so pixels map 1:1.
            let _: () = msg_send![layer, setContentsScale: scale];

            // Set an initial frame so the layer is visible immediately.
            let bounds: [f64; 4] = [0.0, 0.0, w, h];
            let _: () = msg_send![layer, setFrame: bounds];

            // Fill the window — autoresizing mask kCALayerWidthSizable(2)|HeightSizable(4).
            let _: () = msg_send![layer, setAutoresizingMask: 6usize];

            // Add on top of softbuffer's sublayer (last added = topmost).
            let _: () = msg_send![root_layer, addSublayer: layer];

            let color_space = cg_sys::CGColorSpaceCreateDeviceRGB();

            Some(PresentLayer { layer, color_space })
        }
    }

    /// Upload `pixels` (width × height u32 values in 0xAARRGGBB layout with
    /// straight alpha) to the CALayer as a CGImage.  Alpha=0 pixels will be
    /// transparent, letting the NSVisualEffectView below show through.
    pub fn present(&mut self, pixels: &[u32], width: u32, height: u32) {
        use objc2::{class, msg_send};
        use std::os::raw::c_void;

        let n = (width as usize) * (height as usize);
        if n == 0 || pixels.len() < n {
            return;
        }

        unsafe {
            // Copy the slice into a heap-allocated Box whose ownership is
            // transferred to the CGDataProvider via the release callback.
            let boxed: Box<[u32]> = pixels[..n].to_vec().into_boxed_slice();
            let data_ptr = boxed.as_ptr() as *const c_void;
            let byte_len = n * 4;
            let _ = Box::into_raw(boxed); // ownership now with release_pixels callback

            let provider = cg_sys::CGDataProviderCreateWithData(
                std::ptr::null_mut(),
                data_ptr,
                byte_len,
                Some(cg_sys::release_pixels),
            );
            if provider.is_null() {
                // If the provider allocation failed the callback won't fire, so
                // we must free manually.  Reconstruct and drop the Box.
                drop(Box::from_raw(std::slice::from_raw_parts_mut(
                    data_ptr as *mut u32,
                    n,
                )));
                return;
            }

            let image = cg_sys::CGImageCreate(
                width as usize,
                height as usize,
                8,                        // bits per component
                32,                       // bits per pixel
                (width as usize) * 4,     // bytes per row
                self.color_space,
                cg_sys::BITMAP_INFO_ALPHA_FIRST_32LE,
                provider,
                std::ptr::null(),
                false,
                cg_sys::RENDERING_INTENT_DEFAULT,
            );

            // The image retains the provider; we can release our reference.
            cg_sys::CGDataProviderRelease(provider);

            if image.is_null() {
                return;
            }

            // Disable implicit CALayer animations for the contents swap.
            let _: () = msg_send![class!(CATransaction), begin];
            let _: () = msg_send![class!(CATransaction), setDisableActions: true];
            let _: () = msg_send![self.layer, setContents: image as *mut objc2::runtime::AnyObject];
            let _: () = msg_send![class!(CATransaction), commit];

            // CALayer retained the image when setContents: was called; release ours.
            cg_sys::CGImageRelease(image);
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for PresentLayer {
    fn drop(&mut self) {
        unsafe { cg_sys::CGColorSpaceRelease(self.color_space) };
    }
}

/// Set up window vibrancy: insert an NSVisualEffectView as a sibling behind
/// the winit view so blurred desktop content shows through alpha=0 pixels.
///
/// We deliberately do NOT call `setContentView:` because that fires
/// `viewDidMoveToWindow:nil` on WinitView which clears winit's internal state
/// and causes a `panic_cannot_unwind` in subsequent mouse events.
#[cfg(target_os = "macos")]
pub fn setup_vibrancy(window: &winit::window::Window) {
    use objc2::{class, msg_send, runtime::AnyObject};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let ns_view = match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::AppKit(h) => h.ns_view.as_ptr() as *mut AnyObject,
        _ => return,
    };

    let scale = window.scale_factor();
    let phys = window.inner_size();
    let w = phys.width as f64 / scale;
    let h = phys.height as f64 / scale;

    unsafe {
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        if ns_window.is_null() {
            return;
        }

        // Non-opaque window with a clear background so alpha=0 pixels reveal
        // the desktop through the NSVisualEffectView blur.
        let _: () = msg_send![ns_window, setOpaque: false];
        let clear: *mut AnyObject = msg_send![class!(NSColor), clearColor];
        let _: () = msg_send![ns_window, setBackgroundColor: clear];

        // ns_view's superview inside NSWindow is NSThemeFrame (the internal
        // frame view that also parents the title bar).
        let frame_view: *mut AnyObject = msg_send![ns_view, superview];
        if frame_view.is_null() {
            return;
        }

        let vev: *mut AnyObject = msg_send![class!(NSVisualEffectView), new];

        // NSRect as [f64; 4] = [origin.x, origin.y, size.width, size.height].
        // Same memory layout as NSRect; [f64; 4] implements Encode.
        let rect: [f64; 4] = [0.0, 0.0, w, h];
        let _: () = msg_send![vev, setFrame: rect];

        // NSViewWidthSizable(2) | NSViewHeightSizable(16) = 18
        let _: () = msg_send![vev, setAutoresizingMask: 18usize];
        // NSVisualEffectMaterial.underPageBackground = 21
        let _: () = msg_send![vev, setMaterial: 21usize];
        // NSVisualEffectBlendingMode.behindWindow = 0
        let _: () = msg_send![vev, setBlendingMode: 0usize];
        // NSVisualEffectState.active = 1
        let _: () = msg_send![vev, setState: 1usize];

        // addSubview:positioned:relativeTo: with NSWindowBelow(-1) inserts VEV
        // directly beneath ns_view in the frame_view subview stack.
        let _: () = msg_send![
            frame_view,
            addSubview: vev
            positioned: -1i64
            relativeTo: ns_view
        ];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn setup_vibrancy(_window: &winit::window::Window) {}

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
