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
- Detect duplicate content and avoid creating duplicate received items.
- Windows notifications for received items.
- Activity log for pairing, connections, transfers, and failures.
- Clear all shelf items with one action.
- Detect the Windows system language and localize the interface in English,
  French, Spanish, or German, with English fallback.
- Windows tray/shortcut-friendly installer workflow.

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
- Received text or images can be copied back to the clipboard.
- Received files can be revealed in Explorer.
- Items expire after one hour by default.
- Use **Pause receiving** to reject new incoming transfers.

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
- Unpaired devices are rejected.
- The installer opens the Windows Firewall only for PRIVATE networks.
- Room traffic uses the same authenticated LAN channel.
- The device key is currently stored in
  %LOCALAPPDATA%\\Shelf\\config.json; DPAPI protection is a future hardening
  task.

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
crates/shelf-app/        Windows edge UI and clipboard integration
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
