# Nix package for fosipcore.
#
# Builds the Rust binary with rustPlatform and installs the same file layout
# as the .deb / .rpm packages (see pkgs/README.md):
#
#   $out/bin/fosipcore
#   $out/lib/systemd/system/fosipcore@.service
#   $out/share/fosipcore/example.env
#
# The version is read from Cargo.toml, which build.rs picks up as
# FOSIPCORE_VERSION's fallback (CARGO_PKG_VERSION), so the binary's reported
# version matches the package metadata with no extra plumbing.
#
# The function takes a set-pattern argument (not a bare `pkgs:`) so both
# calling styles work:
#   - `pkgs.callPackage ./fosipcore.nix {}`  (the NixOS module) — Nix's
#     callPackage can only introspect set-pattern arguments, so a curried
#     `pkgs:` lambda would be called with an empty set and break.
#   - `import ./fosipcore.nix pkgs`          (the flake's packages output)
#
# `installSystemdUnit` makes the bundled fosipcore@.service optional for
# installs that don't use systemd.

{
  rustPlatform,
  lib,
  stdenv,
  installSystemdUnit ? true,
  ...
}:

let
  root = ../..;
  cargoToml = builtins.fromTOML (builtins.readFile (root + "/Cargo.toml"));
in
rustPlatform.buildRustPackage {
  pname = "fosipcore";
  version = cargoToml.package.version;

  # Only the files the build actually needs (excludes local target/, .git, ...).
  src = lib.fileset.toSource {
    root = root;
    fileset = lib.fileset.unions [
      (lib.fileset.fileFilter (
        file:
        builtins.elem file.name [
          "Cargo.toml"
          "Cargo.lock"
          "build.rs"
          "LICENSE"
        ]
      ) root)
      (lib.fileset.fileFilter (file: file.type == "regular") (root + "/src"))
    ];
  };

  cargoLock = {
    lockFile = root + "/Cargo.lock";
  };

  # buildPhase (from rustPlatform) already ran `cargo build --release`;
  # override installPhase to place the extra assets alongside the binary.
  installPhase = ''
    runHook preInstall
    # cargoBuildHook passes --target explicitly, so artifacts land in a
    # triple-stamped subdirectory of the target root.
    install -Dm755 target/${stdenv.hostPlatform.config}/release/fosipcore $out/bin/fosipcore
    ${lib.optionalString installSystemdUnit ''
    install -Dm644 ${root}/pkgs/fosipcore@.service $out/lib/systemd/system/fosipcore@.service
    ''}
    install -Dm644 ${root}/pkgs/example.env $out/share/fosipcore/example.env

    # Mirror the deb/rpm post-install: create /etc/fosipcore with a copy of
    # the example config. Skipped when /etc is not writable (e.g. sandboxed
    # builds on NixOS) — the systemd unit's EnvironmentFile=- lines already
    # tolerate the directory being absent.
    if [ -w /etc ]; then
      install -d /etc/fosipcore
      if [ ! -f /etc/fosipcore/example.env ]; then
        cp $out/share/fosipcore/example.env /etc/fosipcore/example.env
        chmod 644 /etc/fosipcore/example.env
      fi
    fi

    runHook postInstall
  '';

  # Unit tests are not run in the Nix build: two of them resolve a fake
  # hostname ("camera.lan") whose behavior differs under the Nix sandbox,
  # so doCheck is flaky here. Run `cargo test` locally for full coverage.
  doCheck = false;
  doInstallCheck = false; # the binary binds fixed loopback ports

  meta = with lib; {
    description = "WebSocket-to-RTSP proxy for legacy Foscam IP camera web UIs";
    homepage = "https://github.com/swedishborgie/fosipcore";
    license = licenses.mit;
    mainProgram = "fosipcore";
    platforms = platforms.linux;
  };
}
