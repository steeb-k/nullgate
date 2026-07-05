# Linux packaging (tarball + system service + auto-updater)

How the Linux release is built and installed. Builds are **local** (the maintainer runs this in
WSL Ubuntu or on a Linux box).

## What ships
`nullgate-<version>-linux-x86_64.tar.gz` — the binaries (`nullgate`, `ipn-daemon`, `ipn-cli`), the
`nullgatectl` install manager, the systemd **system** units, an app-menu `.desktop`, and pre-rendered
hicolor icons. The desktop GUI relies on **system GTK** on the target (not bundled):
`sudo apt install libgtk-4-1 libadwaita-1-0`. This is **GUI-only** — the daemon and
`nullgate-cli` don't link GTK, so a headless install needs none of it (`nullgatectl --install`
prints a note about the missing libs but completes anyway).

Unlike a pure user app, Nullgate's daemon needs `CAP_NET_ADMIN`/`CAP_NET_RAW` to create the TUN, so
`nullgatectl --install` sets it up as a **root systemd service** (it gets the caps for free), with a
root daily update timer — mirroring the Windows LocalSystem service + SYSTEM task. The GUI runs
as your normal user and talks to the daemon over `/tmp/nullgate.sock`.

## Prerequisites
- Build: `cargo`, `tar`, **ImageMagick** (`magick` or `convert`) for icon sizes, and the GTK dev
  packages: `sudo apt install libgtk-4-dev libadwaita-1-dev pkg-config build-essential`.
- Target runtime: `libgtk-4-1 libadwaita-1-0` (GTK 4.10+ / libadwaita 1.4+).

## Build
```sh
scripts/package-linux.sh                 # cargo build --release, then package
scripts/package-linux.sh --skip-build    # repackage existing target/release bins
# -> dist/nullgate-<version>-linux-x86_64.tar.gz
```

## Install / manage (on the target)
One-liner (downloads the latest release):
```sh
curl -fsSL https://raw.githubusercontent.com/steeb-k/nullgate/main/install.sh | sh
```
Or from the unpacked tarball: `./nullgatectl --install`. Either way `nullgatectl` uses `sudo` for the
privileged steps and:
- installs `nullgate`/`ipn-daemon`/`ipn-cli`/`nullgatectl` to `/usr/local/bin`,
- installs `/etc/systemd/system/ipn-daemon.service` (root; `CAP_NET_ADMIN`) and enables+starts it,
- installs `ipn-update.service` + `ipn-update.timer` (daily auto-update) and enables the timer,
- installs the app-menu entry + hicolor icons, and a tray-agent autostart (`nullgate --agent`,
  `NoDisplay`) in `/etc/xdg/autostart`.

Manage: `nullgatectl --status`, `nullgatectl --update [--check]`, `nullgatectl --uninstall [--purge]`.

## Auto-update
`ipn-update.timer` (system, daily, randomized) runs `nullgatectl --update` as root: it compares
`ipn-daemon --version` to the latest tag of the public `steeb-k/nullgate` repo,
downloads the new tarball, atomically swaps the binaries, reloads systemd, and restarts the
daemon.

## Gotchas
- The GUI must **not** run as root (it loses your display); privilege lives in the daemon.
- `.gitattributes` keeps the shell scripts/units LF so they survive a Windows checkout.
- Stale socket after an unclean stop: `sudo systemctl restart ipn-daemon`.
