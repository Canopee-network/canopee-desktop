# canopee-tray next steps

## Done
- Standalone `.app` packaging: `./scripts/build_app.sh` → `dist/Canopee.app`
  (Info.plist with `canopee://` URL scheme, `LSUIElement`, bundled icon, ad-hoc
  or `CODESIGN_IDENTITY` signing, LaunchServices registration). Notarization
  steps documented in the script.
- `canopee://` handling in-app: `NSApplicationDelegate` receives URLs
  (`application:openURLs:`) → routed to the worker → parsed/resolved
  (`src/uri.rs`, alias file aware) → app resolved (own HomeIndex locally,
  else DHT `ResolveAppPointer`) → fetched/stored, served over local HTTP, and
  opened in the browser. Verified end-to-end with a published app. Menu "Open
  app" now uses the same path directly (no URL-round-trip loop).
- Icon state differentiation (B2): `src/badge.rs` composites a status dot
  (green=Running / plain=Stopped / red=Error) onto the menu-bar icon at
  runtime via NSImage → PNG, and `set_icon` swaps per state. Verified by unit
  tests (valid 24×24+ square PNGs, per-state icons distinct) and a live run.
- Pluralized status counts (B3): objects/peers/relays now read "1 object",
  "2 peers" etc. in the menu line and tooltip.
- Start failure surfaced (B4): when the node can't be opened at launch,
  Start emits "cannot start node — failed to open at launch" instead of a
  silent no-op.
- Notifications (A7): the worker tracks object/peer/error transitions and
  enqueues them; the main-thread timer delivers `NSUserNotification` banners
  (deprecated API, no permission prompt — documented). Verified live:
  "New object stored — 51 total" and peer-connect banners.
- URL open failures surfaced in status (B7): `spawn_open` reports through a
  channel; worker shows "could not open …/timed out opening …" in the menu
  line (and clears it on success instead).
- Graceful quit (C4): Quit stops the node (socket Shutdown + await task) and
  sets a flag the main-thread timer acts on via `app.terminate`, so the
  `process::exit(0)` shutdown race is gone.
- Data window (`src/data_window.rs`, "Open My Data…" in the menu): a 3-tab
  NSWindow (Storage / Contacts / Devices) with per-tab search, empty states,
  and node-backed actions over a `DataAction`/`DataFeedback` channel pair.
  Covers add files, import/export bundle, rename, share/unshare, delete, copy
  id, contact CRUD + profile resolve, device sync/remove.
  - Identity header: display name, username and identity are visible and
    editable in-app ("Edit Profile…", "Username…"), so setting a name or
    claiming a username no longer requires the CLI.
  - "Save a Copy…" writes an object's original bytes to a chosen path and
    "Open" (or double-clicking a row) hands them to the default application —
    previously the only way to get data back out was the CLI, and "Export…"
    writes an opaque bincode bundle.
  - Data actions run on their own tokio task, so a slow import or multi-file
    add no longer stalls the tray's 2s refresh loop.
  - Rename now moves the object's home entry (and republishes the
    `entry:<name>` pointer when shared); `SetName` alone only writes the local
    sidecar, so renaming a shared object used to appear to do nothing.
  - `--data` opens the window at launch, for QA without clicking the tray icon.

## Known issues
- `badge::tests::badge_png_encodes_24x24_png` fails (pre-existing, unrelated to
  the data window): the composited status dot covers fewer pixels than the test
  expects. Needs a look at `src/badge.rs` compositing.
- The window's action buttons are a flat left-aligned row. A popup "Actions ▾"
  plus a primary button, and right-click context menus on rows, would be closer
  to AppKit convention than nine buttons in a line.
- Contact add/edit takes a raw `dh_public_key` as pasted hex; there is no key
  exchange flow in the window yet, so sharing contact keys still needs the CLI
  or a `canopee://` link.

## Next

A. Deferred features that need macOS UI (dialogs / text input)
1. ~~Export / Import storage~~ — DONE in the data window.
2. Dial a peer / listen via relay — text-input dialog (peer ID / relay ID) wired to the runtime's network manager to open a connection.
3. PubSub — subscribe/unsubscribe topic + publish (input dialog).
4. ~~Profile / contacts~~ — PARTIAL: profile + contact list are manageable;
   adding a contact by scanning a `canopee://` link (instead of pasting a hex
   key) is still open.
5. Preferences / Launch at login — a small settings sheet (SMAppService / SMAppService.mainApp, menu submenu with checkboxes).
6. First-run onboarding — a proper window (not just the menu) on first launch: show peer ID, copy/backup identity key. The data window's identity header covers most of this.
7. Notifications — NSUserNotification/UNUserNotificationCenter on node events (peer connect, new object, error).


B. Feature gaps / polish already scoped
1. App launch via canopee:// — DONE (tray is the scheme handler; remote issuance falls back to DHT `ResolveAppPointer`, which times out offline).
2. Icon state differentiation — DONE via runtime-composited status badges; a hand-drawn state icon set (`APP_ICON_SRC` in build_app.sh) would still be nicer.
3. Grammar in status line — DONE ("1 relay", pluralized counts).
4. NodeState::start() failure — DONE, now surfaces the error.
5. Home entries that aren't apps — they render as a disabled label ("penguin.png"); decide if they should be openable (document URL scheme) or stay informational.
6. Error handling for SetHomeEntryShared — sets self.error but the checkable toggle state relies on the refresh cycle to flip; confirm UX feels instant.
7. URL-handler failures — DONE (reported into the tray status line via the report channel).
8. Open-app HTTP server is minimal (no gzip/range/ETag) and lives until the tray quits or 3min idle — port the CLI server's semantics if needed.
9. Notifications use the deprecated NSUserNotification API (no permission prompt needed); migrate to UNUserNotificationCenter + authorization if desired.

C. Verification / hardening
1. Manual QA — menu actions (open app, pbcopy, share toggles, Start/Stop/Restart, Quit) can only be click-tested; set up a checklist.
2. .gitignore — target/, dist/ covered.
3. Cross-check client/server framing — socket.rs is untested-since-rewrite; the CLI stop proved the same wire format works, but add a Rust unit/integration test for the bincode framing.
4. Graceful shutdown of worker task — DONE (Quit stops the node and lets the app terminate itself); a SIGTERM handler is still open work.
5. Real app icon artwork — the bundle icon is upscaled from the 24px menu glyph (APP_ICON_SRC override in build_app.sh).
6. Notarization + stapling for external distribution — documented in scripts/build_app.sh.

Priority order I'd suggest next: a popup/context-menu pass on the data window's
button rows (it has grown to nine buttons on the Storage tab), then A2–A3
(dial/listen/PubSub dialogs) since overlay menus are already needed for URL text
input; then A5–A6 (prefs/onboarding); then migrate notifications to
`UNUserNotificationCenter` if banners are insufficient.
