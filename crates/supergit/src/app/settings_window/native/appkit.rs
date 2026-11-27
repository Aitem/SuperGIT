//! The settings window drawn by AppKit: an `NSTabViewController` in the
//! toolbar style (the pages' symbols in the window's toolbar, as in Safari's
//! or Mail's settings), each page an `NSGridView` of right-aligned labels
//! and the system's controls: segmented controls, pop-up buttons,
//! checkboxes, text views. Built once and kept: closing hides it.
//!
//! Controls report through `Target` (their target, and the text view's and
//! the window's delegate) into the `Event` channel; `update` sets them from
//! a new `State`. All of it runs on the main thread.

use std::cell::RefCell;

use futures::channel::mpsc::UnboundedSender;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::*;
use objc2_foundation::*;

use super::super::Page;
use super::{Event, State};
use crate::settings::{AgentView, Appearance, DiffLayout};

/// The pages' width; their height follows what they hold.
const WIDTH: f64 = 620.;
/// Notes and text views, right of the labels.
const COLUMN: f64 = 400.;
const APPEARANCES: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];
const VIEWS: [AgentView; 2] = [AgentView::Chat, AgentView::Terminal];
const LAYOUTS: [DiffLayout; 2] = [DiffLayout::Unified, DiffLayout::Split];

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
}

struct Window {
    panel: Retained<Panel>,
    tabs: Retained<NSTabViewController>,
    /// The controls' target: they hold it weakly.
    _target: Retained<Target>,
    events: UnboundedSender<Event>,
    controls: Controls,
    state: Option<State>,
    /// The branches' checkboxes, by tag.
    branches: Vec<String>,
}

struct Controls {
    appearance: Retained<NSSegmentedControl>,
    version: Retained<NSTextField>,
    agent_view: Retained<NSSegmentedControl>,
    prompt: Retained<NSTextView>,
    prompt_default: Retained<NSButton>,
    prompt_none: Retained<NSButton>,
    preview_for: Retained<NSTextField>,
    preview: Retained<NSTextView>,
    sounds: [Retained<NSPopUpButton>; 2],
    banners: [Retained<NSButton>; 2],
    problem: Retained<NSTextField>,
    layout: Retained<NSSegmentedControl>,
    difftastic: Retained<NSButton>,
    difftastic_status: Retained<NSTextField>,
    repository: Retained<NSGridView>,
    no_repository: Retained<NSTextField>,
    repository_name: Retained<NSTextField>,
    auto_cleanup: Retained<NSButton>,
    branch_list: Retained<NSStackView>,
}

fn send(event: Event) {
    WINDOW.with_borrow(|window| {
        if let Some(window) = window {
            let _ = window.events.unbounded_send(event);
        }
    });
}

fn on(control_state: NSControlStateValue) -> bool {
    control_state == NSControlStateValueOn
}

fn state_value(on: bool) -> NSControlStateValue {
    if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. The target keeps no
    // state of its own: the controls live in `WINDOW`, events go to a
    // channel.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SuperGitSettingsTarget"]
    struct Target;

    impl Target {
        #[unsafe(method(appearanceChanged:))]
        fn appearance_changed(&self, sender: &NSSegmentedControl) {
            if let Some(&appearance) = usize::try_from(sender.selectedSegment())
                .ok()
                .and_then(|ix| APPEARANCES.get(ix))
            {
                send(Event::Appearance(appearance));
            }
        }

        #[unsafe(method(checkNow:))]
        fn check_now(&self, _sender: &NSButton) {
            send(Event::CheckForUpdates);
        }

        #[unsafe(method(agentViewChanged:))]
        fn agent_view_changed(&self, sender: &NSSegmentedControl) {
            if let Some(&view) = usize::try_from(sender.selectedSegment())
                .ok()
                .and_then(|ix| VIEWS.get(ix))
            {
                send(Event::AgentView(view));
            }
        }

        #[unsafe(method(promptDefault:))]
        fn prompt_default(&self, _sender: &NSButton) {
            set_prompt(crate::initial_prompt::DEFAULT);
        }

        #[unsafe(method(promptNone:))]
        fn prompt_none(&self, _sender: &NSButton) {
            set_prompt("");
        }

        #[unsafe(method(soundChanged:))]
        fn sound_changed(&self, sender: &NSPopUpButton) {
            let name = sender
                .selectedItem()
                .and_then(|item| usize::try_from(item.tag()).ok())
                .and_then(|ix| crate::sound::SOUNDS.get(ix))
                .map(|name| (*name).to_string());
            send(Event::Sound {
                needs_you: sender.tag() == 0,
                name,
            });
        }

        #[unsafe(method(bannerChanged:))]
        fn banner_changed(&self, sender: &NSButton) {
            send(Event::Banner {
                needs_you: sender.tag() == 0,
                on: on(sender.state()),
            });
        }

        #[unsafe(method(layoutChanged:))]
        fn layout_changed(&self, sender: &NSSegmentedControl) {
            if let Some(&layout) = usize::try_from(sender.selectedSegment())
                .ok()
                .and_then(|ix| LAYOUTS.get(ix))
            {
                send(Event::DiffLayout(layout));
            }
        }

        #[unsafe(method(difftasticChanged:))]
        fn difftastic_changed(&self, sender: &NSButton) {
            send(Event::Difftastic(on(sender.state())));
        }

        #[unsafe(method(autoCleanupChanged:))]
        fn auto_cleanup_changed(&self, sender: &NSButton) {
            send(Event::AutoCleanup(on(sender.state())));
        }

        #[unsafe(method(protectionChanged:))]
        fn protection_changed(&self, sender: &NSButton) {
            let branch = WINDOW.with_borrow(|window| {
                let window = window.as_ref()?;
                window
                    .branches
                    .get(usize::try_from(sender.tag()).ok()?)
                    .cloned()
            });
            if let Some(branch) = branch {
                send(Event::Protect {
                    branch,
                    protected: on(sender.state()),
                });
            }
        }
    }

    unsafe impl NSObjectProtocol for Target {}

    unsafe impl NSTextDelegate for Target {
        #[unsafe(method(textDidChange:))]
        fn text_did_change(&self, _notification: &NSNotification) {
            WINDOW.with_borrow(|window| {
                if let Some(window) = window {
                    show_preview(window);
                }
            });
        }

        #[unsafe(method(textDidEndEditing:))]
        fn text_did_end_editing(&self, _notification: &NSNotification) {
            save_prompt();
        }
    }

    unsafe impl NSTextViewDelegate for Target {}

    unsafe impl NSWindowDelegate for Target {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            save_prompt();
            send(Event::Closed);
        }
    }
);

define_class!(
    // SAFETY: NSWindow may be subclassed; this one only overrides two
    // responder methods and keeps no state.
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SuperGitSettingsPanel"]
    struct Panel;

    impl Panel {
        /// Esc closes the window, as ⌘W does.
        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _sender: Option<&AnyObject>) {
            self.performClose(None);
        }

        /// ⌘W, and the editing keys: the app's menus have no Edit menu to
        /// carry ⌘C, ⌘V and the rest to the text views.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            let handled = match key_action(event) {
                Some(action) if action == sel!(performClose:) => {
                    self.performClose(None);
                    true
                }
                Some(action) => {
                    let app = NSApplication::sharedApplication(self.mtm());
                    let sender: &AnyObject = self;
                    // SAFETY: the standard editing actions take their sender
                    // as their one argument; nil sends them to the first
                    // responder.
                    unsafe { app.sendAction_to_from(action, None, Some(sender)) }
                }
                None => false,
            };
            // SAFETY: NSWindow implements it, with this signature.
            handled || unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }
    }
);

define_class!(
    // SAFETY: NSView may be subclassed; this one only draws top-down, so a
    // scroll view shows its first rows first.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SuperGitFlippedView"]
    struct Flipped;

    impl Flipped {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

/// What a ⌘ key does in the window.
fn key_action(event: &NSEvent) -> Option<Sel> {
    let flags = event.modifierFlags();
    if !flags.contains(NSEventModifierFlags::Command) {
        return None;
    }
    let key = event
        .charactersIgnoringModifiers()?
        .to_string()
        .to_lowercase();
    let shift = flags.contains(NSEventModifierFlags::Shift);
    Some(match (key.as_str(), shift) {
        ("w", false) => sel!(performClose:),
        ("x", false) => sel!(cut:),
        ("c", false) => sel!(copy:),
        ("v", false) => sel!(paste:),
        ("a", false) => sel!(selectAll:),
        ("z", false) => sel!(undo:),
        ("z", true) => sel!(redo:),
        _ => return None,
    })
}

/// Show the window on `page`, building it the first time.
pub(super) fn show(state: &State, page: Option<Page>, focus: bool, events: UnboundedSender<Event>) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let created = WINDOW.with_borrow_mut(|slot| match slot {
        Some(window) => {
            window.events = events;
            false
        }
        None => {
            *slot = Some(build(events, mtm));
            true
        }
    });
    let Some((panel, tabs, prompt)) = WINDOW.with_borrow(|window| {
        window.as_ref().map(|window| {
            (
                window.panel.clone(),
                window.tabs.clone(),
                window.controls.prompt.clone(),
            )
        })
    }) else {
        return;
    };
    // A window that was closed shows the saved template again.
    if !panel.isVisible() {
        prompt.setString(&NSString::from_str(&state.initial_prompt));
    }
    update(state);
    if let Some(ix) = page.and_then(|page| Page::ALL.iter().position(|p| *p == page)) {
        tabs.setSelectedTabViewItemIndex(ix as NSInteger);
    }
    if created {
        panel.center();
    }
    if focus {
        panel.makeKeyAndOrderFront(None);
    } else {
        panel.orderFrontRegardless();
    }
}

/// Set the controls from `state`.
pub(super) fn update(state: &State) {
    WINDOW.with_borrow_mut(|window| {
        let Some(window) = window else {
            return;
        };
        let c = &window.controls;
        let appearance = match state.appearance {
            Appearance::System => None,
            Appearance::Light => {
                // SAFETY: AppKit exports the appearance names as constants.
                NSAppearance::appearanceNamed(unsafe { NSAppearanceNameAqua })
            }
            Appearance::Dark => {
                // SAFETY: as above.
                NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua })
            }
        };
        window.panel.setAppearance(appearance.as_deref());
        select(
            &c.appearance,
            APPEARANCES.iter().position(|a| *a == state.appearance),
        );
        c.version
            .setStringValue(&NSString::from_str(&format!("SuperGIT {}", state.version)));
        select(
            &c.agent_view,
            VIEWS.iter().position(|v| *v == state.agent_view),
        );
        for (ix, popup) in c.sounds.iter().enumerate() {
            let tag = state.sounds[ix]
                .as_deref()
                .and_then(|name| crate::sound::SOUNDS.iter().position(|s| *s == name))
                .map_or(-1, |ix| ix as NSInteger);
            popup.selectItemWithTag(tag);
            c.banners[ix].setState(state_value(state.banners[ix]));
        }
        c.problem
            .setStringValue(&NSString::from_str(state.problem.as_deref().unwrap_or("")));
        c.problem.setHidden(state.problem.is_none());
        select(
            &c.layout,
            LAYOUTS.iter().position(|l| *l == state.diff_layout),
        );
        c.difftastic.setState(state_value(state.difftastic));
        c.difftastic.setEnabled(state.difftastic_available);
        c.difftastic_status
            .setStringValue(&NSString::from_str(&state.difftastic_status));
        c.repository.setHidden(state.repository.is_none());
        c.no_repository.setHidden(state.repository.is_some());
        if let Some(repository) = &state.repository {
            c.repository_name
                .setStringValue(&NSString::from_str(&repository.name));
            c.auto_cleanup
                .setState(state_value(repository.auto_cleanup));
            let names: Vec<String> = repository
                .branches
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            if names != window.branches {
                for view in c.branch_list.arrangedSubviews().iter() {
                    view.removeFromSuperview();
                }
                let mtm = window.panel.mtm();
                let target: &AnyObject = &window._target;
                for (ix, name) in names.iter().enumerate() {
                    let checkbox = checkbox(name, sel!(protectionChanged:), target, mtm);
                    checkbox.setTag(ix as NSInteger);
                    c.branch_list.addArrangedSubview(&checkbox);
                }
                window.branches = names;
            }
            for (view, (_, protected)) in c
                .branch_list
                .arrangedSubviews()
                .iter()
                .zip(&repository.branches)
            {
                if let Ok(checkbox) = view.downcast::<NSButton>() {
                    checkbox.setState(state_value(*protected));
                }
            }
        }
        window.state = Some(state.clone());
        show_preview(window);
    });
}

/// The template's preview for the selected worktree, and which of Default
/// and None would change it.
fn show_preview(window: &Window) {
    let c = &window.controls;
    let text = c.prompt.string().to_string();
    c.prompt_default
        .setEnabled(text != crate::initial_prompt::DEFAULT);
    c.prompt_none.setEnabled(!text.is_empty());
    let place = window
        .state
        .as_ref()
        .and_then(|state| state.prompt_place.as_ref());
    let (title, preview) = match place {
        Some((name, place)) => {
            let prompt = crate::initial_prompt::render(&text, place);
            (
                format!("What an agent in {name} gets:"),
                if prompt.is_empty() {
                    "Nothing is added.".to_string()
                } else {
                    prompt
                },
            )
        }
        None => (
            "Select a worktree to see what its agents get.".to_string(),
            String::new(),
        ),
    };
    c.preview_for.setStringValue(&NSString::from_str(&title));
    c.preview.setString(&NSString::from_str(&preview));
}

fn set_prompt(text: &str) {
    WINDOW.with_borrow(|window| {
        if let Some(window) = window {
            window.controls.prompt.setString(&NSString::from_str(text));
            show_preview(window);
        }
    });
    send(Event::InitialPrompt(text.to_string()));
}

fn save_prompt() {
    let text = WINDOW.with_borrow(|window| {
        window
            .as_ref()
            .map(|window| window.controls.prompt.string().to_string())
    });
    if let Some(text) = text {
        send(Event::InitialPrompt(text));
    }
}

fn select(control: &NSSegmentedControl, ix: Option<usize>) {
    if let Some(ix) = ix {
        control.setSelectedSegment(ix as NSInteger);
    }
}

// ---- building ---------------------------------------------------------------

fn build(events: UnboundedSender<Event>, mtm: MainThreadMarker) -> Window {
    // SAFETY: an NSObject subclass without instance variables.
    let target: Retained<Target> = unsafe { msg_send![Target::alloc(mtm), init] };
    let t: &AnyObject = &target;

    // General.
    let appearance = segmented(
        &["System", "Light", "Dark"],
        sel!(appearanceChanged:),
        t,
        mtm,
    );
    let version = NSTextField::labelWithString(&NSString::from_str(""), mtm);
    let check = button("Check Now", sel!(checkNow:), t, mtm);
    let version_row = hstack(&[&version, &check], mtm);
    let general = grid(
        &[
            [&label("Appearance:", mtm), &appearance],
            [&label("Version:", mtm), &version_row],
            [
                &empty(mtm),
                &note(
                    "SuperGIT also checks at startup and then every 12 hours.",
                    mtm,
                ),
            ],
        ],
        mtm,
    );

    // Agents.
    let agent_view = segmented(&["Chat", "Terminal"], sel!(agentViewChanged:), t, mtm);
    let (prompt_scroll, prompt) = text_view(COLUMN, 180., true, mtm);
    prompt.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    let prompt_default = button("Default", sel!(promptDefault:), t, mtm);
    let prompt_none = button("None", sel!(promptNone:), t, mtm);
    let prompt_buttons = hstack(&[&prompt_default, &prompt_none], mtm);
    let preview_for = note("", mtm);
    let (preview_scroll, preview) = text_view(COLUMN, 110., false, mtm);
    let agents = grid(
        &[
            [&label("Open conversations as:", mtm), &agent_view],
            [
                &empty(mtm),
                &note(
                    "Terminal runs the agent's own TUI (claude, codex) in SuperGIT's colors; \
                     Chat is SuperGIT's own view.",
                    mtm,
                ),
            ],
            [&label("Initial prompt:", mtm), &prompt_scroll],
            [&empty(mtm), &prompt_buttons],
            [
                &empty(mtm),
                &note(
                    "Added to the system prompt of every agent SuperGIT starts: Claude's \
                     appended system prompt, Codex's developer instructions (new threads). \
                     Placeholders: {worktree}, {branch}, {repo}, {repo_name}. \
                     {#linked}…{/linked} shows only in a linked worktree, {#main}…{/main} \
                     only in the main checkout, {#folder}…{/folder} only in a folder \
                     without Git.",
                    mtm,
                ),
            ],
            [&label("Preview:", mtm), &preview_for],
            [&empty(mtm), &preview_scroll],
        ],
        mtm,
    );

    // Notifications.
    let sounds = [true, false].map(|needs_you| sound_popup(needs_you, t, mtm));
    let banners = [0, 1].map(|tag| {
        let banner = checkbox("Show a banner", sel!(bannerChanged:), t, mtm);
        banner.setTag(tag);
        banner
    });
    let problem = NSTextField::wrappingLabelWithString(&NSString::from_str(""), mtm);
    problem.setTextColor(Some(&NSColor::systemRedColor()));
    problem.setPreferredMaxLayoutWidth(COLUMN);
    let notifications = grid(
        &[
            [&label("Approvals and questions:", mtm), &sounds[0]],
            [&empty(mtm), &banners[0]],
            [&label("Completions and failures:", mtm), &sounds[1]],
            [&empty(mtm), &banners[1]],
            [
                &empty(mtm),
                &note(
                    "For conversations SuperGIT runs, chat or terminal; the one on screen \
                     stays quiet. Sounds play even in a Focus. If macOS blocks banners, \
                     enable SuperGIT in System Settings → Notifications.",
                    mtm,
                ),
            ],
            [&empty(mtm), &problem],
        ],
        mtm,
    );

    // Diffs.
    let layout = segmented(&["Unified", "Split"], sel!(layoutChanged:), t, mtm);
    let difftastic = checkbox(
        "Show diffs with difftastic",
        sel!(difftasticChanged:),
        t,
        mtm,
    );
    let difftastic_status = note("", mtm);
    let diffs = grid(
        &[
            [&label("Layout:", mtm), &layout],
            [&empty(mtm), &note("Also in the bar above each diff.", mtm)],
            [&label("Syntax:", mtm), &difftastic],
            [&empty(mtm), &difftastic_status],
        ],
        mtm,
    );

    // Repository.
    let repository_name = label("", mtm);
    let auto_cleanup = checkbox(
        "Clean up worktrees after their PR merges",
        sel!(autoCleanupChanged:),
        t,
        mtm,
    );
    let branch_list = NSStackView::new(mtm);
    branch_list.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    branch_list.setAlignment(NSLayoutAttribute::Leading);
    branch_list.setSpacing(6.);
    let branches = scrolling(&branch_list, COLUMN, 160., mtm);
    let repository = grid(
        &[
            [&label("Repository:", mtm), &repository_name],
            [&empty(mtm), &auto_cleanup],
            [
                &empty(mtm),
                &note(
                    "Only PRs seen open while SuperGIT ran qualify. Selected, dirty, busy, \
                     kept, protected or unread work stays. Squash and rebase merges need a \
                     manual review.",
                    mtm,
                ),
            ],
            [&label("Protected branches:", mtm), &branches],
            [
                &empty(mtm),
                &note(
                    "Cleanup never deletes the checked ones. The main worktree and the \
                     default base are always protected.",
                    mtm,
                ),
            ],
        ],
        mtm,
    );
    let no_repository = note("Open a repository to set up its worktree cleanup.", mtm);
    let repository_page = vstack(&[&repository, &no_repository], mtm);

    let tabs = NSTabViewController::new(mtm);
    tabs.setTabStyle(NSTabViewControllerTabStyle::Toolbar);
    let contents: [&NSView; 5] = [&general, &agents, &notifications, &diffs, &repository_page];
    for (page, content) in Page::ALL.into_iter().zip(contents) {
        let item = NSTabViewItem::tabViewItemWithViewController(&controller(page, content, mtm));
        item.setLabel(&NSString::from_str(page.label()));
        let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str(symbol(page)),
            Some(&NSString::from_str(page.label())),
        );
        item.setImage(image.as_deref());
        tabs.addTabViewItem(&item);
    }

    let style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(WIDTH, 400.));
    // SAFETY: NSWindow's designated initializer, on the main thread.
    let panel: Retained<Panel> = unsafe {
        msg_send![
            Panel::alloc(mtm),
            initWithContentRect: frame,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ]
    };
    // SAFETY: `Window` keeps the panel; closing only hides it.
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    panel.setToolbarStyle(NSWindowToolbarStyle::Preference);
    panel.setContentViewController(Some(&tabs));
    panel.setTitle(&NSString::from_str("Settings"));

    Window {
        panel,
        tabs,
        _target: target,
        events,
        controls: Controls {
            appearance,
            version,
            agent_view,
            prompt,
            prompt_default,
            prompt_none,
            preview_for,
            preview,
            sounds,
            banners,
            problem,
            layout,
            difftastic,
            difftastic_status,
            repository,
            no_repository,
            repository_name,
            auto_cleanup,
            branch_list,
        },
        state: None,
        branches: Vec::new(),
    }
}

/// SF Symbols for the toolbar.
fn symbol(page: Page) -> &'static str {
    match page {
        Page::General => "gearshape",
        Page::Agents => "sparkles",
        Page::Notifications => "bell.badge",
        Page::Diffs => "plus.forwardslash.minus",
        Page::Repository => "arrow.triangle.branch",
    }
}

/// A page: its grid centered, with margins; the window takes its size.
fn controller(page: Page, content: &NSView, mtm: MainThreadMarker) -> Retained<NSViewController> {
    let view = NSView::new(mtm);
    content.setTranslatesAutoresizingMaskIntoConstraints(false);
    view.addSubview(content);
    let constraints = [
        content
            .topAnchor()
            .constraintEqualToAnchor_constant(&view.topAnchor(), 24.),
        view.bottomAnchor()
            .constraintEqualToAnchor_constant(&content.bottomAnchor(), 24.),
        content
            .centerXAnchor()
            .constraintEqualToAnchor(&view.centerXAnchor()),
        content
            .leadingAnchor()
            .constraintGreaterThanOrEqualToAnchor_constant(&view.leadingAnchor(), 24.),
        view.widthAnchor().constraintEqualToConstant(WIDTH),
    ];
    NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&constraints));
    let controller = NSViewController::new(mtm);
    controller.setView(&view);
    controller.setTitle(Some(&NSString::from_str(page.label())));
    view.layoutSubtreeIfNeeded();
    controller.setPreferredContentSize(view.fittingSize());
    controller
}

/// Rows of a label (right-aligned) and its control.
fn grid(rows: &[[&NSView; 2]], mtm: MainThreadMarker) -> Retained<NSGridView> {
    let rows: Vec<Retained<NSArray<NSView>>> =
        rows.iter().map(|row| NSArray::from_slice(row)).collect();
    let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&rows), mtm);
    grid.setRowSpacing(10.);
    grid.setColumnSpacing(10.);
    grid.columnAtIndex(0)
        .setXPlacement(NSGridCellPlacement::Trailing);
    grid.setRowAlignment(NSGridRowAlignment::FirstBaseline);
    grid
}

fn empty(mtm: MainThreadMarker) -> Retained<NSView> {
    NSGridCell::emptyContentView(mtm)
}

fn label(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    NSTextField::labelWithString(&NSString::from_str(text), mtm)
}

/// A small gray note that wraps in the controls' column.
fn note(text: &str, mtm: MainThreadMarker) -> Retained<NSTextField> {
    let note = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    note.setFont(Some(&NSFont::systemFontOfSize(
        NSFont::smallSystemFontSize(),
    )));
    note.setTextColor(Some(&NSColor::secondaryLabelColor()));
    note.setPreferredMaxLayoutWidth(COLUMN);
    note
}

fn button(
    title: &str,
    action: Sel,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    // SAFETY: the target implements `action`, taking the sender.
    unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action),
            mtm,
        )
    }
}

fn checkbox(
    title: &str,
    action: Sel,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    // SAFETY: the target implements `action`, taking the sender.
    unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(action),
            mtm,
        )
    }
}

fn segmented(
    labels: &[&str],
    action: Sel,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSSegmentedControl> {
    let labels: Vec<Retained<NSString>> = labels.iter().map(|l| NSString::from_str(l)).collect();
    // SAFETY: the target implements `action`, taking the sender.
    unsafe {
        NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
            &NSArray::from_retained_slice(&labels),
            NSSegmentSwitchTracking::SelectOne,
            Some(target),
            Some(action),
            mtm,
        )
    }
}

/// None, then the system's sounds; items are tagged with their index in
/// `sound::SOUNDS` (None: -1), the button with 0 for approvals, 1 else.
fn sound_popup(
    needs_you: bool,
    target: &AnyObject,
    mtm: MainThreadMarker,
) -> Retained<NSPopUpButton> {
    let popup =
        NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), NSRect::ZERO, false);
    popup.addItemWithTitle(&NSString::from_str("None"));
    if let Some(item) = popup.lastItem() {
        item.setTag(-1);
    }
    if let Some(menu) = popup.menu() {
        menu.addItem(&NSMenuItem::separatorItem(mtm));
    }
    for (ix, &name) in crate::sound::SOUNDS.iter().enumerate() {
        let title = if name == crate::sound::default(needs_you) {
            format!("{} (default)", crate::sound::label(name))
        } else {
            crate::sound::label(name)
        };
        popup.addItemWithTitle(&NSString::from_str(&title));
        if let Some(item) = popup.lastItem() {
            item.setTag(ix as NSInteger);
        }
    }
    popup.setTag(if needs_you { 0 } else { 1 });
    // SAFETY: the target implements `soundChanged:`, taking the sender.
    unsafe {
        popup.setTarget(Some(target));
        popup.setAction(Some(sel!(soundChanged:)));
    }
    popup
}

/// A plain-text view in a bordered scroll view of this size.
fn text_view(
    width: f64,
    height: f64,
    editable: bool,
    mtm: MainThreadMarker,
) -> (Retained<NSScrollView>, Retained<NSTextView>) {
    let scroll = NSTextView::scrollableTextView(mtm);
    scroll.setBorderType(NSBorderType::BezelBorder);
    let view = scroll
        .documentView()
        .and_then(|view| view.downcast::<NSTextView>().ok())
        .expect("a scrollable text view holds a text view");
    view.setRichText(false);
    view.setEditable(editable);
    view.setAllowsUndo(editable);
    view.setAutomaticQuoteSubstitutionEnabled(false);
    view.setAutomaticDashSubstitutionEnabled(false);
    view.setContinuousSpellCheckingEnabled(false);
    view.setTextContainerInset(NSSize::new(4., 6.));
    let size = if editable {
        12.
    } else {
        NSFont::smallSystemFontSize()
    };
    view.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(size, 0.)));
    if !editable {
        view.setTextColor(Some(&NSColor::secondaryLabelColor()));
    }
    sized(&scroll, width, height);
    (scroll, view)
}

/// `content` in a bordered scroll view of this size, its rows from the top.
fn scrolling(
    content: &NSView,
    width: f64,
    height: f64,
    mtm: MainThreadMarker,
) -> Retained<NSScrollView> {
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), NSRect::ZERO);
    scroll.setHasVerticalScroller(true);
    scroll.setBorderType(NSBorderType::BezelBorder);
    // SAFETY: an NSView subclass without instance variables.
    let document: Retained<Flipped> =
        unsafe { msg_send![Flipped::alloc(mtm), initWithFrame: NSRect::ZERO] };
    document.setTranslatesAutoresizingMaskIntoConstraints(false);
    content.setTranslatesAutoresizingMaskIntoConstraints(false);
    document.addSubview(content);
    scroll.setDocumentView(Some(&document));
    let clip = scroll.contentView();
    let constraints = [
        content
            .topAnchor()
            .constraintEqualToAnchor_constant(&document.topAnchor(), 6.),
        content
            .leadingAnchor()
            .constraintEqualToAnchor_constant(&document.leadingAnchor(), 8.),
        document
            .bottomAnchor()
            .constraintEqualToAnchor_constant(&content.bottomAnchor(), 6.),
        document
            .widthAnchor()
            .constraintEqualToAnchor(&clip.widthAnchor()),
    ];
    NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&constraints));
    sized(&scroll, width, height);
    scroll
}

fn sized(view: &NSView, width: f64, height: f64) {
    view.setTranslatesAutoresizingMaskIntoConstraints(false);
    let constraints = [
        view.widthAnchor().constraintEqualToConstant(width),
        view.heightAnchor().constraintEqualToConstant(height),
    ];
    NSLayoutConstraint::activateConstraints(&NSArray::from_retained_slice(&constraints));
}

fn hstack(views: &[&NSView], mtm: MainThreadMarker) -> Retained<NSStackView> {
    let stack = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm);
    stack.setSpacing(8.);
    stack
}

fn vstack(views: &[&NSView], mtm: MainThreadMarker) -> Retained<NSStackView> {
    let stack = NSStackView::stackViewWithViews(&NSArray::from_slice(views), mtm);
    stack.setOrientation(NSUserInterfaceLayoutOrientation::Vertical);
    stack.setAlignment(NSLayoutAttribute::Leading);
    stack
}

/// Screenshot builds: the window, toolbar included, drawn to a PNG.
#[cfg(feature = "screenshot")]
pub(super) fn capture(path: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    WINDOW.with_borrow(|window| {
        let window = window.as_ref().context("the settings window is not open")?;
        let content = window
            .panel
            .contentView()
            .context("the settings window has no content")?;
        // The frame around the content holds the title bar and toolbar.
        // SAFETY: a window's content view is in its frame view.
        let view = unsafe { content.superview() }.unwrap_or(content);
        view.layoutSubtreeIfNeeded();
        let bounds = view.bounds();
        let image = view
            .bitmapImageRepForCachingDisplayInRect(bounds)
            .context("cannot make an image of the window")?;
        view.cacheDisplayInRect_toBitmapImageRep(bounds, &image);
        // SAFETY: PNG needs no properties.
        let png = unsafe {
            image.representationUsingType_properties(
                NSBitmapImageFileType::PNG,
                &NSDictionary::new(),
            )
        }
        .context("cannot encode the image")?;
        anyhow::ensure!(
            png.writeToFile_atomically(&NSString::from_str(path), true),
            "cannot write {path}"
        );
        Ok(())
    })
}

/// Screenshot builds, `SUPERGIT_DEBUG_SETTINGS_CLICK=1`: click the first
/// banner checkbox and the first branch's, as a hand would, to check the
/// way back to the app.
#[cfg(feature = "screenshot")]
pub(super) fn debug_click() {
    let buttons = WINDOW.with_borrow(|window| {
        let window = window.as_ref()?;
        let branch = window
            .controls
            .branch_list
            .arrangedSubviews()
            .firstObject()
            .and_then(|view| view.downcast::<NSButton>().ok());
        Some((window.controls.banners[0].clone(), branch))
    });
    if let Some((banner, branch)) = buttons {
        // SAFETY: a click sends the button's action to its target, which
        // takes the sender.
        unsafe { banner.performClick(None) };
        if let Some(branch) = branch {
            // SAFETY: as above.
            unsafe { branch.performClick(None) };
        }
    }
}
