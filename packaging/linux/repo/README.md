# apps.kznjk.com package repository definitions

Shipped inside the .deb and .rpm so an install from a downloaded file keeps
updating through apt, dnf or zypper. They must stay byte-identical to the copies
the site serves (`~/flatpak-repo/packages/static/` on the host), which are what
the manual setup instructions install: dpkg then treats an existing manual setup
as the same config file instead of prompting about it.

| File | Installed as |
|------|--------------|
| `kznjk.sources` | `/etc/apt/sources.list.d/kznjk.sources` (.deb conffile) |
| `kznjk-packages.asc` | `/etc/apt/keyrings/kznjk-packages.asc` (.deb conffile) |
| `kznjk.repo` | copied to `/etc/yum.repos.d/kznjk.repo` by the .rpm on first install |
| `kznjk-zypper.repo` | copied to `/etc/zypp/repos.d/kznjk.repo` by the .rpm on first install |

The key is `apps.kznjk.com Package Repositories <admin@kznjk.com>`,
fingerprint `07E6 2212 5389 C2E1 94BA 7A5A 7F75 E425 2FDB 9F8D`. It is not the
flatpak repository's key.
