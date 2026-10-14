//! A picture of part of the window, taken by the webview itself so it is exactly what is on screen (scroll positions,
//! canvases, the current theme), copied to the clipboard as PNG.

#[cfg(not(target_os = "macos"))]
use culprit_core::ErrorCode;
use culprit_core::{Error, Result};
use serde::Deserialize;

/// Part of the window in CSS pixels, from its top left corner.
#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Snapshot `area` at the screen's resolution and put it on the clipboard.
#[cfg(target_os = "macos")]
pub async fn copy(webview: &tauri::Webview, area: Area) -> Result<()> {
    use std::sync::Mutex;

    use block2::RcBlock;
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSImage;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_foundation::NSError;
    use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();
    webview
        .with_webview(move |platform| {
            // WebKit hands the picture back later, on the main thread, through a block that may be called only once.
            let reply = Mutex::new(Some(tx));
            let send = move |result: Result<()>| {
                if let Some(tx) = reply.lock().ok().and_then(|mut tx| tx.take()) {
                    let _ = tx.send(result);
                }
            };
            let Some(mtm) = MainThreadMarker::new() else {
                return send(Err(Error::internal("The screenshot wasn't taken on the main thread")));
            };
            // SAFETY: on macOS the platform webview is the window's WKWebView, alive for this call.
            let view: &WKWebView = unsafe { &*platform.inner().cast() };
            let config = unsafe { WKSnapshotConfiguration::new(mtm) };
            unsafe {
                config.setRect(CGRect::new(CGPoint::new(area.x, area.y), CGSize::new(area.width, area.height)));
                // The UI hides the screenshot button (and any toast) first; wait for that frame.
                config.setAfterScreenUpdates(true);
            }
            let done = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                // SAFETY: WebKit passes either a valid image or a valid error, each null otherwise.
                let result = match (unsafe { image.as_ref() }, unsafe { error.as_ref() }) {
                    (Some(image), _) => macos::png(image).and_then(|png| macos::copy(&png)),
                    (None, Some(error)) => {
                        Err(Error::internal(format!("Couldn't take the screenshot: {}", error.localizedDescription())))
                    }
                    (None, None) => Err(Error::internal("Couldn't take the screenshot")),
                };
                send(result);
            });
            unsafe { view.takeSnapshotWithConfiguration_completionHandler(Some(&config), &done) };
        })
        .map_err(|e| Error::internal(format!("Couldn't reach the window: {e}")))?;
    rx.await.map_err(|_| Error::internal("The window closed before the screenshot was taken"))?
}

#[cfg(target_os = "macos")]
mod macos {
    use culprit_core::{Error, Result};
    use objc2::rc::Retained;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSPasteboard, NSPasteboardTypePNG};
    use objc2_foundation::{NSData, NSDictionary};

    /// The image as PNG at its full pixel size (twice the CSS size on a Retina screen).
    pub fn png(image: &NSImage) -> Result<Retained<NSData>> {
        let failed = || Error::internal("Couldn't turn the screenshot into a PNG");
        let tiff = image.TIFFRepresentation().ok_or_else(failed)?;
        let bitmap = NSBitmapImageRep::imageRepWithData(&tiff).ok_or_else(failed)?;
        unsafe { bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }
            .ok_or_else(failed)
    }

    /// Replace the clipboard with the PNG, as the system's own screenshots do.
    pub fn copy(png: &NSData) -> Result<()> {
        let board = NSPasteboard::generalPasteboard();
        board.clearContents();
        if board.setData_forType(Some(png), unsafe { NSPasteboardTypePNG }) {
            Ok(())
        } else {
            Err(Error::internal("The clipboard didn't take the screenshot; try again"))
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub async fn copy(_webview: &tauri::Webview, _area: Area) -> Result<()> {
    Err(Error::new(ErrorCode::Unsupported, "Screenshots of the window are only on macOS for now"))
}
