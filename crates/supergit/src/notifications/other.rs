//! No-op backend for platforms without UserNotifications: nothing is shown
//! and activations never arrive.

use futures::channel::mpsc::{UnboundedReceiver, unbounded};
use gpui_kit::App;

use super::Target;

pub fn init(_cx: &mut App) -> UnboundedReceiver<Target> {
    let (sender, receiver) = unbounded();
    // Keep the channel open so the activation loop simply waits.
    std::mem::forget(sender);
    receiver
}

pub fn request_permission(_cx: &App) -> anyhow::Result<()> {
    anyhow::bail!("Notifications are not supported on this platform")
}

pub fn deliver(_target: &Target, _revision: u64, _title: &str, _body: &str, _cx: &App) {}
