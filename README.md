# iroh-private-network (Nullgate)

Connect your own computers into a private network, wherever they are, so you can reach one
machine directly — Remote Desktop, SSH, file shares, a home server — **without routing all your
internet through a home VPN**.

A normal VPN sends everything through one chokepoint: you log in and your whole connection is
tunneled home, double-counting bandwidth and slowing the home network for everyone else. Nullgate
links your devices peer-to-peer instead, so only the traffic *between your devices* uses the
link. You reach a machine by a stable private address (e.g. `10.99.0.7`) with the RDP/SSH/etc.
client you already use.

It's like Hamachi / ZeroTier / Tailscale, but with **no accounts and no central server** — your
devices find and authenticate each other directly (built on [iroh](https://www.iroh.computer)).

> **Status:** 0.1.0, under active testing. Works on **Windows, Linux, and macOS** (Android
> planned). Grab an installer from the
> [Releases](https://github.com/steeb-k/iroh-private-network/releases) page.

## What it does
- **A private mesh of your devices.** Each gets a stable address on a `10.99.0.x` network.
- **Direct, encrypted connections.** Peer-to-peer with hole-punching; it only falls back to a
  relay if a direct path can't be made. All traffic is end-to-end encrypted.
- **Use the tools you already have.** Point RDP, SSH, SMB, a browser, etc. at a peer's address.
- **Simple, verified joining.** Create a network and share a ticket (text or QR). The two
  devices show a short **emoji code** you compare to confirm it's really them, then approve.
- **You stay in control.** Remove a device, freeze the network so no one new can join, or
  rotate its secret to reset access entirely. Removed devices drop off automatically.
- **Stays up to date.** A small background updater keeps every device on the latest release.
- **Nothing to sign up for, nothing to host.** No accounts, no coordinator server.

## Get it

**Windows** — download `nullgate-<version>-windows-x86_64.msi` from the
[Releases](https://github.com/steeb-k/iroh-private-network/releases) page and run it (it's
code-signed). It installs the app plus the background networking service and keeps itself
updated. Launch **Nullgate** from the Start menu — the desktop app is called
**Nullgate**.

**Linux & macOS** — one line in a terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/steeb-k/iroh-private-network/main/install.sh | sh
```

It downloads the right build, sets up the background service (you'll be asked for your password
once, because the service needs permission to create the virtual network interface), and enables
daily auto-updates. On **Linux** you also need the system GTK runtime:
`sudo apt install libgtk-4-1 libadwaita-1-0`. Afterwards, manage it with `nullgatectl`
(`nullgatectl --status`, `--update`, `--uninstall`). On **macOS** the app lands in `/Applications`.

## Using it
1. On one device: **+ → Create a network**, then share the ticket (copy it, or show the QR).
2. On the other: **+ → Join with a ticket** and paste it. Both screens show an emoji code.
3. Back on the first device, **Approve** if the emoji codes match. The new device appears in the
   member list with its private address.
4. Connect your normal client (RDP, SSH, …) to that address, e.g. `10.99.0.7`.

## Learn more
- [How it works](docs/architecture.md) — the design, components, and networking details.
- [Building from source](docs/building.md) — developer setup and packaging.
- [Security model](docs/security.md) — identity, verification, and revocation.

## Credits & inspiration
Built on a lot of other people's work:

- **[iroh](https://github.com/n0-computer/iroh)** and the n0 ecosystem
  ([iroh-docs](https://github.com/n0-computer/iroh-docs),
  [iroh-gossip](https://github.com/n0-computer/iroh-gossip),
  [iroh-blobs](https://github.com/n0-computer/iroh-blobs), iroh-tickets,
  [iroh-mdns-address-lookup](https://github.com/n0-computer/iroh)) by
  [n0](https://www.iroh.computer) — the peer-to-peer foundation Nullgate is built on.
- **[dumbpipe](https://github.com/n0-computer/dumbpipe)** (n0) — used to validate the approach
  early on.
- **[iroh-lan](https://github.com/rustonbsd/iroh-lan)** by rustonbsd — prior art for a virtual
  LAN over iroh.
- **[seed-sync-gtk](https://github.com/steeb-k/seed-sync-gtk)** — the cross-platform
  Rust + GTK structure and packaging this project mirrors.
- **[gtk-rs](https://gtk-rs.org)** / **[GTK](https://www.gtk.org)** /
  **[libadwaita](https://gnome.pages.gitlab.gnome.org/libadwaita/)** for the UI.
- **[tun-rs](https://crates.io/crates/tun-rs)** and **[Wintun](https://www.wintun.net)** for the
  virtual network interface.
- The emoji verification uses the
  [Matrix SAS](https://spec.matrix.org/latest/client-server-api/#sas-method-emoji) emoji set.
- Conceptual prior art: [ZeroTier](https://www.zerotier.com),
  [Tailscale](https://tailscale.com), and Hamachi.

## License
**GPL-3.0-or-later**, with one additional permission (GPLv3 §7): the program may be combined and
distributed with the proprietary **Wintun** prebuilt DLL (used only via its public API). See
[`LICENSE`](LICENSE) for the full text and the exact exception.

Wintun (`wintun.dll`, bundled in Windows builds) is **not** covered by the GPL — it's licensed
separately by WireGuard LLC and shipped with its own `wintun-LICENSE.txt`. Linux uses the
kernel's built-in TUN and bundles no third-party driver.
