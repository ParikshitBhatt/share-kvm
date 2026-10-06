# ShareKVM

Share one mouse and keyboard between computers (Windows ↔ Windows, Windows ↔ macOS), and with Android phones, tablets and Android TV. Move the cursor off the edge of one screen and it appears on the other one, like a second monitor. Clipboard text is synced both ways.

## How it works

- **Server**: the computer with the physical mouse and keyboard. A global input hook watches the cursor. When it touches the configured edge, local input is swallowed and streamed to the client.
- **Client**: the computer being controlled. It replays the events and hands control back when the cursor leaves through the edge facing the server.
- Messages are `bincode`, sent as length-prefixed frames over TCP (port 24801) with Nagle's algorithm turned off for low latency.

## Android phones, tablets and Android TV

An Android device can be **controlled** by a computer's mouse and keyboard. The cursor slides off the computer's screen edge onto the phone or TV, the same as onto another computer. (iPhone and iPad can't be controlled this way: iOS doesn't allow any app to drive the device.)

It works without root, using one Android permission (an Accessibility service):

| On the computer | On Android |
| --- | --- |
| Move the mouse | A pointer moves over everything |
| Click / drag / right-click | Tap / swipe / long press |
| Scroll wheel | Scroll |
| Type | Text goes into the selected field (US layout) |
| Arrow keys, Enter | Move the selection like a TV remote's D-pad, and select. ShareKVM draws its own highlight when the app can't show one (for example after a tap) |
| Esc / tap Cmd or Win alone | Back / Home |
| Cmd/Ctrl+V | Paste text copied on the computer |

**Setup:** install `dist/ShareKVM-android.apk` (or build it, below) and open ShareKVM. Then:

1. Turn on **ShareKVM pointer and keyboard** in Accessibility settings.
2. Pick the computer under "Computers on your network" and enter its pairing code once.
3. On the computer, choose the screen edge that leads to the device.

On a TV, everything can be done with the remote.

**How it's built:**

- `crates/sharekvm-android` runs the same encrypted session in Rust (pairing, saved keys and protocol from `sharekvm-core`, built without the desktop feature). It's compiled to `libsharekvm_android.so` and hands simple instructions to Kotlin over JNI.
- `android/` holds the Kotlin app. `ShareKvmService` (the Accessibility service) draws the pointer and highlight, performs gestures, types with `ACTION_SET_TEXT`, and navigates. `MainActivity` is settings for touch and D-pad. `Discovery` uses Android's NsdManager for mDNS.
- Desktop input types travel on the wire as rdev's enums. The Android build uses byte-identical copies ([`input.rs`](crates/sharekvm-core/src/input.rs)), checked by a test against rdev for every variant.

**Build:**

```bash
android/build-rust.sh release
```

```bash
cd android && ./gradlew assembleDebug
```

The first script needs the Android NDK, the second the Android SDK (`local.properties` points at it). `crates/sharekvm-android/examples/drive.rs` is a scripted stand-in for a desktop (`move`, `click`, `type`, `key`…), used to test on an emulator without a physical mouse.

**Limits:**

- Drags are performed when you release the button, not live.
- Receiving files and reading the Android clipboard aren't supported yet.
- Some apps that ignore Accessibility text input need the on-screen keyboard.

## Multiple monitors

Each computer reads its real monitor arrangement ([`screens.rs`](crates/sharekvm-core/src/screens.rs)): side by side, stacked, offset or L-shaped.

- **Your own monitors:** on the sharing computer, the cursor moves between its own monitors as usual. It only crosses to the other computer at the **outer** edge of the whole desktop on the chosen side, where there's no monitor beyond.
- **Arriving:** the cursor enters the other computer on whichever monitor is outermost on the facing side, then moves across that computer's monitors as a local mouse would.
- **Hand-over position:** the point where you cross travels as a fraction of each desktop's height (or width), so crossing near the top arrives near the top, even when the two arrangements differ.
- **Plugging monitors in or out** is picked up automatically, and the settings window draws your arrangement.

## Finding other computers

The sharing computer announces itself on the local network over mDNS/DNS-SD (the protocol behind Bonjour), as `_sharekvm._tcp`. On the other computer, **Be controlled** lists every ShareKVM computer it can see, tagged **Paired**, **New** (needs the code once) or **Different version**. Click **Connect** to use one.

- **Changing addresses:** the app remembers each server's device ID. If a router hands the server a new IP address, the client looks it up again by ID and reconnects on its own.
- **Trust:** discovery only finds computers. Nothing it reports is trusted until the encrypted pairing handshake succeeds.
- **Leaving the list:** stopping sharing sends an mDNS "goodbye", so the computer disappears from other lists straight away.
- **Command line:** `sharekvm discover` lists computers, and `sharekvm client <addr> --server-id <id>` turns on the address lookup.
- **Networks:** discovery needs both computers on the same network segment. Guest Wi-Fi and some office networks block mDNS; entering the address still works there. macOS asks once for *Local Network* access, and Windows may ask to allow ShareKVM through the firewall.

## File transfer

- **Drop on the window:** while connected, drop files or folders onto the ShareKVM window to send them to the other computer.
- **Drag across the edge:** drag files out of Finder past the screen edge and let go on the other computer. This works in both directions as long as the files come *from* a Mac, because macOS exposes the system drag pasteboard to other apps. Windows has no equivalent API, so a Windows PC can receive files dragged from a Mac but can't start an edge drag itself; use the drop zone instead.
- **Where files go:** received files are saved to `Downloads/ShareKVM`. Name clashes become `name (2).ext`, so nothing is overwritten, and Finder/Explorer opens on arrival (you can turn this off in the app).
- **Safety and speed:** every incoming name is sanitised and must stay inside that folder. Transfers share the encrypted connection but sit in a separate low-priority queue, in 32 KB chunks with a small socket buffer, so the mouse stays responsive. A cancelled or interrupted transfer deletes its partial files.

From the command line, `--commands` reads `{"cmd":"send_files","paths":[...]}` lines on stdin, and `--download-dir` changes where files are saved.

## Security

All traffic is encrypted ([`secure.rs`](crates/sharekvm-core/src/secure.rs)).

- **Pairing:** the two computers run a SPAKE2 password-authenticated key exchange keyed by the pairing code. The code is never sent. Recording a session doesn't help anyone guess it offline, and each connection attempt allows only one online guess.
- **Remembered computers:** after the first pairing, the server issues a random 256-bit key for that pair, sent over the encrypted channel. Both sides save it ([`trust.rs`](crates/sharekvm-core/src/trust.rs)), and later connections run the same SPAKE2 exchange with that key instead of the code, bound to both computers' device IDs. **Forget** in the app deletes the key. On the server it also generates a new pairing code, so the forgotten computer can't re-pair with the old one.
- **Lockout:** after 5 wrong codes or keys, the server refuses new pairing attempts for 60 seconds, and the app warns you about the attempts.
- **Traffic:** the shared secret is expanded with HKDF-SHA256 into a separate key for each direction. Every message is sealed with ChaCha20-Poly1305 using a counter nonce, so tampering, replay or reordering drops the connection.

A 6-digit code is fine for a home or office LAN. On a shared network, use a longer code.

```
crates/sharekvm-core   engine: protocol, server, client, clipboard, platform injection
crates/sharekvm-cli    `sharekvm` engine binary / command-line app
crates/sharekvm-android  Android session + JNI bridge (libsharekvm_android.so)
android/               Android app (Kotlin): phones, tablets, Android TV
app/src-tauri          ShareKVM desktop app (Tauri 2): settings, status, tray
app/ui                 the app's window (plain HTML/CSS/JS)
```

The desktop app runs the `sharekvm` binary as a child process with `--status-json` and reads status events from its stdout. It's a separate process because on macOS the input hook must own the main thread, which Tauri needs for its window.

## Build

```bash
cargo build --release
```

The binary is `target/release/sharekvm` (`sharekvm.exe` on Windows). Build on each OS you run it on.

## Desktop app

```bash
cargo build
./target/debug/sharekvm-app
```

ShareKVM runs in the background like a system utility:

- **Always on:** sharing starts as soon as ShareKVM runs, and ShareKVM opens at login. It starts hidden, with just an icon in the macOS menu bar or the Windows system tray.
- **Menu bar / tray:** shows the live status ("Connected to Office-PC"), a **Sharing** on/off switch, **Settings…** and **Quit**. On Windows, left-click the tray icon to open settings.
- **Settings window:** open it only to change something. Changes apply right away; the engine restarts with them and there's no Start button. Closing the window keeps everything running. On macOS the Dock icon is shown only while the window is open.
- **Opening ShareKVM again** (from Applications or the Start menu) brings up the running copy's settings, rather than starting a second copy.
- **Self-healing:** if the engine stops unexpectedly, it restarts after 5 seconds, backing off to once a minute. The first time, the window opens to explain (for example, a missing macOS permission). Once you grant it, the next retry just works. Problems only you can fix, such as a wrong pairing code, open the window and wait for you.

First-time setup:

1. On the computer with the mouse and keyboard, choose **Share my mouse & keyboard** and click the side the other computer is on.
2. On the other computer, choose **Be controlled**, click **Connect** next to the first computer, and enter its pairing code once.

Development builds (`target/debug`) don't register themselves as a login item unless you switch on **Open ShareKVM at login**. Release builds do so by default.

To ship a bundled app later, copy the engine to `app/src-tauri/binaries/sharekvm-<target-triple>`, add `"externalBin": ["binaries/sharekvm"]` under `bundle` in `tauri.conf.json`, and run `cargo tauri build`.

## Command line

On the computer with the mouse and keyboard (the client sits to its right):

```bash
sharekvm server --edge right --code 4821
```

On the other computer (the code is only needed the first time):

```bash
sharekvm client 192.168.1.20 --code 4821
```

Identity and saved keys are stored in `~/Library/Application Support/sharekvm` (macOS), `%APPDATA%\sharekvm` (Windows) or `~/.config/sharekvm` (Linux), or in `--data-dir`. The desktop app keeps its own copy in its config folder. Key files are readable by your user only.

Add `--swap-ctrl-meta` on the client to swap Ctrl and Cmd/Win, for example when a Mac controls a Windows PC.

**Emergency return:** press **Ctrl+Alt+Esc** to take control back. Control also returns automatically if the connection drops for more than about 7 seconds.

### macOS permissions

In System Settings → Privacy & Security, grant both **Accessibility** and **Input Monitoring** to the app that runs `sharekvm` (Terminal, iTerm, or later the ShareKVM app).

### Windows

Allow TCP port 24801 through Windows Defender Firewall on the server. Hooks cannot see elevated (admin) windows unless `sharekvm` also runs as administrator.

## Current limitations (MVP)

- One client per server.
- Cursor speed isn't adjusted for display scaling, so moving between a Retina Mac and a high-DPI Windows display can feel faster or slower on one side.
- Text clipboard only (files go through file transfer, not the clipboard).
- Edge drag-and-drop needs a Mac as the source (see File transfer).
- While the client is active, the server's cursor stays visible at the centre of the server's screen.

## Roadmap

1. Desktop app: launch at login, signed installers
2. Multiple clients (a computer on each side)
3. Edge drag from Windows (an OLE drop-target window at the screen edge)
4. Hide the server's cursor while remote
