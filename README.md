<p align="center">
  <img src="img/nullgate-header.png" alt="Nullgate" />
</p>

## An iroh-based peer-to-peer virtual LAN

Connect your own computers into a private network, wherever they are, so you can reach one
machine directly — take your homelab anywhere without exposing it to the internet with an 
end-to-end encrypted virtual network.

It's like Hamachi / ZeroTier / Tailscale, but with **no accounts and no central server** — your
devices find and authenticate each other directly (built on [iroh](https://www.iroh.computer)).

> **Status:** 0.2.0, under active testing. Works on **Windows, Linux, macOS, and Android**
> (the Android app is Kotlin/Compose and routes traffic through Android's built-in VPN). Grab an
> installer or the APK from the
> [Releases](https://github.com/steeb-k/iroh-private-network/releases) page (Android build details:
> `docs/android-packaging.md`).

## What it does
- **A private mesh of your devices.** Members get a static IP on an overlay network that routes
  through the best available connection -- including local networks.
- **Direct, encrypted connections.** Peer-to-peer with hole-punching; it only falls back to a
  relay if a direct path can't be made. All traffic is end-to-end encrypted.
- **Use the tools you already have.** Point RDP, SSH, file shares, media servers, as if every
  device is in the same room.
- **Simple, verified joining.** Create a network and share a ticket (text or QR). The two
  devices show a short **emoji code** you compare to confirm it's really them, then approve.
- **Three levels of access.** Devices are **Peers** (use the network, view the activity log),
  **Controllers** (also add/remove Peers and hand out Peer invites), or run by the **Originator**
  (full control, including kicking members). Share a Peer or Controller ticket depending on how
  much you want to delegate; optionally, tickets can be single-use to negate any attempts at abuse.
- **A built-in activity log.** Every administrative change — who added or removed whom, role
  changes, renames — is recorded with the time and the person who did it, kept for 30 days, and
  visible to everyone on the network.
- **Per-device privacy switches.** On your own device you can **disable remote access** (you can
  still reach others, but no one can reach you) or **hide from the member list**.
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
daily auto-updates. On **Linux** you also need the system GTK runtime for the GUI application:
`sudo apt install libgtk-4-1 libadwaita-1-0`. Afterwards, manage it with `nullgatectl`
(`nullgatectl --status`, `--update`, `--uninstall`). On **macOS** the app lands in `/Applications`.

## Using it
1. On one device: **+ → Create a network**, then share the ticket (copy it, or show the QR).
2. On the other: **+ → Join with a ticket** and paste it. Both screens show an emoji code.
3. Back on the first device, **Approve** if the emoji codes match. The new device appears in the
   member list with its private address.
4. Connect your normal client (RDP, SSH, …) to that address, e.g. `10.99.0.7`.

The background service keeps running and starts with your device. If it ever stops unexpectedly
it restarts itself automatically, and it keeps a log (including the reason for any crash) under
`%ProgramData%\Nullgate\logs` on Windows, `/var/log/nullgate` on Linux, and `/Library/Logs/Nullgate`
on macOS — handy if you ever need to report a problem.

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
