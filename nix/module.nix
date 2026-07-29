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
  options.services.clip-sync = {
    enable = lib.mkEnableOption "the clip-sync user clipboard mesh";

    package = lib.mkOption {
      type = lib.types.package;
      description = "clip-sync package to run.";
    };

    configFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Configuration file passed to clip-sync. When unset, each user uses
        %h/.config/clip-sync/config.toml, allowing a writable Stow-managed file.
      '';
    };

    wantedBy = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "graphical-session.target" ];
      description = "User targets that start clip-sync.";
    };

    extraEnvironment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Additional environment variables for the user service.";
    };

    tray.enable = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Run the persistent StatusNotifier tray item. Disable this when using
        the daemon-only package or a desktop without a tray host.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    systemd.user.services.clip-sync = {
      description = "Masterless encrypted clipboard-history mesh";
      documentation = [ "https://github.com/Fractal-Tess/clip-sync" ];
      after = [ "graphical-session-pre.target" ];
      partOf = [ "graphical-session.target" ];
      wantedBy = cfg.wantedBy;
      environment = cfg.extraEnvironment;
      path = [ pkgs.netbird ];

      serviceConfig = {
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
        ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
        UMask = "0077";
      };
    };

    systemd.user.services.clip-sync-tray = lib.mkIf cfg.tray.enable {
      description = "clip-sync system tray";
      documentation = [ "https://github.com/Fractal-Tess/clip-sync" ];
      after = [
        "clip-sync.service"
        "graphical-session-pre.target"
      ];
      partOf = [ "graphical-session.target" ];
      wantedBy = cfg.wantedBy;
      environment = cfg.extraEnvironment;
      path = [ pkgs.hyprland ];

      serviceConfig = {
        Type = "simple";
        ExecStart = "${lib.getExe cfg.package} --config ${configPath} ui tray";
        Restart = "on-failure";
        RestartSec = 2;
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        RestrictAddressFamilies = [ "AF_UNIX" ];
        UMask = "0077";
      };
    };
  };
}
