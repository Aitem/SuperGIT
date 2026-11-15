//! Offscreen screenshots of the real window (feature `screenshot`).
//!
//! `SUPERGIT_SCREENSHOT=/tmp/ui.png [SUPERGIT_SCREENSHOT_DELAY=4] supergit <repo>`
//! waits for data to load, renders the current frame through Metal to a PNG
//! and quits. No screen-recording permission needed.

use std::time::Duration;

use gpui_kit::*;

pub fn schedule(handle: AnyWindowHandle, cx: &mut App) {
    let Ok(out) = std::env::var("SUPERGIT_SCREENSHOT") else {
        return;
    };
    let delay = std::env::var("SUPERGIT_SCREENSHOT_DELAY")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(4.0);
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_secs_f64(delay))
            .await;
        // An unfocused window may not have drawn since the data arrived, and
        // images (avatars) only start loading when first drawn: draw once,
        // give them a moment, then capture.
        let _ = cx.update_window(handle, |_, window, cx| {
            window.draw(cx).clear(cx);
        });
        cx.background_executor()
            .timer(Duration::from_millis(1500))
            .await;
        let image = cx.update_window(handle, |_, window, cx| {
            // macOS can stop presenting an unfocused window. Capture fresh
            // entity state instead of the last frame presented on screen.
            window.draw(cx).clear(cx);
            window.render_to_image()
        });
        match image {
            Ok(Ok(image)) => match image.save(&out) {
                Ok(()) => eprintln!("screenshot saved: {out}"),
                Err(e) => eprintln!("screenshot: cannot save {out}: {e}"),
            },
            Ok(Err(e)) | Err(e) => eprintln!("screenshot failed: {e:#}"),
        }
        cx.update(|cx| cx.quit());
    })
    .detach();
}
