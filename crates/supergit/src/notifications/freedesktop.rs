//! Freedesktop notifications (`org.freedesktop.Notifications` over D-Bus):
//! KDE, GNOME, dunst, mako… Clicking a notification (its default action)
//! opens the conversation, like on macOS. One notification per
//! conversation: a newer one replaces the previous, the way macOS groups
//! them by thread.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::App;
use notify_rust::{Hint, Notification, Timeout};

use super::Target;

static ACTIVATIONS: OnceLock<UnboundedSender<Target>> = OnceLock::new();
/// Conversation thread → the id of its notification on screen.
static SHOWN: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();

pub fn init(_cx: &mut App) -> UnboundedReceiver<Target> {
    let (sender, receiver) = unbounded();
    let _ = ACTIVATIONS.set(sender);
    receiver
}

/// Freedesktop servers need no authorization; check that one is running.
pub fn request_permission(_cx: &App) -> anyhow::Result<()> {
    notify_rust::get_server_information()
        .map(drop)
        .map_err(|e| anyhow::anyhow!("No notification server on the session bus: {e}"))
}

pub fn deliver(target: &Target, _revision: u64, title: &str, body: &str, _cx: &App) {
    let thread = format!(
        "{}:{}:{}",
        target.repository.display(),
        target.provider,
        target.conversation
    );
    let target = target.clone();
    let (title, body) = (title.to_owned(), body.to_owned());
    // Showing blocks on D-Bus, and waiting for the click blocks until the
    // notification closes: both stay off the UI thread.
    let spawned = std::thread::Builder::new()
        .name("notification".into())
        .spawn(move || show(thread, target, &title, &body));
    if let Err(e) = spawned {
        eprintln!("SuperGIT: cannot show a notification: {e}");
    }
}

fn show(thread: String, target: Target, title: &str, body: &str) {
    let shown = SHOWN.get_or_init(Default::default);
    let previous = shown.lock().ok().and_then(|map| map.get(&thread).copied());
    let mut notification = Notification::new();
    notification
        .appname("SuperGIT")
        .summary(title)
        .body(body)
        .icon("supergit")
        .hint(Hint::DesktopEntry("supergit".into()))
        .action("default", "Open")
        .timeout(Timeout::Default);
    if let Some(id) = previous {
        notification.id(id);
    }
    let handle = match notification.show() {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("SuperGIT: notification failed: {e}");
            return;
        }
    };
    let id = handle.id();
    if let Ok(mut map) = shown.lock() {
        map.insert(thread.clone(), id);
    }
    handle.wait_for_action(|action| {
        if action == "default"
            && let Some(sender) = ACTIVATIONS.get()
        {
            let _ = sender.unbounded_send(target);
        }
    });
    if let Ok(mut map) = shown.lock()
        && map.get(&thread) == Some(&id)
    {
        map.remove(&thread);
    }
}
