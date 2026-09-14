{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.clip-sync;
  configPath =
    if cfg.configFile == null then "%h/.config/clip-sync/config.toml" else toString cfg.configFile;
in
{
  options.services.clip-sync = import ./options.nix { inherit lib; };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    systemd.user.services.clip-sync = {
      Unit = {
        Description = "Masterless encrypted clipboard-history mesh";
        Documentation = [ "https://github.com/Fractal-Tess/clip-sync" ];
        After = [ "graphical-session-pre.target" ];
        PartOf = [ "graphical-session.target" ];
      };
      Install.WantedBy = lib.optionals cfg.autoStart cfg.wantedBy;

      Service = {
        Environment = lib.mapAttrsToList (name: value: "${name}=${value}") (
          {
            PATH = "${
              lib.makeBinPath [ pkgs.iproute2 ]
            }:%h/.nix-profile/bin:/etc/profiles/per-user/%u/bin:/run/current-system/sw/bin";
          }
          // cfg.environment
        );
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} --config ${configPath} daemon";
        Restart = "on-failure";
        RestartSec = 2;
        TimeoutStopSec = 10;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectSystem = "strict";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
          "AF_NETLINK"
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
        UMask = "0077";
      };
    };
  };
}
