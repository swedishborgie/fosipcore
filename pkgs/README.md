# fosipcore — Package Build & Installation

Prebuilt packages are published on the [releases page](https://github.com/swedishborgie/fosipcore/releases) and built by GitHub Actions (`.github/workflows/packages.yml`) — pushing a `v*` tag builds both packages and attaches them to the release. No Docker or local packaging toolchain is required.

## Versioning

The **git tag is the source of truth for release versions**. Tagging `v0.2.0` produces `fosipcore 0.2.0` packages and a binary that reports `v0.2.0` at startup:

- `build.rs` reads `FOSIPCORE_VERSION` (set by CI from the tag, `v` stripped) and bakes it into the binary; locally it falls back to the version in `Cargo.toml`.
- CI passes the same version to `cargo deb --deb-version` and `cargo generate-rpm --set-metadata`, so package metadata matches the tag.
- Non-tag builds (main branch, manual dispatch) fall back to the version in `Cargo.toml`.

When cutting a release, bump `Cargo.toml` to the new version **and** push the matching `v*` tag, so local and tagged builds agree. (Note: `DEFAULT_SERVICE_VERSION` in `main.rs` is *not* this — it's the camera-protocol service version the web UI expects and must stay pinned to the camera's value.)

---

## Installation

### Ubuntu / Debian (.deb)

```bash
sudo apt install ./fosipcore_*_amd64.deb
```

### Fedora / RHEL (.rpm)

```bash
sudo dnf install ./fosipcore-*.rpm
```

Both packages install:

| Path | Purpose |
|------|---------|
| `/usr/bin/fosipcore` | The binary |
| `/lib/systemd/system/fosipcore@.service` | Per-user systemd **template** unit |
| `/usr/share/fosipcore/example.env` | Example environment config |
| `/etc/fosipcore/example.env` | Copy of the example, created by the post-install script |

## Activating for your user

The service is a template unit: one instance per user, running as that user.

```bash
sudo systemctl enable --now fosipcore@<username>
systemctl status fosipcore@<username>
journalctl -u fosipcore@<username> -f
```

## Configuration

### System-managed config (admin)

```bash
sudo cp /etc/fosipcore/example.env /etc/fosipcore/<username>.env
sudo nano /etc/fosipcore/<username>.env
```

### Per-user overrides

```bash
mkdir -p ~/.config/fosipcore
cp /etc/fosipcore/example.env ~/.config/fosipcore/env
nano ~/.config/fosipcore/env
```

`~/.config/fosipcore/env` is loaded **after** `/etc/fosipcore/<username>.env`, so per-user values take precedence.

> Note: multiple instances on the same host must use distinct `FOSIPCORE_SERVICE_MANAGER_PORT` / `FOSIPCORE_LIVE_PORT_BASE` ranges, since each instance binds fixed loopback ports.

---

## Building the packages locally

Requires: a stable Rust toolchain (plus a C compiler — rustls's `ring` backend compiles a small C shim) and the two packaging tools:

```bash
cargo install cargo-deb cargo-generate-rpm --locked

cargo build --release
cargo deb --no-build          # → target/debian/fosipcore_<version>_<arch>.deb
cargo generate-rpm            # → target/generate-rpm/fosipcore-<version>-1.<arch>.rpm
```

Package metadata (file list, maintainer, post-install scripts) lives in
`[package.metadata.deb]` / `[package.metadata.generate-rpm]` in `Cargo.toml`
and `pkgs/debian/postinst`.

### Nix flake

The repo also ships a flake (`flake.nix` → `pkgs/nix/fosipcore.nix`,
`rustPlatform.buildRustPackage`) that installs the **same file layout** as
the .deb / .rpm:

| Path | Purpose |
|------|---------|
| `<out>/bin/fosipcore` | The binary |
| `<out>/lib/systemd/system/fosipcore@.service` | Per-user systemd template unit |
| `<out>/share/fosipcore/example.env` | Example environment config |

```bash
# Build
nix build .#fosipcore

# Install
nix profile install github:swedishborgie/fosipcore

# Run without installing
nix run github:swedishborgie/fosipcore
```

Notes:

- The package version comes from `Cargo.toml` (same source the binary bakes in
  via `build.rs`), so flake and tag-driven versions agree when you bump
  `Cargo.toml` before tagging.
- The post-install step that creates `/etc/fosipcore/` (as the deb/rpm do)
is skipped when `/etc` is not writable (e.g. sandboxed builds); the unit's
  `EnvironmentFile=-` lines tolerate the directory being absent.
- On NixOS, prefer managing the template unit through a NixOS module
  (`systemd.services."fosipcore@<user>"`) rather than the ad-hoc
  `/etc/fosipcore/` directory.
