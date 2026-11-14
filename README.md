# Shelf

A shelf at the edge of your screen that is shared between your PCs.
Drag a file, image or text onto it and it appears on your other PCs; paste with Ctrl+V.

## For users: install
1. Download **Shelf-Setup.exe** and run it (keep "Start Shelf when Windows starts" ticked).
2. Shelf starts and sits at the right edge of the screen. Move the mouse there to open it.
3. Do the same on the other PC, then pair once:
   - PC A: Devices > "Show a pairing code on this PC".
   - PC B: Devices > "Pair" next to PC A > type the code > Connect.

## Using it
- Drag files onto the shelf, or press "Send clipboard" (files, image or text).
- The receiving PC puts the item on its clipboard (switch off with "Auto-copy received"),
  so Ctrl+V pastes it anywhere. "Copy" re-copies, "Show" opens the folder, "Remove" deletes.
- Items and their files expire after 1 hour.

## Collaboration rooms
- Open **Rooms** and create a named class or team room. The owner receives an invite
  code and QR payload for the room.
- Pair with the owner, then enter the room ID and invite code under **Rooms** and
  choose **Join**.
- Select **Use dropbox** for a room. Dropped files are replicated to the room
  members in each member's local Shelf room folder.
- Enter an assignment name before dropping files to submit timestamped work.
- Members can request a named file from the room owner.
- Room owners can delete room files; members can upload but cannot delete shared
  room entries. Same-name files are preserved as conflict copies.
- Room broadcast and direct/selected-device sending continue to work independently
  of the active room.

## For the developer: producing Shelf-Setup.exe
Option A (no setup on your PC): put this folder in a GitHub repository, open the
Actions tab, run "Build Shelf installer". When it finishes, download Shelf-Setup.exe from
the run's Artifacts. Pushing a tag like v0.1.0 also attaches it to a GitHub Release,
which gives you a public download link to share.

Option B (on a PC with Rust and Inno Setup 6): double-click build-installer.bat.

## Security
- Pairing: 6 digit code + SPAKE2. The code is never sent; each code allows ONE attempt
  and expires after 2 minutes.
- Transfers: Noise_XX (X25519, ChaChaPoly, BLAKE2s), mutually authenticated; unpaired
  devices are rejected.
- The installer adds a Windows Firewall rule for PRIVATE networks only.
- The device key is stored in plain text in %LOCALAPPDATA%\Shelf\config.json (DPAPI
  protection is a good future improvement).

## Layout
- crates/shelf-core: discovery (mDNS), pairing, encryption, transfer, engine
- crates/shelf-app:  the screen-edge shelf (egui/eframe) and clipboard integration
- installer/:        Inno Setup script and icon
- .github/workflows: cloud build of the installer

## Not included yet
Dragging items OUT of the shelf into other apps (use Ctrl+V or "Show"), folders,
tray icon, code signing (Windows SmartScreen will warn on an unsigned installer),
multi-monitor placement, phones, internet relay.
