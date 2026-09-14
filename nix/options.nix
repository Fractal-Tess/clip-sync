{ lib }:
{
  enable = lib.mkEnableOption "the clip-sync user clipboard mesh";

  package = lib.mkOption {
    type = lib.types.package;
    description = "clip-sync package to run and install.";
  };

  configFile = lib.mkOption {
    type = lib.types.nullOr lib.types.path;
    default = null;
    description = ''
      Configuration file passed to clip-sync. When unset, each user uses
      %h/.config/clip-sync/config.toml, allowing a writable per-user config.
    '';
  };

  autoStart = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = "Whether enabling the service adds its default user target as a startup dependency.";
  };

  wantedBy = lib.mkOption {
    type = lib.types.listOf lib.types.str;
    default = [ "graphical-session.target" ];
    description = "User targets that start clip-sync when autoStart is enabled.";
  };

  environment = lib.mkOption {
    type = lib.types.attrsOf lib.types.str;
    default = { };
    description = "Additional environment variables for the user service.";
  };
}
