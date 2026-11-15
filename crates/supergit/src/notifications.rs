//! macOS notifications carry an exact provider-scoped conversation destination.

use std::path::PathBuf;
use std::sync::OnceLock;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::{App, Global};
use objc2::{ClassType, define_class, msg_send, rc::Retained, runtime::ProtocolObject};
use objc2_foundation::{NSBundle, NSObject, NSObjectProtocol, NSString};
use objc2_user_notifications::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub provider: String,
    pub conversation: String,
}

static ACTIVATIONS: OnceLock<UnboundedSender<Target>> = OnceLock::new();

define_class!(
    // SAFETY: NSObject has no subclassing requirements. The delegate owns no
    // main-thread-only UI state; callbacks only enqueue a Rust value.
    #[unsafe(super(NSObject))]
    #[name = "SuperGitNotificationDelegate"]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn activated(
            &self,
            _: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &block2::DynBlock<dyn Fn()>,
        ) {
            // SAFETY: UserNotifications exports this immutable NSString constant.
            let default_action = unsafe { UNNotificationDefaultActionIdentifier };
            if response.actionIdentifier().to_string() != default_action.to_string() {
                completion.call(());
                return;
            }
            let identifier = response.notification().request().identifier().to_string();
            if let Ok((target, _)) = serde_json::from_str::<(Target, u64)>(&identifier)
                && let Some(sender) = ACTIVATIONS.get()
            {
                let _ = sender.unbounded_send(target);
            }
            completion.call(());
        }

        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn present(
            &self,
            _: &UNUserNotificationCenter,
            _: &UNNotification,
            completion: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            // The app suppresses the currently visible conversation itself.
            completion
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }
    }
);

struct Service {
    center: Option<Retained<UNUserNotificationCenter>>,
    _delegate: Option<Retained<Delegate>>,
}
impl Global for Service {}

/// Register early in the launch callback. A raw CLI binary has no notification
/// identity; it must remain usable without invoking the bundle-only API.
pub fn init(cx: &mut App) -> UnboundedReceiver<Target> {
    let (sender, receiver) = unbounded();
    let _ = ACTIVATIONS.set(sender);
    let (center, delegate) = if NSBundle::mainBundle().bundleIdentifier().is_some() {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        // SAFETY: this NSObject subclass has no additional initialization state.
        let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::class(), new] };
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        (Some(center), Some(delegate))
    } else {
        (None, None)
    };
    cx.set_global(Service {
        center,
        _delegate: delegate,
    });
    receiver
}

pub fn request_permission(cx: &App) -> anyhow::Result<()> {
    let center = cx
        .global::<Service>()
        .center
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Notifications require the bundled SuperGIT.app"))?;
    let callback = block2::RcBlock::new(
        |granted: objc2::runtime::Bool, error: *mut objc2_foundation::NSError| {
            if !granted.as_bool() || !error.is_null() {
                eprintln!(
                    "SuperGIT notification authorization was not granted; check macOS notification settings"
                );
            }
        },
    );
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert,
        &callback,
    );
    Ok(())
}

pub fn deliver(target: &Target, revision: u64, title: &str, body: &str, cx: &App) {
    let Some(center) = &cx.global::<Service>().center else {
        return;
    };
    let Ok(identifier) = serde_json::to_string(&(target, revision)) else {
        return;
    };
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    content.setThreadIdentifier(&NSString::from_str(&format!(
        "{}:{}:{}",
        target.repository.display(),
        target.provider,
        target.conversation
    )));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&identifier),
        &content,
        None,
    );
    center.addNotificationRequest_withCompletionHandler(&request, None);
}

#[cfg(test)]
mod tests {
    use super::Target;

    #[test]
    fn activation_keeps_repository_worktree_provider_and_conversation() {
        let target = Target {
            repository: "/work/repo".into(),
            worktree: "/work/任务".into(),
            provider: "claude".into(),
            conversation: "draft:1".into(),
        };
        let id = serde_json::to_string(&(target, 7_u64)).unwrap();
        let (restored, revision): (Target, u64) = serde_json::from_str(&id).unwrap();
        assert_eq!(restored.worktree, PathBuf::from("/work/任务"));
        assert_eq!(restored.provider, "claude");
        assert_eq!(restored.conversation, "draft:1");
        assert_eq!(revision, 7);
    }
    use std::path::PathBuf;
}
