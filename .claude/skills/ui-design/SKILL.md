---
name: ui-design
description: SuperGIT's UI rules and review checklist — layout stability, top-down hierarchy, toolbars, buttons and tooltips, keyboard shortcuts, color contrast (WCAG AA), empty/loading/error states. Use when adding or changing anything on screen (views, panels, bars, buttons, colors, shortcuts) and before calling a UI change done.
---

# UI design rules for SuperGIT

A native Git GUI on macOS and Linux (GPUI). These rules distill the
platform guidelines (sources at the end) into what keeps biting in this app.
CLAUDE.md's **Buttons**, **Icons**, **Review layout** and **Panel lines**
notes are part of this; read them too.

## 1. Find the existing pattern first

Before drawing anything, look for the piece that already exists and reuse
it — consistency beats a locally nicer variant (Nielsen #4, "consistency and
standards").

- Heights: `RAIL_WIDTH` (48px: headers, rail squares), `TOOLBAR_HEIGHT`
  (41px: bars above columns, via `toolbar(theme)`), `COMMIT_ROW_HEIGHT`.
- Pieces: `toolbar()`, `square_button()`, `rail_square()`,
  `render_data_notice()` (empty/error/stale states), `elastic()` (keeps a
  wide child from pushing panels out), `crate::selectable::SelectableLabel`.
- Colors: only `cx.theme()` tokens. No literal colors except brand marks
  (`icons::agent_icon`).

## 2. Layout stability: nothing jumps

Controls stay where the hand left them.

- Showing/hiding a panel, loading, selecting, a counter changing, a hover —
  none of these may move other controls. A bar that holds toggles spans the
  area it controls and stays put whether its panel shows or not (the review
  bar sits above the commit tree, not beside it).
- Reserve space for things that come and go: a fixed slot for a count or a
  spinner, the count inside the search field (not a second line under it).
- Loading keeps the layout: same rows and bars, placeholder content, the
  last good data with a stale notice — never an empty area that collapses.
- Side-by-side bars share a height, so their bottom lines meet.
- One line between panels (CLAUDE.md "Panel lines"): no doubled borders.

## 3. Hierarchy: top-down, general to specific

The window reads top to bottom, each level narrower than the one above:
worktree header (what and where: `repo / worktree`, branch actions) →
review bar (History toggle, Changes / Agents / Pull requests) → each
column's bar (search, filters, view options) → content.

- A control lives at the level it acts on. Branch actions go in the header,
  not in a commit's card. A diff's layout goes above that diff.
- One primary (filled) button per area at most. Everything else is ghost.
- Group related controls; separate groups with space or a 1px divider
  rather than boxes.
- Don't repeat information shown elsewhere on screen (the removed fork
  summary bar repeated the badge, the tab count and the header status).

## 4. Buttons, labels, tooltips

- Every button has a tooltip: one short phrase saying what it does. It does
  not repeat the label, and it names the shortcut if there is one (`MOD`).
  The delay is `TOOLTIP_DELAY`.
- Labels are short verbs or nouns. No trailing "…" on buttons (menu items
  keep it). Toolbar buttons show an icon plus a label. Icon-only buttons
  are fine in tight bars, as long as they have tooltips.
- Toggles look selected while on (`.selected(true)`, gray).
- A disabled button's tooltip says why it is disabled ("No upstream yet:
  publish the branch with Push").
- Destructive actions preview first or can be undone (worktree Delete →
  the cleanup preview).

## 5. Keyboard

- Frequent actions have shortcuts: `shortcut!("…")` in main.rs, shown in
  the tooltip and in the menu bar (macOS) / `AppMenuBar` (Linux).
- Don't reassign standard platform shortcuts. macOS: ⌘Q quit, ⌘W close,
  ⌘H hide, ⌥⌘H hide others, ⌘M minimize, ⌘, settings, ⌘C/V/X/A/Z editing.
  The app menu keeps Hide / Hide Others / Show All and a Window ▸ Minimize.
  GNOME: Ctrl+Q, Ctrl+W, Ctrl+, and Ctrl+N; Super is the system's.
  Toggles of the app's own panels take ⌘ plus a letter nobody else claims
  (⌘B sidebar, ⌘L agent panel, ⌘J terminal), or ⌘⇧ (⌘⇧H History).
- Esc closes dialogs and popovers; Return runs a dialog's default action.
- Linux: every app `ctrl-` shortcut must also be released to programs in
  the embedded terminal (`bind_terminal_keys` in main.rs).

## 6. Color and contrast (WCAG 2.2 AA)

- Text needs at least 4.5:1 against its background (1.4.3). Control
  boundaries, icons and state marks need 3:1 (1.4.11).
  `contrast::tests` checks the theme's pairs. A new color pair goes into
  that test.
- Color is never the only signal. Use + and − signs with green/red, words
  on badges ("fork point"), and icons next to status colors.
- Fading (opacity, muted) is for context the user may skip. Never use it on
  something they must read to act.
- gpui-kit's status colors are made for fills: `appearance::
  readable_status_colors` darkens the light theme's success, danger and
  warning so they read as text, and gives filled danger/success buttons
  dark text in the dark theme. Patch the palette there, not at call sites.

## 7. States

Every view has all of them designed:
- empty ("No commits yet", "No files match");
- loading;
- error, with a next step (Retry);
- stale data ("may be out of date — showing the last successful data");
- disabled.

Never a blank area, never a dead end.

## 8. Text

- Long names (branches, paths, subjects) truncate. Use `.truncate()` with
  an ellipsis. If the text must stay selectable, clip without an ellipsis
  (`SelectableLabel`).
- Cut user-visible text by `chars()`, never by byte index (CLAUDE.md).
- The full value sits in a tooltip when truncation hides it (the
  worktree's path on the header title).

## 9. Verify before saying done

1. Screenshot build (CLAUDE.md "Seeing the UI"). Use an isolated
   `SUPERGIT_STATE_DIR` and a fixture repository with long names, a linked
   worktree, and empty and changed states.
2. Check a narrow layout (chat open) and a wide one (chat closed), and
   panels shown and hidden: nothing jumps between them.
3. Check both appearances: `{"appearance":"dark"}` in the profile's
   `settings.json`.
4. `cargo test -p supergit contrast` after touching colors.

## Review checklist

Paste it into the review and answer each line:

- [ ] Reuses existing heights, bars and buttons; no one-off styling
- [ ] No control moves when panels toggle, data loads or selection changes
- [ ] Sits at the right level of the top-down hierarchy; nothing duplicated
- [ ] Every button: tooltip (what it does + shortcut), no "…" in the label
- [ ] Shortcuts don't take standard platform ones; Linux terminal releases them
- [ ] Text ≥ 4.5:1, marks ≥ 3:1; color never the only signal
- [ ] Empty / loading / error / stale / disabled states designed
- [ ] Long names truncate (full value in a tooltip)
- [ ] Screenshots: narrow + wide, light + dark

## Sources

- Apple Human Interface Guidelines — macOS, toolbars, menus, keyboards:
  https://developer.apple.com/design/human-interface-guidelines/designing-for-macos,
  /toolbars, /the-menu-bar, /keyboards (append `.md` for plain text).
- GNOME HIG — header bars, keyboard, standard shortcuts:
  https://developer.gnome.org/hig/ (patterns/containers/header-bars.html,
  reference/keyboard.html).
- WCAG 2.2 Understanding 1.4.3 Contrast (Minimum) and 1.4.11 Non-text
  Contrast: https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum,
  https://www.w3.org/WAI/WCAG22/Understanding/non-text-contrast
- Nielsen Norman Group, 10 usability heuristics:
  https://www.nngroup.com/articles/ten-usability-heuristics/
- Vercel Web Interface Guidelines (web-first; layout stability, states,
  focus, keyboard): https://github.com/vercel-labs/web-interface-guidelines
- Deeper macOS rules as an agent skill (MIT, SwiftUI-oriented):
  https://github.com/ehmo/platform-design-skills (`skills/macos`)
