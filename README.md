# Shelf

![Shelf app icon](crates/shelf-app/assets/icon.png)

Shelf is a Windows edge-of-screen sharing tool for nearby PCs. Send files,
images, clipboard text, and classroom or team work over an authenticated local
network connection.

## Features

- Send clipboard contents, files, and images to one, several, or all paired PCs.
- Drag one or many files onto the shelf, including complete folders.
- Preserve folder structure when transferring directories.
- Compress text, files, and folders automatically when compression makes them smaller.
- Show upload/download progress, speed, ETA, and transfer failures.
- Pause incoming transfers while continuing to send.
- Cancel active transfers and resume interrupted file transfers.
- Cancel transfers individually from their own progress row; interrupted large files resume from the completed portion after a reconnect.
- Right-click files or folders in Explorer and choose **Send with Shelf** instead of dragging them to the edge.
- Show a QR code that lets a phone send photos, files, whole folders and text to the shelf from its browser, and lets the phone download files and folders you choose on the PC, with nothing to install.
- Use the shelf as a stash when no device is online: drop files and folders on it, keep them as long as you like, and drag them out into any app later.
- Keep a folder such as Downloads tidy: Shelf watches it and sorts new files into PDFs, Images, Installers, Archives, Documents, Audio, Video, Design (PSD, AI, Affinity, Figma…), Code, 3D (GLB, OBJ, FBX…) and Fonts, with a preview and one-click **Undo** (see **Organizer** below).
- Batch image tool: drop many images and resize, convert (PNG, JPG, WebP), compress and rename them in one go.
- Detect duplicate content and avoid creating duplicate received items.
- Preview received images and PDFs in the item list, with a quick text peek for received text.
- Windows notifications for received items.
- Activity log for pairing, connections, transfers, and failures.
- Clear all shelf items with one action.
- Detect the Windows system language and localize the interface in English,
  French, Spanish, or German, with English fallback.
- Windows tray/shortcut-friendly installer workflow.
- Tray icon: left-click to open the shelf, right-click for Show Shelf / Quit Shelf.
- Global shortcuts: Ctrl+Shift+S toggles the shelf; Ctrl+Shift+C sends the current clipboard to the selected device.
- Choose the left or right screen edge, and which monitor, under Send to.
- Choose the look under Style: the macOS glass style or Material 3 (tonal buttons, filter
  chips, Material switches and surface-container cards). Both follow the system light/dark setting.

## Install

1. Download the latest **Shelf-Setup.exe**.
2. Run the installer.
3. Keep **Create a desktop shortcut** selected if you want a desktop shortcut.
4. Optionally enable **Start Shelf when Windows starts**.
5. Repeat on each nearby Windows PC.

Shelf opens as a narrow bar at the right edge of the screen. Move the pointer
to the edge to expand it.

## Pair PCs

1. Open **Devices** on PC A.
2. Choose **Show a pairing code on this PC**.
3. On PC B, choose **Pair** beside PC A.
4. Enter the six-digit code and select **Connect**.

Pairing is required before devices can exchange data.

## Everyday sharing

- Choose **All devices** to broadcast.
- Select several paired devices to send to a specific group.
- Drag files onto the shelf or choose **Send Clipboard**.
- Use **Ctrl+Shift+C** to send the current clipboard without opening the shelf.
- Use **Ctrl+Shift+S** to open or close the shelf from any application.
- Use Explorer's **Send with Shelf** context-menu action for files and folders.
- Received text or images can be copied back to the clipboard.
- Received files can be revealed in Explorer.
- Items expire after one hour by default.
- Use **Pause receiving** to reject new incoming transfers.

## Use it as a stash

With no paired device online, the shelf works as a personal stash. Drop files or folders on
it and Shelf keeps a copy until you remove it; nothing expires and the copies survive restarts.

- **Drag out:** grab the top of an item card (the hand cursor shows) and drop it in Explorer,
  an email, a chat window or any app that accepts files. Folders work too.
- Items stay in the stash after dragging, so you can drop them in several places. Use the
  remove button on the card, or **Clear all**, to delete the stashed copy.
- The originals are never moved or changed. If a device is online, drops are sent to it as usual.
- Stashed items live in `%LOCALAPPDATA%\Shelf\stash`.

## Send from your phone

Any phone with a camera and a browser can drop files, folders and text on the shelf, and save files you share back. The phone does not
need Shelf installed and does not need to be paired.

1. Make sure the phone and the PC are on the same Wi-Fi or network.
2. Click the **QR** button in the round quick-action row (hover text: *Phone upload*).
3. Scan the code with the phone camera and open the link.
4. Tap **Choose files** to pick photos or files, **Choose folder** to send a whole folder, or
   **Send text** to type or paste a note. They appear on the shelf.

A folder arrives as one item with its subfolders intact, and only once every file in it has
arrived, so a half-sent folder never shows up. If the connection drops, **Retry failed**
continues from the file that failed. Text arrives like clipboard text from a paired PC.
Folder picking depends on the phone's browser: Chrome on Android and computer browsers
offer it (computer browsers also accept a folder dragged onto the page); a browser without
folder picking sends the files it is given as plain files. Empty folders are not sent.

Uploads show progress in the transfer list, can be cancelled from their row, honor
**Pause receiving**, and skip files that are already on the shelf. Received photos get the
usual image preview. Click the button again, or **Stop**, to close the page; it also closes
by itself after 10 minutes. **Copy link** is there if the phone cannot scan.

### From the PC to the phone

With the QR code open, click **Add files** or **Add folder** under it. Whatever you add shows
up on the phone page under **From this PC**, with a **Save** button; folders download as a
`.zip`. Click **Remove** next to an item to take it back off the page.

Only items you add this way can be fetched. The phone cannot browse the PC or the shelf, the
list disappears when you close the page, and links inside a folder are skipped rather than
followed.

> **It is not encrypted.** A phone browser cannot take part in Shelf's paired, encrypted
> channel, so this page is plain HTTP on the local network. The only credential is a random
> token in the link, which changes every time the page is opened. Anyone who can watch
> traffic on that network while it is open could see the link and the files, so use it on a
> network you trust (your own Wi-Fi, not a public one).

## Organizer: tidy Downloads and batch-process images

Open the gear (settings) and expand **Organizer**. Everything runs on your PC; nothing is uploaded.

**A rule is conditions plus actions.** For example: *if it's an image over 2 MB, convert to WebP,
compress, then move to Pictures/Designs.* Conditions: file type, extension, a word in the name,
larger or smaller than a size. Actions: move, rename, resize, convert, compress. The first
matching rule wins, so rules near the top take priority. Files with a type Shelf doesn't know are left where they are; add a rule for any extension (for example `.afphoto`) in the rules form.


### Two ways in

- **Automatic:** turn on **Watch for new files** for a folder (Downloads by default). A file is
  only touched once it has finished downloading; temporary files like `.crdownload` are ignored,
  and files already in the folder are left alone. **Tidy it now** sorts what is already there.
- **Manual:** choose what happens when you drop files on the shelf: **Send them** (the normal
  behaviour), **Sort with rules**, or **Image tool**. Folders are searched recursively.

### Preview and Undo

- **Ask me first** (on by default) shows *Here's what I'd do* for every file and waits for you to
  Apply or Skip. Turn it off to let the watcher act on its own.
- Every action is written to the **Undo history**. **Undo all** (asks you to confirm) reverses every move at once, which is handy after a big batch; one that can't be undone, because the file has since moved, is skipped and reported while the rest go back. **Undo last** or the per-row **Undo** puts the
  file back exactly where it was, removes folders the action created, and never overwrites a file
  that has since appeared at the old location. If the file was moved or deleted again since, Undo
  says so and changes nothing.
- Processed images keep their original in the history's trash until you press **Clear history**
  (or it ages out after 300 actions). Files are never overwritten: name clashes become `name (1).ext`.

### Batch image tool

Pick **Image tool**, choose the output format (Keep, PNG, JPG, WebP), optional max width and height
(images are only ever shrunk, never enlarged), compression, a rename template and a destination
folder, then drop the images. Rename templates use `{name}`, `{date}` (2026-11-25), `{n}` (001, 002, ...)
and `{ext}`, e.g. `{date}_{name}` gives `2026-11-25_logo.webp`.

Compression applies to JPEG quality. PNG and WebP output is lossless, so there it only selects the
strongest compression. JPG has no transparency, so transparent areas become white. Readable formats
are PNG, JPG, WebP, GIF and BMP; other files simply skip the image steps.

### Rules in plain English

Type a sentence into the rules box and press **Make rule**, for example:

- `put school PDFs in Study`
- `move installers under 50 MB into the Software folder`
- `resize screenshots to 1200px wide and rename with the date`
- `if it's an image over 2 MB convert to WebP, compress, then move to Pictures/Designs`

This is a small built-in keyword parser, not an online AI service, so it works offline and sends
nothing anywhere. It shows how it understood the sentence in the normal rule form so you can check
and edit it before saving, and it says so when it can't tell what you mean. Folder names are relative
to the watched folder, except `Pictures`, `Documents`, `Desktop`, `Music`, `Videos` and `Downloads`,
which mean your own folders of those names.

Organizer settings, rules and history live in `%LOCALAPPDATA%\Shelf\organizer`.

## Collaboration rooms

Rooms are LAN-only shared workspaces. Every member keeps a local copy of the
room’s files.

### Create a room

1. Open **Rooms**.
2. Enter a room name and choose **Create**.
3. The owner sees:
   - Room ID
   - Six-digit invite code
   - shelf://room/... invite link
   - QR code for the same invite payload

### Join a room

1. Pair with the room owner first.
2. Paste the complete shelf://room/... link into either room field, or enter
   the Room ID and invite code separately.
3. Choose **Join on** the paired owner device.
4. Shelf receives the room manifest and requests existing room files.

### Shared Dropbox

1. Select **Use dropbox** for a room.
2. Drop files onto Shelf.
3. Files are sent to room members and stored in each member’s local room folder.

Same-name files are preserved as conflict copies. Files include hashes,
uploader identity, timestamps, version information, and optional assignment
metadata.

Room members can upload. Only the room owner can delete shared room entries.
Owner deletions are propagated to members.

### Assignments and requests

Room owners can create assignments with:

- title
- deadline
- instructions
- required-file metadata

Enter an assignment name before dropping files to submit work. The room shows
submission counts and missing-member counts. Members can request a named file
from the room owner.

## Classroom and team support

The Rooms UI includes:

- Owner, member, editor, contributor, and viewer roles.
- Shared notes and links.
- Pinned announcements.
- Shared clipboard entries.
- File versions and edit locks.
- Per-file download restrictions.
- Activity history.
- Room expiration metadata.
- Assignment creation, submissions, and file requests.

Updates are synchronized across members over the authenticated LAN channel.
New rooms become the active dropbox automatically, and idle rooms reuse the
last known address of paired members when discovery briefly goes quiet.

## Security and privacy

- Pairing uses a six-digit one-time code and SPAKE2.
- Transfers use mutually authenticated Noise XX:
  X25519, ChaChaPoly, and BLAKE2s.
- Unpaired devices are rejected. The one deliberate exception is **Send from your phone**,
  which is off until you open it, is upload-only, uses an unguessable one-time link, closes
  after 10 minutes, and is not encrypted (see above).
- The installer opens the Windows Firewall only for PRIVATE networks.
- Room traffic uses the same authenticated LAN channel.
- The device key in %LOCALAPPDATA%\\Shelf\\config.json is encrypted with Windows
  DPAPI, so only your Windows user on this PC can read it. Keys from older
  versions are encrypted automatically the next time Shelf starts.

Shelf does not use a cloud relay. Devices must be reachable on the same local
network.

## Build the installer

### GitHub Actions

1. Push the project to GitHub.
2. Open **Actions**.
3. Run **Build Shelf installer** manually.
4. Download Shelf-Setup.exe from the workflow artifact.

Pushing a tag such as v0.1.1 also attaches the installer to a GitHub Release.

### Local Windows build

Install Rust and Inno Setup 6, then run:

~~~bat
build-installer.bat
~~~

The installer output is written to installer\\Output\\Shelf-Setup.exe.

## Project layout

~~~text
crates/shelf-core/       Discovery, pairing, encryption, transfer, rooms
crates/shelf-core/src/organizer/  Rules, folder watcher, image tool, undo journal
crates/shelf-app/        Windows edge UI and clipboard integration (organizer_ui.rs: Organizer panel)
crates/shelf-app/assets/ Application icon
installer/               Inno Setup configuration and installer icon
.github/workflows/       GitHub Actions installer build
~~~

## Verification

Run the core tests and release build offline from a cached Rust environment:

~~~powershell
cargo test --workspace --offline
cargo build --release -p shelf --offline
~~~
