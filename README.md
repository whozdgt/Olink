<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="src/assets/olink.svg">
    <img src="src/assets/olink-classic.svg" width="112" alt="Olink">
  </picture>
</p>

<h1 align="center">Olink</h1>

<p align="center">
  <a href="https://github.com/whozdgt/Olink/actions/workflows/build.yml"><img src="https://github.com/whozdgt/Olink/actions/workflows/build.yml/badge.svg" alt="Build"></a>
  <img src="https://img.shields.io/badge/ColorOS-17%20compatible-1f8f4e" alt="ColorOS 17 compatible">
</p>

**Your OPPO, OnePlus or realme phone on Linux.** Olink speaks the phone's own
PC-connection protocol ("O+ Connect" / Device connect → PC in ColorOS), so the
phone connects **directly to Linux** over Wi-Fi: no OPPO PC app, no cloud, no
account.

**ColorOS 17 compatible** (and still works on ColorOS 16). Built and tested on
an OPPO Find X9 Ultra with Arch / CachyOS and Hyprland.

- Pair by scanning a QR code, then allow the phone on the PC; afterwards the
  phone reconnects by itself.
- Clipboard both ways (password-manager copies are never sent).
- Files and folders both ways: **Send files…**, **Send folder…**, drag and
  drop, or share to the PC from the phone.
- Phone notifications as desktop pop-ups, battery level.
- The phone screen in a window you control with mouse, trackpad and keyboard,
  or just one app while the phone stays free to use.
- Runs in the tray and starts with your login, using ~35 MB in the
  background: the window (a web view) only exists while it's open.

<p align="center">
  <img src="docs/screenshots/midnight.png" width="49%" alt="Olink in the Midnight theme: black, with every word in green">
  <img src="docs/screenshots/classic.png" width="49%" alt="Olink in the Classic theme: light, with square corners">
</p>

## Install

### Arch / CachyOS

```bash
git clone https://github.com/whozdgt/Olink && cd Olink/packaging/arch
makepkg -si
```

Uninstall with `sudo pacman -R olink`.

### AppImage (any distribution)

```bash
npm install
scripts/build-appimage.sh
# → dist/Olink-<version>-x86_64.AppImage  (needs patchelf)
```

The script keeps your home path (and so your username) out of the binary.

Runtime needs: WebKitGTK 4.1, GTK 3, libayatana-appindicator, GStreamer with
the good/bad plugins, libnice and gst-libav (phone screen), wl-clipboard,
libnotify, xdg-desktop-portal.

### Firewall

The phone connects in to TCP 10150-10152, 46888, 46890 and 46892, and the
phone screen uses UDP 46900-46999 ([what each port is for](ARCHITECTURE.md#ports)).
With ufw (the package installs a profile):

```bash
sudo ufw allow Olink
```

## Using it

Open **Olink**, then on the phone go to **Settings → Device connect → PC**
and scan the QR code. Olink asks whether to allow the phone; only allow it if
you just scanned the code.

Closing the window keeps Olink in the tray (the phone stays connected);
**Quit** in the tray menu ends it. Settings (⚙): screen quality, swipe or
scroll for the touchpad, notifications, clipboard sharing, start on login.

## Themes

Olink follows your desktop's light or dark setting, using the two themes of
its sister app [Streamline](https://github.com/whozdgt/streamline):
**Midnight** when dark (black, every word in the accent green, rounded
shapes) and **Classic** when light (square corners, black text). The icon
has a version for each. To keep Midnight all the time, see the note at the
top of `src/styles.css`.

## Security

The QR code carries no secret, so pairing always needs your OK on the PC.
After that the link uses the phone's own encryption (UKey2 pairing, AES-GCM),
replays are refused, and nothing the phone sends is ever run or shown as
HTML. See the security notes in [PROGRESS.md](PROGRESS.md) for what was
reviewed, what was fixed and the protocol's own limits.

## Development

```bash
npm install
npm run tauri dev                  # run from source
cd src-tauri && cargo test         # unit tests
cargo test -- --ignored live_      # rogue-pairing test against the running app
OPLUS_DEBUG=1 npm run tauri dev    # also log raw protocol dumps
```

`olink net-probe pair` runs the phone server in a terminal (the QR is written
to `$XDG_RUNTIME_DIR/olink/pairing-qr.svg`). UI work: see [UI.md](UI.md).

## Docs

- **[ARCHITECTURE.md](ARCHITECTURE.md)**: how a connection comes up, the code map, the ports.
- **[PROGRESS.md](PROGRESS.md)**: what works, how, and the security review.
- **[SCP.md](SCP.md)** and **[CRYPTO.md](CRYPTO.md)**: pairing, the secure channel and its crypto.
- **[CAST.md](CAST.md)**: the phone screen (ODMF signalling, WebRTC, UIBC input).
- **[PROTOCOL.md](PROTOCOL.md)**: file transfer.
- **[PAIRING.md](PAIRING.md)**: the research log, from the Windows app's logs to the phone's own code.
- **[UI.md](UI.md)**: the window's page, commands and events.

## Licence

MIT, see [LICENSE](LICENSE).

## Legal

Reverse-engineering for interoperability is broadly permitted (EU Software
Directive; UK CDPA s.50B; US DMCA §1201(f)). This is a clean-room re-implementation
from observed behaviour and public standards. Do not redistribute OPPO's binaries,
code, bundled certificate, or trademarks/assets. Olink implements Google's open
UKey2 protocol itself, in Rust; it doesn't include Google's code. Not legal advice.

OPPO, OnePlus, realme, ColorOS and O+ Connect are trademarks of their owners.
Olink is an independent project and isn't affiliated with or endorsed by them.
