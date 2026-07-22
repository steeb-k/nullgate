<p align="center">
  <img src="img/nullgate-header.png" alt="Nullgate" />
</p>

## Serverless Peer-to-Peer Mesh Network

Connect your own computers into a private network, wherever they are, so you can reach one
machine directly — take your homelab anywhere without exposing it to the internet with an 
end-to-end encrypted virtual network.

It's like Hamachi / ZeroTier / Tailscale, but with **no accounts and no central server** — your
devices find and authenticate each other directly (built on [iroh](https://www.iroh.computer)).

> **Status:** Under active testing. Works on **Windows, Linux, macOS, and Android**
> (the Android app is Kotlin/Compose and routes traffic through Android's built-in VPN). Grab an
> installer or the APK from the
> [Releases](https://github.com/steeb-k/nullgate/releases) page (Android build details:
> `docs/android-packaging.md`). On Android you can also add this repo to
> [Obtainium](https://github.com/ImranR98/Obtainium) (source URL
> `https://github.com/steeb-k/nullgate`) to install and auto-update the app straight from GitHub
> releases.

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
- **Action buttons.** Give a device a button — "RDP", "SSH", "Web" — in the colour of your
  choosing, and it appears on that device's row and in the tray menu. Clicking it runs the command
  you picked, with the device's address filled in for you, optionally in a terminal window. Set up
  per machine, so the command can be whatever suits the computer you're sitting at.
- **Sleeps when your laptop does.** A device that goes to sleep leaves the network on the way down
  and rejoins when you wake it, so it won't keep announcing itself while it's shut in your bag.
- **You stay in control.** Remove a device, freeze the network so no one new can join, or
  rotate its secret to reset access entirely. Removed devices drop off automatically.
- **Stays up to date.** A small background updater keeps every device on the latest release.
- **Nothing to sign up for, nothing to host.** No accounts, no coordinator server.

## Get it

**Windows** — download the `.msi` for your PC from the
[Releases](https://github.com/steeb-k/nullgate/releases) page and run it (it's
code-signed). Most PCs want `nullgate-<version>-windows-x86_64.msi`; if yours has a
Snapdragon or other ARM chip, take `nullgate-<version>-windows-arm64.msi` instead
(Settings → System → About → *System type* tells you which). It installs the app plus the
background networking service and keeps itself updated. Launch **Nullgate** from the Start
menu — the desktop app is called **Nullgate**.

**Linux & macOS** — one line in a terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/steeb-k/nullgate/main/install.sh | sh
```

It downloads the right build, sets up the background service (you'll be asked for your password
once, because the service needs permission to create the virtual network interface), and enables
daily auto-updates. On **Linux**, the desktop app needs the system GTK runtime
(`sudo apt install libgtk-4-1 libadwaita-1-0`) — but that's only for the GUI; on a headless
box you can skip it and drive everything with `nullgate-cli` (see "Headless / CLI" below).
Afterwards, manage it with `nullgatectl` (`nullgatectl --status`, `--update`, `--uninstall`).
On **macOS** the app lands in `/Applications`.

## Using it
1. On one device: **+ → Create a network**, then share the ticket (copy it, or show the QR).
2. On the other: **+ → Join with a ticket** and paste it. Both screens show an emoji code.
3. Back on the first device, a banner across the top says a device wants to join — click **Review**
   to open the approval screen, then **Approve** if the emoji codes match. The new device appears in
   the member list with its private address.
4. Connect your normal client (RDP, SSH, …) to that address, e.g. `10.99.0.7`.

If the background service is ever stopped or not carrying traffic, the app shows a banner across the
top with a **Start service** / **Restart service** button — clicking it prompts for your admin
password and starts the service for you (no need to open a terminal).

Nullgate lives in your **system tray**. Closing the app window just closes the window — the network
stays up and the tray icon stays put, so **closing Nullgate never disconnects you**. Click the tray
icon (or **Open Nullgate**) to bring the window back; notifications open it too. The tray menu also
has **Restart Nullgate daemon** if the background service ever needs a nudge, and **Quit Nullgate**
to disconnect and close it entirely. The tray runs from a tiny helper that starts with your login,
so it's there whether or not the window is open.

**Giving a device an action button:** open a device from the member list and choose **Action
button**. Give it a short label ("RDP"), pick one of eight colours, and type the command to run —
for example `mstsc /v:{ip}` on Windows, or `ssh me@{ip}` elsewhere. `{ip}` is filled in with that
device's Nullgate address when you click; `{name}`, `{hostname}` and `{node_id}` work too.

Tick **Open in a terminal window** for anything that needs a console — `ssh`, a script, a
command whose output you want to read. Without it the command runs on its own, with no window,
which is what a graphical program like Remote Desktop wants. On Windows the terminal is a normal
console window; on Linux it's whichever terminal you have installed (or `$TERMINAL`); on macOS it's
Terminal.app. Either way the window belongs to the command, and closes when the command finishes.

The button then appears on that device's row, and in the tray menu as "*device* (*label*)", so you
can reach the machine without opening the window at all. The command runs as a program with its
arguments — not through a shell — so pipes and `&&` don't apply; put double quotes around anything
containing spaces. It stays clickable when the device is offline (just dimmed), since only you know
whether your command needs the other end awake.

Action buttons are **set up per machine and never shared**: the command that reaches a device from
your Windows desktop isn't the one that reaches it from your Mac, so each computer keeps its own.
They live in a plain `actions.json` in Nullgate's config folder if you'd rather edit them by hand.

**Using your own relay server:** when two devices can't connect directly, traffic is carried by a
relay — normally the free public ones run by the iroh project. If you host your own iroh relay, open
**Relay servers** (on the main screen, or from the "No network yet" page before you join — on the
phone it's in the ⋮ menu) and add its address, plus its access token if it requires one. Your relay
then carries the traffic, while the public relays stay available as a backup, so a device that
*doesn't* have your relay can still reach you.

Nullgate connects to the relay to check it — token and all — before saving it, and tells you inside
the dialog if the relay won't have it. You can re-test a relay you've already added at any time with
the ⇄ button beside it: a relay that used to work can start turning you away (a token gets rotated,
the server gets redeployed), and otherwise the way you'd find out is that other devices quietly stop
seeing this one.

**Put the same relay — same address, same token — on every device.** This is the one setting that
can cut your network in half. Each device is only configured locally; adding a relay here does not
add it for anyone else. If a relay needs a token, it turns away devices that don't have it, and a
device it turns away can't see the ones that use it. Either set it everywhere, or nowhere.

If you'd rather *never* touch the public relays, switch the policy to **my relays only** — then
devices without your relay genuinely cannot reach this one, which is the point. Changes are saved
immediately and applied to the running service; the app tells you if it couldn't apply one.

The background service keeps running and starts with your device. If it ever stops unexpectedly —
or if its memory use climbs too high (a safeguard against a leak in the underlying networking
library) — it restarts itself automatically, and it keeps a log (including the reason for any crash
or restart) under `%ProgramData%\Nullgate\logs` on Windows, `/var/log/nullgate` on Linux, and
`/Library/Logs/Nullgate` on macOS — handy if you ever need to report a problem. A device that comes
back within a couple of minutes of one of these quick restarts won't spam everyone with a "came
online" notification.

**On the phone**, Nullgate eases off its background housekeeping when the app isn't on screen — it
checks in far less often with the screen off — so it stays connected without draining the battery,
then picks the pace back up the moment you open it. It also follows your network: switch between
Wi‑Fi and mobile data, or turn another VPN on and then off, and Nullgate reconnects on its own. When
another VPN takes over, Nullgate steps aside (it can't route while another VPN owns the connection)
and comes back automatically once that VPN is switched off — you shouldn't need to toggle anything.

### Headless / CLI
No GUI? Drive the same daemon with `nullgate-cli` — GTK isn't
needed. The verification code is shown as **words** instead of emojis, so you can read it aloud or
paste it over chat and compare it exactly.

```sh
nullgate-cli status                 # network + members
nullgate-cli create "my network"    # become the originator; prints a ticket
nullgate-cli join <ticket>          # prints your verification words, waits for approval
nullgate-cli watch                  # in another shell: shows join requests + their words
nullgate-cli approve <node-id>      # approve once the words match; deny with `deny`
```

To add a device to a network: run `nullgate-cli watch` on an existing member, `nullgate-cli join
<ticket>` on the new one, check the word lists match on both, then `nullgate-cli approve <node-id>`.

Custom relay servers work here too:

```sh
nullgate-cli relay add https://relay.example.com:8443
nullgate-cli relay mode only        # never use the public relays (`preferred` is the default)
nullgate-cli relay show             # what's configured, and whether it's applied
nullgate-cli relay clear            # back to the public relays
```

`relay add` **asks** for the access token rather than taking it on the command line, so it never
lands in your shell history or in the list of running processes that everyone else on the machine
can read. Type it in (nothing is echoed), or press Enter if your relay doesn't use one. If you do
give a token, Nullgate tries it against the relay before saving it, and asks again if the relay
won't take it — a token the relay rejects would quietly cut this device off from the network.

Each command reports whether the change reached the running daemon, so "saved" and "applied" are
never confused. Run the same commands on **every** device — relay settings are per-device.

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
