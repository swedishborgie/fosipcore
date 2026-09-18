# NixOS module for fosipcore.
#
# Usage (in your system configuration):
#
#   { inputs, ... }:
#   {
#     inputs.fosipcore.flake = true;
#
#     nixosConfigurations.yoursystem = nixpkgs.lib.nixosSystem {
#       ...
#       modules = [ inputs.fosipcore.nixosModules.default ];
#     };
#
#     # In configuration.nix (or wherever you set options):
#     services.fosipcore = {
#       enable = true;
#       users.<username>.enable = true;
#     };
#   }
#
# This reuses the package's fosipcore@.service template unit (User=%i,
# WorkingDirectory=/home/%i, EnvironmentFile handling, restart policy —
# single source of truth) and only replaces its ExecStart, which points
# at /usr/bin/fosipcore and therefore cannot work on NixOS. The override
# uses systemd's "asDropin" strategy, i.e. it is emitted as a drop-in
# next to the instance name, extending the packaged template unit
# instead of shadowing it.
#
# The systemd integration is entirely opt-in: enabling no users installs
# only the package (binary on the default PATH) and creates no units. For
# each enabled user, `autostart` (default true) controls whether the
# instance is wanted by multi-user.target; set it to false to install the
# unit without starting it at boot (`systemctl start fosipcore@<name>`
# still works).
#
# Per-instance configuration lives in ~/.config/fosipcore/env
# (see pkgs/example.env). Note: each instance binds fixed loopback ports
# by default (50000, 20000-26000); run multiple instances on one host
# only with distinct FOSIPCORE_SERVICE_MANAGER_PORT /
# FOSIPCORE_LIVE_PORT_BASE values in their env files.

{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.fosipcore;
  enabledUsers = lib.filterAttrs (_: userCfg: userCfg.enable) cfg.users;
  anyEnabled = enabledUsers != {};
in
{
  options.services.fosipcore = {
    enable = lib.mkEnableOption "the fosipcore package (binary on the default PATH; per-user systemd instances are opt-in via users.<name>.enable)";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ../pkgs/nix/fosipcore.nix { };
      description = ''
        The fosipcore package to use. Defaults to the one built from this
        repository's sources, using the consuming system's nixpkgs (so the
        binary matches the system's glibc and channel).
      '';
    };

    users = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            enable = lib.mkEnableOption "the per-user fosipcore instance (fosipcore@<username>)";
            autostart = lib.mkOption {
              type = lib.types.bool;
              default = true;
              description = ''
                Start the instance automatically at boot (wanted by
                multi-user.target). When false, the unit is still installed
                but not started at boot; start it manually with
                `systemctl start fosipcore@<username>`.
              '';
            };
          };
        }
      );
      default = { };
      description = ''
        Which per-user fosipcore instances to run. Each enabled username
        manages fosipcore@<username>, running as that user with config from
        /home/<username>/.config/fosipcore/env (see pkgs/example.env).
        Enabling no users installs only the package, with no systemd units.
      '';
    };
  };

  config = {
    assertions =
      lib.mapAttrsToList (
        name: userCfg: {
          assertion = !userCfg.enable || cfg.enable;
          message = "services.fosipcore.users.${name}.enable requires services.fosipcore.enable = true;";
        }
      ) cfg.users;

    # Puts the binary on the default PATH (convenience; the service itself
    # references the store path directly, so this is not required for the
    # unit to work).
    environment.systemPackages = lib.mkIf cfg.enable [ cfg.package ];

    # Symlink the packaged units — in particular the fosipcore@.service
    # template — into the active systemd unit directory (/run/systemd/system),
    # so the per-user instance drop-ins below can extend it. Without this the
    # template only lives in the system profile and is not on systemd's unit
    # search path, so the instances would not resolve.
    systemd.packages = lib.mkIf (cfg.enable && anyEnabled) [ cfg.package ];

    # One instance per enabled user.
    systemd.services = lib.mkIf (cfg.enable && anyEnabled) (lib.mapAttrs' (
      username: userCfg:
      lib.nameValuePair "fosipcore@${username}" {
        # The unit stays enabled (i.e. not masked to /dev/null) even when
        # autostart is false, so it can be started manually.
        enable = true;
        wantedBy = lib.optionals userCfg.autostart [ "multi-user.target" ];
        # Emit a drop-in for the instance, extending the packaged template
        # unit rather than generating a full unit that would shadow it.
        overrideStrategy = "asDropin";
        # The packaged unit's ExecStart (/usr/bin/fosipcore) does not exist
        # on NixOS; point it at the real binary.
        serviceConfig.ExecStart = [ (lib.getExe cfg.package) ];
      }
    ) enabledUsers);
  };
}
