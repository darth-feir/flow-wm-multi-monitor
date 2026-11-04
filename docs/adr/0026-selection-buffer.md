# The selection buffer is a keymap option read through UI Automation

**Status:** Accepted (stage 32, 2026-11-03; Alacritty added in stage 32b;
the ancestor walk for web pages in stage 32c, 2026-11-04). Adds `[keymap] selection_buffer`
(off by default) and `keymaps/win-niri/selection.ahk`. The daemon only
carries the option to the keymap ([ADR-0015](0015-binds-are-config.md):
the binds listing is the keymap's single channel to the configuration).

## Context

On Linux (X11, and Wayland compositors such as niri through the primary
selection protocol) highlighting text with the mouse makes it the *primary
selection*, and a middle click pastes it, independently of the clipboard
(Ctrl+C / Ctrl+V). The user wants that on Windows: no history, no change to
the clipboard they copy to, and nothing that breaks the middle click where
it means something else (a link opened in a new tab, autoscroll in a
browser, a Remote Desktop session).

Windows has no such buffer. Applications do not announce a selection; the
only cross-application ways to read one are the clipboard (send Ctrl+C) or
UI Automation (the text pattern of the element that holds the selection).

## Considered Options

- **Where it runs.**
  - **(a) In the keymap (AutoHotkey), as an `#Include` of `flow.ahk`.**
    Chosen: the keymap already owns the mouse hook (Mod + drag), lives in
    the daemon's job object and ends with it; an include cannot outlive the
    script that holds it, and no second process or window is needed.
  - (b) A separate script `flow.ahk` runs and closes. Rejected: two hook
    processes, and a crash of `flow.ahk` would leave it running.
  - (c) In flowd. Rejected: a mouse hook and clipboard handling in the
    window manager, for a feature that is input, not layout.
- **How the toggle reaches it.**
  - **(a) An `option selection_buffer 0|1` record in the `flow query binds
    --plain` listing.** Chosen: the keymap already re-reads the listing when
    `flow.toml` is saved, so the option is live without a restart, and an
    older daemon (no record) means off. The overlay row comes with it.
  - (b) An environment variable at launch, like `keymap_log`. Rejected:
    only read at `flow start`.
- **How the selection is read.**
  - **(a) UI Automation, own COM vtable calls (`IUIAutomation`,
    `IUIAutomationTextPattern::GetSelection`, `IUIAutomationTextRange::GetText`).**
    Chosen: reads the selection without touching the clipboard; about
    twenty vtable calls, so no library is vendored.
  - (b) The MIT-licensed Descolada UIA-v2 library. Rejected for now: a
    5000-line include for the handful of calls used here.
  - (c) Send Ctrl+C and restore the clipboard. Rejected (the user's call):
    it fires on every drag, Ctrl+C means *interrupt* in a terminal, and it
    churns clipboard history. Not built; it can be added later as an opt-in
    fallback for apps without a text pattern.
- **Where a middle click pastes.**
  - **(a) Only where UI Automation says there is a writable text field
    (or the window is a known terminal); everything else gets its middle
    click back unchanged.** Chosen: safe by default; a button or a link is
    never clicked by accident.
  - (b) Everywhere except an exclusion list. Rejected: every app with a
    middle-click meaning would have to be listed, and a left click sent to
    place the caret could press a button.

## Decision

- `[keymap] selection_buffer` (`KeymapConfig`, default `false`), reported by
  `query binds` (`BindsReport::selection_buffer`, `#[serde(default)]`) with
  a *Middle click* hardwired row when on, and printed in `--plain` as
  `option selection_buffer 0|1` before `source`. `flow.ahk` parses it in
  `ApplyBinds` and calls `SelectionApply(on)`; off registers nothing (and
  forgets the buffer).
- **Capture:** `~LButton` / `~+LButton` record the press; `~*LButton Up`
  triggers a read after a drag of more than 8 px or a double / triple click
  (the system double-click time, 4 px). The read runs 80 ms later on a
  timer, off the hook: the focused element's selection, else the element
  under the release point, else their ancestors (stage 32c, *Where the
  selection lives* below); then, for a focused classic Edit control (class
  `Edit` or `WindowsForms10.EDIT.*`), `EM_GETSEL` + `WM_GETTEXT`: such a
  control (a .NET Framework WinForms `TextBox`, seen in the VM) answers
  `WM_GETOBJECT` with its own MSAA object, so UIA offers no text pattern
  for it. A password element (`IsPassword`, or `ES_PASSWORD` on the Edit)
  stops the read. An empty selection keeps the buffer. At most 1 Mi characters.
- **Paste:** `MButton` (no modifier) under a cheap `HotIf` (buffer
  non-empty; not the taskbar, the desktop, a border overlay, Remote
  Desktop, `vmconnect.exe` or the keymap's own windows). The handler asks
  UIA for the element under the pointer and up to three parents:
  `Edit` not read-only, or `Document` whose ValuePattern says writable, or
  (outside browsers) a keyboard-focusable `Document` with a text pattern.
  Known terminals (Windows Terminal, conhost, Alacritty, WezTerm, mintty,
  PuTTY, ConEmu) skip the check. A password or read-only field, a browser
  page, anything else: the suppressed press is replayed (down now, up when
  the button is released). To paste: a left click at the pointer (focus +
  caret; terminals are only activated), `ClipboardAll()` saved, the buffer
  set as `CF_UNICODETEXT` with `ExcludeClipboardContentFromMonitorProcessing`,
  `CanIncludeInClipboardHistory = 0` and `CanUploadToCloudClipboard = 0`,
  Shift+Insert (Ctrl+Shift+V for Alacritty), 300 ms, the clipboard
  restored.
- UIA connection / transaction timeouts are lowered to 1 / 1.5 s (when
  `IUIAutomation2` reports the documented defaults), so a hung app cannot
  hold the keymap for 20 s.
- Calls into the script's own windows are skipped (UIA must not call its
  own UI thread). An elevated window refuses UIA and input (UIPI): nothing
  happens, silently.
- The trace logs `selection:` lines: lengths, the executable, why a click
  was passed on; never the text.

## Alacritty (stage 32b, 2026-11-03)

Alacritty draws its own text and exposes nothing to UI Automation. On
Windows it has no console buffer and no IPC socket. The one thing it
announces is its own copy-on-select.

**What Alacritty does (0.17.0, verified in the VM with a clipboard
monitor).** With `[selection] save_to_clipboard = true`, `copy_selection`
stores the selection to the system clipboard as well as to the *selection*
clipboard. On Windows the selection clipboard is a no-op: there is no
provider, so only one store remains. The writes:

| Gesture | Clipboard writes |
|---|---|
| drag | one, on the left release |
| double click | two: on the second press and on its release |
| triple click | also on the third press and on its release; the text ends in `\n` |
| plain click | none |

Each write is a separate open / empty / set / close. The owner window is
NULL and the formats are CF_UNICODETEXT plus the synthesized ones. With
the option off (Alacritty's default), nothing is written.

**Options.**

- **(a) The copy-on-select write as the signal, read, then undone.**
  Chosen. It needs nothing from Alacritty but its own option. The clipboard
  is touched only for `alacritty.exe` and is put back.
- (b) Send Ctrl+Shift+C after a highlight. Rejected, for the reasons
  Ctrl+C was rejected above. It is also one more keystroke into the
  terminal.
- (c) Read the screen (OCR) or the PTY. Rejected: no stable way to do it
  from outside the process.

**Sequence (`selection.ahk`).**

1. On the first left press of a click sequence in an Alacritty window
   (exe `alacritty.exe`), while no Alacritty capture is running:
   `ClipboardAll()` snapshot plus `GetClipboardSequenceNumber()`. A double
   click writes on its second press already, so a later press of the same
   sequence keeps the first snapshot. For Alacritty, a sequence counts by
   the double-click time only, not by distance: Alacritty's own click
   counter ignores the distance, and it cycles single / double / triple.
2. On the release after a drag (> 8 px), a double or triple click, or a
   Shift+click (which extends the selection), a 15 ms timer starts the
   capture. It polls the sequence number every 20 ms for up to 300 ms.
3. Moved: wait until it holds still for one poll (the second write of a
   double click). Then read `A_Clipboard` and note the sequence number.
   Non-empty text becomes the buffer.
4. If the sequence number is still the one noted at the read, restore the
   snapshot (`A_Clipboard := snapshot`). Otherwise something wrote after
   the read (the user's Ctrl+Shift+C, another app): that write wins and
   nothing is restored.
5. Never moved: the buffer is kept, nothing is restored, and the trace
   says so.

A release while a capture runs (the third click of a triple, before the
double's capture ends) queues one more capture. The queued capture runs
right after the first one, against the sequence number of the first one's
read. A paste (`SelPaste`) during a capture bumps a counter; the capture
then keeps the buffer and only restores. Restores and Alacritty's own
writes are never marked for clipboard history: Alacritty's write is not
ours to mark, and the paste path's restore is unmarked too. Trace lines:
`captured N chars (drag|double|triple|shift-click, alacritty
copy-on-select) from alacritty.exe`, `alacritty did not copy within 300
ms; buffer kept (N chars)`, `clipboard restored (N bytes)`, `the
clipboard changed again after the read; not restored`.

**Limits.**

- The user must set `save_to_clipboard = true`. Without it, every
  highlight times out (logged).
- For the time between Alacritty's write and the restore (under 100 ms:
  the 15 ms timer, one or two 20 ms polls), the
  clipboard holds the highlighted text. Clipboard history, cloud clipboard
  and clipboard managers see Alacritty's unmarked write. A paste
  elsewhere in that window would paste the highlight.
- A copy by the user in the few milliseconds between the read and the
  restore check is detected. A copy between Alacritty's write and the read
  is not: it is read as the selection and then undone.
- A selection extended with the right button (Alacritty copies on the
  right release too), or made in vi mode, is not captured, and its write
  stays on the clipboard.
- `ClipboardAll()` runs on every first press in Alacritty, so a large
  clipboard (an image) is copied once per click.
- **Test rig.** The guest's display has OpenGL 1.1 only, so the real
  `alacritty.exe` exits at start (WGL). The VM runs a copy with ANGLE from
  the guest's own Edge WebView (`libEGL.dll`, `libGLESv2.dll`) and
  `[debug] prefer_egl = true`.

## Where the selection lives (stage 32c, 2026-11-04)

**Finding (Firefox Developer Edition 160.0b4 in the VM, native UI
Automation; a probe dumped the focused element, the element under the
pointer and their control-view ancestors after a real drag).**

- Firefox keeps a page's selection on the **Document** element's text
  pattern, one Document per frame (an iframe's text is on its own Document,
  a child of the outer page's Document). The text runs (`Text`) under the
  pointer have a text pattern too, but `GetSelection` on them returns one
  empty range even when the selection lies inside them. Containers
  (`Group`) and links have no text pattern.
- A click on non-focusable text gives the focus to the Document, so on a
  plain page (Wikipedia, MDN, a local page after clicking a link, a button
  or an input) the focused element answers. The failing shape is a
  **focusable container**: a `tabindex` scroller or article, as chat-style
  web apps build them. A press inside one focuses it: the focus is a `Group`
  without a text pattern and the pointer is a `Text` with an empty range, so
  neither of the two elements asked by stage 32 has the selection. The
  Document above both has it.
- **A cold accessibility tree.** Firefox builds its tree on the first UI
  Automation request. The first capture in a Firefox process found the
  focused Document with no name and no selection (ranges = 0), and the
  element under the pointer was the top-level `Window`. A selection made
  before the tree existed could stay unreadable after it (seen on a local
  page). This was the first capture after `UI Automation ready` failing
  on the host. A new site in the same Firefox (a new content process)
  read fine at once.
- Wikipedia, MDN, an iframe page and a page with a focused link / button /
  input captured on the stage 32 code once the tree was warm. The login
  page of a Discord-style app reported no selection anywhere: it sets
  `user-select: none`, nothing is highlighted, so "no selected text" is
  right there.

**Options.**

- **(a) Walk the control view up from the pointer element, then from the
  focused element, and ask every ancestor with a text pattern.** Chosen:
  works for any app that keeps the selection on a container (Firefox's
  Document, and the same structure in Chromium and Electron), not only
  Firefox. No control type is required.
- (b) Ask only for the nearest Document (a tree walker with a control-type
  condition). Rejected: tied to web pages; (a) reaches the Document anyway
  and stops there.
- (c) Retry the same two elements later. Rejected as the fix for the
  container case: their answer does not change. Kept, once, for the cold
  tree (below).

**Decision.**

- After the focused element and the element under the pointer, the capture
  walks up from the pointer element through the control view (up to 24
  ancestors; a chat-style page measured 15 hops from a text run to its
  Document), then from the focused element. Each ancestor is checked for
  `IsPassword` (stops the read) and asked for its selection when it has a
  text pattern. The first non-empty selection wins. A walk stops at the
  first Document (a page's selection is its own; an outer page's Document
  is not asked about a selection in its iframe), at a `Window`, and when the
  whole capture has run 1.5 s (plus the UIA timeouts of one call). A
  focused or pointer element that is itself a Document or a Window is not
  walked from.
- The first left press in each browser window (the browser list of the
  paste side) asks, on a timer, for that window's element and the focused
  element, so Firefox builds its tree while the user is still selecting.
  If a capture comes back empty within 3 s of that, it is read once more
  400 ms later (trace: `nothing yet (...); the accessibility tree was just
  woken, reading again in 400 ms`).
- The "no selected text" line names what was asked, by control type, never
  text: `drag in firefox.exe: no selected text (focus Group no-text ->
  Document empty; pointer Text empty -> Group -> Document empty); buffer
  kept (N chars)`. `no-text`: no text pattern (said for the first element
  of a chain; ancestors without one are listed by type only); `empty`: a
  text pattern without a selection; `error`: the call failed;
  `x3`: three in a row of that type; `(out of time)`: the 1.5 s ran out. A
  capture says where it found the text: `(drag, pointer ancestor
  Document)`.

**Limits.** A page that turns selection off (`user-select: none`) has
nothing to capture. If the woken tree still lacks the selection after the
second read (a slow machine, a huge page), that first highlight is missed;
the next one is read normally. The walk costs up to
four UIA calls per ancestor (control type, IsPassword, pattern,
selection); Firefox answers them from its parent-process cache.

## Consequences

- A highlight in apps without a text pattern is not captured. Alacritty
  is captured through its copy-on-select (above), when the user has turned
  it on. Pasting into Alacritty works.
- In browsers, only fields paste; a middle click on a page or a link is the
  browser's (new tab, autoscroll).
- A paste is visible to the target app as a clipboard paste; the restore is
  an ordinary clipboard write, so Win+V history may show the original entry
  again on top (the buffer text itself is never recorded).
- A middle click over a text field costs one UIA hit test before it is
  replayed or pasted (milliseconds; at most the 1.5 s timeout on a hung
  app).
- **Tests.** Unit tests for the option in the config, the report, the
  `--plain` record and the baked fallback block; the VM driver
  `test-stage32.ps1` drives the real mouse in the guest (Notepad, a WinForms
  window with a password box, Edge); `test-stage32b.ps1` drives Alacritty;
  `test-stage32c.ps1` drives Firefox on a local page (focusable scroller and
  article, an iframe, a link, the first drag after Firefox starts).
