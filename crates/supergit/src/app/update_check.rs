//! Update checks in the window: the background schedule, SuperGIT ▸ Check
//! for Updates…, and the status bar button while a newer release is out. Its
//! menu installs the release over this copy, downloads it to Downloads or
//! opens its page; an installed update waits for "Restart to Update".

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::{Either, select};
use gpui_kit::Anchor;
use gpui_kit::component::menu::DropdownMenu as _;
use sg_github::Release;

use super::*;
use crate::updates::{self, install};

/// What happens to the available update.
pub(super) enum UpdateJob {
    /// Downloading `tag` (to install it, or into Downloads); `bytes` grows
    /// toward `total`.
    Running {
        install: bool,
        tag: String,
        bytes: Arc<AtomicU64>,
        total: u64,
    },
    /// `tag` replaced `target`; it runs after a restart.
    Installed { tag: String, target: PathBuf },
}

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
                let first = force;
                force = false;
                if this
                    .update(cx, |this, cx| {
                        this.set_update(latest, cx);
                        // Screenshot builds: `SUPERGIT_DEBUG_UPDATE_INSTALL=1`
                        // installs what the forced check found.
                        if cfg!(feature = "screenshot")
                            && first
                            && std::env::var_os("SUPERGIT_DEBUG_UPDATE_INSTALL").is_some()
                        {
                            this.start_update_job(true, cx);
                        }
                    })
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
                            "SuperGIT {} is available: the button in the status bar installs or downloads it",
                            release.tag
                        ),
                        None => format!("SuperGIT {} is up to date", updates::running_version()),
                    };
                    this.flash(message, false, cx);
                }
                Err(error) => this.flash(format!("Cannot check for updates: {error:#}"), true, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Install the available update over this copy, or save it in Downloads.
    /// The work runs on a thread of its own (a download can take minutes);
    /// the status bar shows its progress.
    fn start_update_job(&mut self, install: bool, cx: &mut Context<Self>) {
        let Some(release) = self.update.clone() else {
            return;
        };
        if matches!(self.update_job, Some(UpdateJob::Running { .. })) {
            return;
        }
        let destination = if install {
            install::install_target().map(Path::to_path_buf)
        } else {
            install::downloads_dir()
        };
        let Some(destination) = destination else {
            self.flash(
                if install {
                    "This copy of SuperGIT is built from source: pull and rebuild it instead"
                } else {
                    "No Downloads folder: HOME is not set"
                },
                true,
                cx,
            );
            return;
        };
        let tag = release.tag.clone();
        let bytes = Arc::new(AtomicU64::new(0));
        let (done_tx, mut done) = futures::channel::oneshot::channel();
        let job_bytes = bytes.clone();
        let job_destination = destination.clone();
        let spawned = std::thread::Builder::new()
            .name("supergit-update".into())
            .spawn(move || {
                let result = if install {
                    install::install(&release, &job_destination, &job_bytes)
                        .map(|()| job_destination)
                } else {
                    install::download(&release, &job_destination, &job_bytes)
                };
                let _ = done_tx.send(result);
            });
        if let Err(error) = spawned {
            self.flash(format!("Cannot start the update: {error}"), true, cx);
            return;
        }
        self.update_job = Some(UpdateJob::Running {
            install,
            tag: tag.clone(),
            bytes: bytes.clone(),
            total: self
                .update
                .as_ref()
                .and_then(install::package)
                .map_or(0, |asset| asset.size),
        });
        cx.notify();
        self.update_job_task = Some(cx.spawn(async move |this, cx| {
            // Repaint the status bar as the percentage changes.
            let mut shown = 0;
            let result = loop {
                let tick = cx.background_executor().timer(Duration::from_millis(250));
                match select(done, tick).await {
                    Either::Left((result, _)) => break result,
                    Either::Right((_, pending)) => {
                        done = pending;
                        let now = bytes.load(Ordering::Relaxed);
                        if now != shown {
                            shown = now;
                            if this.update(cx, |_, cx| cx.notify()).is_err() {
                                return;
                            }
                        }
                    }
                }
            };
            let result = result.unwrap_or_else(|_| Err(anyhow::anyhow!("the update stopped")));
            this.update(cx, |this, cx| {
                this.finish_update_job(install, tag, result, cx)
            })
            .ok();
        }));
    }

    fn finish_update_job(
        &mut self,
        install: bool,
        tag: String,
        result: anyhow::Result<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.update_job = None;
        self.update_job_task = None;
        match result {
            Ok(target) if install => {
                self.flash(
                    format!("SuperGIT {tag} is installed: restart SuperGIT to use it"),
                    false,
                    cx,
                );
                self.update_job = Some(UpdateJob::Installed { tag, target });
            }
            Ok(file) => {
                let folder = file
                    .parent()
                    .map_or_else(String::new, |dir| format!(" to {}", display_home(dir)));
                let name = file
                    .file_name()
                    .map_or_else(Default::default, |name| name.to_string_lossy().into_owned());
                self.flash(format!("Downloaded {name}{folder}"), false, cx);
                if let Err(error) = launch::reveal_file(&file) {
                    self.flash(format!("{error:#}"), true, cx);
                }
            }
            Err(error) => self.flash(
                format!(
                    "Cannot {} SuperGIT {tag}: {error:#}",
                    if install { "install" } else { "download" }
                ),
                true,
                cx,
            ),
        }
        cx.notify();
    }

    /// Quit and start the installed copy once this process is gone.
    fn restart_to_update(&mut self, target: &Path, cx: &mut Context<Self>) {
        match install::relaunch_after_exit(target) {
            Ok(()) => cx.quit(),
            Err(error) => self.flash(format!("{error:#}"), true, cx),
        }
    }

    /// The status bar button while a newer release is out: a menu (opening
    /// upward) to install it, download it or open its page; then progress;
    /// then a restart.
    pub(super) fn update_button(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let info = cx.theme().info;
        match &self.update_job {
            Some(UpdateJob::Installed { tag, target }) => {
                let target = target.clone();
                return Some(
                    Button::new("restart-to-update")
                        .label("Restart to Update")
                        .text_color(info)
                        .xsmall()
                        .ghost()
                        .tooltip(format!(
                            "SuperGIT {tag} is installed · Restart now: running agents and terminals stop, conversations can be resumed"
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.restart_to_update(&target, cx)
                        }))
                        .into_any_element(),
                );
            }
            Some(UpdateJob::Running {
                install,
                tag,
                bytes,
                total,
            }) => {
                let done = bytes.load(Ordering::Relaxed);
                let percent = if *total > 0 && done < *total {
                    format!(" {}%", done * 100 / total)
                } else {
                    String::new()
                };
                return Some(
                    Button::new("update-progress")
                        .label(format!(
                            "{} {tag}…{percent}",
                            if *install {
                                "Installing"
                            } else {
                                "Downloading"
                            }
                        ))
                        .xsmall()
                        .ghost()
                        .loading(true)
                        .into_any_element(),
                );
            }
            None => {}
        }
        let release = self.update.clone()?;
        let owner = cx.entity();
        let packaged = install::package(&release).is_some();
        let installable = packaged && install::install_target().is_some();
        Some(
            Button::new("update-available")
                .label(format!("{} available", release.tag))
                .text_color(info)
                .xsmall()
                .ghost()
                .dropdown_caret(true)
                .tooltip(format!(
                    "SuperGIT {} is out (this is {})",
                    release.tag,
                    updates::running_version()
                ))
                .dropdown_menu_with_anchor(Anchor::BottomRight, move |mut menu, _, _| {
                    let job = |install: bool| {
                        let owner = owner.clone();
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            owner.update(cx, |this, cx| this.start_update_job(install, cx));
                        }
                    };
                    if installable {
                        menu = menu.item(
                            PopupMenuItem::new(format!("Install {}", release.tag))
                                .on_click(job(true)),
                        );
                    } else if packaged {
                        menu = menu.label("Built from source: pull and rebuild to update");
                    }
                    if packaged {
                        let folder = install::downloads_dir()
                            .map_or_else(|| "Downloads".into(), |dir| display_home(&dir));
                        menu = menu.item(
                            PopupMenuItem::new(format!("Download to {folder}"))
                                .on_click(job(false)),
                        );
                    }
                    let url = release.url.clone();
                    let owner = owner.clone();
                    menu.separator()
                        .item(
                            PopupMenuItem::new("Open Release Page").on_click(move |_, _, cx| {
                                if let Err(error) = launch::open_url(&url) {
                                    owner.update(cx, |this, cx| {
                                        this.flash(format!("{error:#}"), true, cx)
                                    });
                                }
                            }),
                        )
                })
                .into_any_element(),
        )
    }
}

/// `/Users/me/Downloads` → `~/Downloads`.
fn display_home(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}
