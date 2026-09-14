# Linux packaging (tarball, .deb, .rpm, Arch PKGBUILD)

How the Linux release is built and installed. CI builds and publishes it from a tag
([ci-release.md](ci-release.md)); the scripts below are what CI runs and work by hand too.

## What ships
Every release carries four Linux assets. The tarball, `.deb` and `.rpm` come from one
`ubuntu-24.04` build; the Arch package is built from the same tag in an `archlinux` container.

| Asset | For | Installed by | Updated by |
|-------|-----|--------------|------------|
| `nullgate-<ver>-linux-x86_64.tar.gz` | any systemd distro | `install.sh` / `nullgatectl --install` into `/usr/local` | its own daily timer (`nullgatectl --update`) |
| `nullgate_<ver>-1_amd64.deb` | Debian 13+, Ubuntu 24.04+ | apt, from the repository or the file, into `/usr` | `apt upgrade` via apps.kznjk.com/deb |
| `nullgate-<ver>-1.x86_64.rpm` | Fedora 40+, openSUSE Tumbleweed | dnf/zypper, from the repository or the file, into `/usr` | `dnf upgrade` / `zypper update` via apps.kznjk.com/rpm |
| `nullgate-<ver>-1-x86_64.pkg.tar.zst` | Arch | pacman, from the `[kznjk]` repository | `pacman -Syu` via apps.kznjk.com/arch |

Plus the **AUR package** `nullgate`, a source build from `packaging/arch/PKGBUILD`, which AUR
helpers update like any other.

All of them contain the same three binaries (`nullgate`, `nullgate-daemon`, `nullgate-cli`),
`nullgatectl`, the daemon's systemd **system** unit, the app-menu `.desktop`, the tray agent's
autostart entry, AppStream metainfo and hicolor icons. The GUI links **system GTK** (GTK 4.10+,
libadwaita 1.4+), which the packages declare as dependencies and the tarball leaves to the user
(`sudo apt install libgtk-4-1 libadwaita-1-0`). The daemon and CLI don't link GTK, so a headless
tarball install needs none of it. Built on `ubuntu-24.04`, the GUI needs glibc 2.39 and the daemon
and CLI 2.34.

The daemon needs `CAP_NET_ADMIN`/`CAP_NET_RAW` to create the TUN, so every form installs it as a
**root systemd service**, mirroring the Windows LocalSystem service. The GUI runs as your normal
user and talks to it over `/tmp/nullgate.sock`.

## Build
Prerequisites: `cargo`, `tar`, and the GTK dev packages
(`sudo apt install libgtk-4-dev libadwaita-1-dev libdbus-1-dev pkg-config build-essential`). The
`.deb`/`.rpm` also need [nfpm](https://github.com/goreleaser/nfpm/releases) on `PATH` (CI pins the
version and its sha256 in `build.yml`).

```sh
scripts/package-linux.sh                 # cargo build --release, stage, tar
scripts/package-linux.sh --skip-build    # re-stage existing target/release bins
scripts/package-linux-native.sh          # .deb + .rpm from that staged tree
# -> dist/nullgate-<ver>-linux-x86_64.tar.gz, dist/nullgate_<ver>-1_amd64.deb,
#    dist/nullgate-<ver>-1.x86_64.rpm
```

`package-linux.sh` stages `dist/nullgate-<ver>-linux-x86_64/`; `package-linux-native.sh` copies
that tree to `dist/native/tree` (nfpm expands no variables in content paths, so
`packaging/linux/nfpm.yaml` reads a fixed path), rewrites the unit's `/usr/local/bin` to `/usr/bin`,
and runs nfpm twice. The unit is otherwise byte-identical to the tarball's.

## Tarball install (`nullgatectl`)
One-liner (downloads the latest release):
```sh
curl -fsSL https://raw.githubusercontent.com/steeb-k/nullgate/main/install.sh | sh
```
Or from the unpacked tarball: `./nullgatectl --install`. Either way `nullgatectl` uses `sudo` for the
privileged steps and:
- installs `nullgate`/`nullgate-daemon`/`nullgate-cli`/`nullgatectl` to `/usr/local/bin`,
- installs `/etc/systemd/system/nullgate-daemon.service` and enables + starts it,
- installs `nullgate-update.service` + `nullgate-update.timer` (daily auto-update) and enables the
  timer,
- installs the app-menu entry, metainfo and hicolor icons under `/usr/share`, and the tray-agent
  autostart entry (`nullgate --agent`, `NoDisplay`) in `/etc/xdg/autostart`,
- launches the tray agent in the invoking user's session right away on install/upgrade
  (`launch_agent_for_user`, best-effort via the user's `systemd --user`), so the tray appears
  without waiting for the next login.

Manage: `nullgatectl --status`, `nullgatectl --update [--check]`, `nullgatectl --uninstall [--purge]`.

**Auto-update.** `nullgate-update.timer` (system, daily, randomized) runs `nullgatectl --update` as
root: it compares `nullgate-daemon --version` to the latest tag of the public `steeb-k/nullgate`
repo, downloads the new tarball, atomically swaps the binaries, reloads systemd, and restarts the
daemon.

## Native packages (.deb, .rpm, AUR)
Two rules shape everything about them.

**1. A native package and a tarball install never coexist.** The tarball's binaries in
`/usr/local/bin` come first on `PATH`, its units in `/etc/systemd/system` shadow the packaged ones in
`/usr/lib/systemd/system`, and its timer would keep overwriting them. So the `.deb` `preinst` and
`.rpm` `%pre` **abort** when `/usr/local/bin/nullgate-daemon` exists and print
`sudo nullgatectl --uninstall`. pacman can't abort from a scriptlet, so the AUR package warns in
`pre_install` instead. Switching is lossless: both run the daemon as root with the same data
directory, and `--uninstall` without `--purge` keeps it.

**2. Native packages don't update themselves; the package manager does.** They ship no update
timer. `nullgatectl` is still installed (for `--status` and `--update --check`), but it detects a
package-managed install (`/usr/bin/nullgate-daemon` present, `/usr/local/bin/nullgate-daemon`
absent) and refuses `--install`, `--update` and `--uninstall`, naming the right package-manager
command. `install.sh` inherits that, because on an existing install it just calls `nullgatectl`.
Updates come from the package repositories below.

**Maintainer scripts** (`packaging/linux/scripts/*.sh`) are shared by both formats and tell them
apart by argument: dpkg passes words (`install`, `upgrade`, `configure <old-version>`, `remove`,
`purge`), rpm a count of installed instances.

| When | What happens |
|------|--------------|
| fresh install | `daemon-reload`, `enable --now nullgate-daemon` |
| upgrade / reinstall | `daemon-reload`, `try-restart` — a daemon the user stopped or disabled stays that way; the tray agent sees the daemon's new version and relaunches itself |
| removal | `disable --now`, kill running tray agents, `daemon-reload` |
| `.deb` purge | also removes `/var/log/nullgate` |

Every `systemctl` call is skipped when `/run/systemd/system` is absent (containers, chroots). The
autostart entry is a **config file** (dpkg conffile / rpm `%config(noreplace)` / pacman `backup`),
so `apt remove` leaves it behind; its `TryExec=nullgate` makes a leftover entry inert. dpkg also
reports a reinstall after `apt remove` as an upgrade even though the removal disabled the service,
so `postrm remove` leaves `/var/lib/nullgate/.package-removed` and `postinst` treats that as a fresh
install. Icon caches and the desktop database are refreshed by the distributions' own triggers.

Dependencies: the `.deb` names Debian/Ubuntu packages (`libc6 (>= 2.39)`, `libgtk-4-1 (>= 4.10)`,
`libadwaita-1-0 (>= 1.4)`, `libdbus-1-3`, `libgcc-s1`); the `.rpm` names **sonames**
(`libgtk-4.so.1()(64bit)`, …), because Fedora and openSUSE name the same libraries differently.

**Arch.** `packaging/arch/PKGBUILD` builds from GitHub's tag archive with `cargo fetch --locked`
then `--frozen` (the iroh fork is a git dependency pinned in `Cargo.lock`), runs `ipn-core`'s unit
tests in `check()`, and installs the same file set as `nfpm.yaml` — keep the two in step. Following
Arch convention it **doesn't enable** the service (`post_install` prints the command) and doesn't
restart it on upgrade (`post_upgrade` says to); it does `disable --now` in `pre_remove`, because
once the unit file is gone `systemctl` can no longer remove the enablement symlink.

The in-repo PKGBUILD carries `sha256sums=('SKIP')` and whatever `pkgver` it was last edited with.
CI overrides both (see below). The AUR copy gets the real values from:

```sh
git clone ssh://aur@aur.archlinux.org/nullgate.git ../nullgate-aur    # once
scripts/aur-prepare.sh <version> ../nullgate-aur [pkgrel]             # after the release is published
cd ../nullgate-aur && git diff && git commit -am "nullgate <version>" && git push
```

`aur-prepare.sh` sets `pkgver`/`pkgrel`, writes the tag archive's sha256, and regenerates
`.SRCINFO` with `makepkg`, running it in an `archlinux` container when the host has no `makepkg`.

## Package repositories (apps.kznjk.com)
Releases reach apt, dnf, zypper and pacman through signed repositories on **apps.kznjk.com**, the
same host as the flatpak repository. Nothing in this repo publishes to them: a systemd timer on that
host (`sync-packages.py` in `~/flatpak-repo`, documented in its README) polls this repo's
`releases/latest` every 10 minutes, downloads the `.deb`, `.rpm` and `.pkg.tar.zst` assets, checks
them against GitHub's sha256 digests, signs them and rebuilds the repositories. Prereleases are
ignored, and the newest three versions stay available for downgrades.

| Repo | URL | Client setup |
|------|-----|--------------|
| apt | `https://apps.kznjk.com/deb`, suite `stable`, component `main` | `/etc/apt/sources.list.d/kznjk.sources` + `/etc/apt/keyrings/kznjk-packages.asc` |
| dnf | `https://apps.kznjk.com/rpm/$basearch` | `/etc/yum.repos.d/kznjk.repo` |
| zypper | same repository | `/etc/zypp/repos.d/kznjk.repo` |
| pacman | `https://apps.kznjk.com/arch/$arch`, `[kznjk]` | by hand: `pacman-key --add` + `--lsign-key`, then a `[kznjk]` section |

All three are signed by **apps.kznjk.com Package Repositories**, fingerprint
`07E6 2212 5389 C2E1 94BA 7A5A 7F75 E425 2FDB 9F8D`. It is a separate key from the flatpak
repository's, and it has no passphrase so the timer can sign unattended: a signing *subkey* of the
flatpak key was rejected because gpg signs with the newest signing subkey when handed a primary
fingerprint, so it would have silently taken over flatpak signing too.

**Installing a downloaded package adds the repository**, like Chrome or VS Code do, so nobody is
stranded on the version they downloaded. The definitions live in `packaging/linux/repo/` and must
stay byte-identical to the copies the site serves (see that directory's README):
- **`.deb`**: `kznjk.sources` and the key are **conffiles**. `apt remove` keeps them (so the
  repository still verifies), `apt purge` deletes them, and deleting them by hand is a lasting
  opt-out because dpkg doesn't restore a removed conffile. A manual setup that wrote the same files
  is simply adopted.
- **`.rpm`**: dnf and zypper read different directories, and only one exists on a given system, so
  the package ships both definitions under `/usr/share/nullgate/repo/` and `postinstall` copies the
  right one on a **first install only**, never over an existing `kznjk.repo`. Erasing takes it back
  unless it was edited. Every upgrade leaves it alone, so deleting it is also a lasting opt-out.
  `kznjk.repo` sets `skip_if_unavailable=True`, so an outage of the host never breaks dnf.
- **Arch** gets no automatic setup: a package editing `pacman.conf` is not acceptable there.

## CI checks
The `linux` job in `build.yml` builds all three assets, then installs the `.deb` **on the runner**
(a real systemd host: the daemon must come up enabled and active with no restarts, `nullgatectl
--update` must refuse, and `apt remove` must stop it) and the `.rpm` in a **Fedora** container
(dependencies resolve, `rpm -V` clean, scriptlets tolerate no systemd). Both also check that the
repository definition was installed, and the rpm test that erasing takes it back. The `arch` job runs
`scripts/ci/arch-pkgbuild.sh` in an `archlinux` container against a `git archive` of the tagged
tree, which stands in for the not-yet-published tag archive: `makepkg`, `namcap`, `pacman -U`, the
`nullgatectl` refusal, `pacman -R`. It uploads the built package, which becomes the release's
`.pkg.tar.zst` asset. Both containers use rolling `latest` images on purpose, so a
distribution change breaks the build before it breaks users.

## Gotchas
- The GUI must **not** run as root (it loses your display); privilege lives in the daemon.
- `.gitattributes` keeps the shell scripts, units, `PKGBUILD` and `.install` LF so they survive a
  Windows checkout.
- Stale socket after an unclean stop: `sudo systemctl restart nullgate-daemon`.
