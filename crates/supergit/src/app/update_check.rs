//! Update checks in the window: the background schedule, SuperGIT ▸ Check
//! for Updates…, and the title bar button while a newer release is out.

use sg_github::Release;

use super::*;
use crate::updates;

impl SuperGitApp {
    /// Check now when due (the stored release otherwise), then again on
    /// every `updates::TICK`; `updates` keeps checks 12 hours apart.
    pub(super) fn start_update_checks(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "screenshot")]
        let mut force = updates::debug_version().is_some();
        #[cfg(not(feature = "screenshot"))]
        let mut force = false;
        // Screenshot windows stay offline unless asked to show an update.
        if std::env::var_os("SUPERGIT_SCREENSHOT").is_some() && !force {
            return;
        }
        self.update_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let latest = cx
                    .background_executor()
                    .spawn(async move {
                        let state = updates::load();
                        if !force && !state.due(updates::now()) {
                            return state.latest;
                        }
                        match updates::check() {
                            Ok(latest) => Some(latest),
                            Err(error) => {
                                eprintln!("update check failed: {error:#}");
                                state.latest
                            }
                        }
                    })
                    .await;
                force = false;
                if this
                    .update(cx, |this, cx| this.set_update(latest, cx))
                    .is_err()
                {
                    return;
                }
                cx.background_executor().timer(updates::TICK).await;
            }
        }));
    }

    /// Offer `latest` when it is newer than this build.
    fn set_update(&mut self, latest: Option<Release>, cx: &mut Context<Self>) {
        let running = updates::running_version();
        let update = latest.filter(|release| updates::is_newer(release, &running));
        if self.update != update {
            self.update = update;
            cx.notify();
        }
    }

    /// SuperGIT ▸ Check for Updates…: checks right away, whenever the last
    /// check was, and says what it found.
    pub(super) fn check_for_updates(
        &mut self,
        _: &CheckForUpdates,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flash("Checking for updates…", false, cx);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { updates::check() })
                .await;
            this.update(cx, |this, cx| match result {
                Ok(latest) => {
                    this.set_update(Some(latest), cx);
                    let message = match &this.update {
                        Some(release) => format!(
                            "SuperGIT {} is available: the button in the title bar opens its release page",
                            release.tag
                        ),
                        None => format!(
                            "SuperGIT {} is up to date",
                            updates::running_version()
                        ),
                    };
                    this.flash(message, false, cx);
                }
                Err(error) => this.flash(format!("Cannot check for updates: {error:#}"), true, cx),
            })
            .ok();
        })
        .detach();
    }

    /// The title bar button while a newer release is out: opens its page.
    pub(super) fn update_button(&self, cx: &mut Context<Self>) -> Option<Button> {
        let release = self.update.clone()?;
        Some(
            Button::new("update-available")
                .label(format!("{} available", release.tag))
                .text_color(cx.theme().info)
                .small()
                .ghost()
                .tooltip(format!(
                    "SuperGIT {} is out (this is {}) · Open the release page",
                    release.tag,
                    updates::running_version()
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Err(error) = launch::open_url(&release.url) {
                        this.flash(format!("{error:#}"), true, cx);
                    }
                })),
        )
    }
}
