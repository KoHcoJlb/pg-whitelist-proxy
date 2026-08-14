{
  description = "PostgreSQL query whitelist proxy";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, crane, ... }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;

      mkPackage = system:
        let
          pkgs = import nixpkgs { inherit system; };
          craneLib = crane.mkLib pkgs;
          commonArgs = {
            src = craneLib.cleanCargoSource ./.;
            strictDeps = true;

            nativeBuildInputs = [ pkgs.rustPlatform.bindgenHook ];

            meta = {
              mainProgram = "pg-whitelist-proxy";
            };
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        in
        craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });
    in
    {
      packages = forAllSystems (system:
        let
          package = mkPackage system;
        in
        {
          pg-whitelist-proxy = package;
          default = package;
        });

      nixosModules = {
        pg-whitelist-proxy = { config, lib, pkgs, ... }:
          let
            cfg = config.services.pg-whitelist-proxy;
            settingsFormat = pkgs.formats.toml { };
            configFile = settingsFormat.generate "config.toml" cfg.settings;
            defaultPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          in
          {
            options.services.pg-whitelist-proxy = {
              enable = lib.mkEnableOption "the pg-whitelist-proxy PostgreSQL proxy";

              package = lib.mkOption {
                type = lib.types.package;
                default = defaultPackage;
                defaultText = lib.literalExpression "self.packages.\${pkgs.stdenv.hostPlatform.system}.default";
                description = "The pg-whitelist-proxy package to run.";
              };

              settings = lib.mkOption {
                type = settingsFormat.type;
                default = { };
                description = ''
                  Freeform settings written to pg-whitelist-proxy's config.toml.
                '';
              };

              environmentFile = lib.mkOption {
                type = lib.types.nullOr lib.types.path;
                default = null;
                example = "/run/secrets/pg-whitelist-proxy.env";
                description = ''
                  Optional systemd environment file. This can provide secrets such as
                  GRAFANA_TOKEN without placing them in the Nix store.
                '';
              };
            };

            config = lib.mkIf cfg.enable {
              systemd.services.pg-whitelist-proxy = {
                description = "PostgreSQL query whitelist proxy";
                wantedBy = [ "multi-user.target" ];
                wants = [ "network-online.target" ];
                after = [ "network-online.target" ];
                restartTriggers = lib.optional (cfg.environmentFile != null) cfg.environmentFile;

                serviceConfig = {
                  Type = "simple";
                  ExecStart = "${lib.getExe cfg.package} --config ${configFile}";
                  DynamicUser = true;
                  Restart = "on-failure";
                  RestartSec = "5s";

                  AmbientCapabilities = "";
                  CapabilityBoundingSet = "";
                  DevicePolicy = "closed";
                  KeyringMode = "private";
                  LockPersonality = true;
                  MemoryDenyWriteExecute = true;
                  NoNewPrivileges = true;
                  PrivateDevices = true;
                  PrivateMounts = true;
                  PrivateTmp = true;
                  ProcSubset = "pid";
                  ProtectClock = true;
                  ProtectControlGroups = true;
                  ProtectHome = true;
                  ProtectHostname = true;
                  ProtectKernelLogs = true;
                  ProtectKernelModules = true;
                  ProtectKernelTunables = true;
                  ProtectProc = "invisible";
                  ProtectSystem = "strict";
                  RemoveIPC = true;
                  RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
                  RestrictNamespaces = true;
                  RestrictRealtime = true;
                  RestrictSUIDSGID = true;
                  SystemCallArchitectures = "native";
                  SystemCallErrorNumber = "EPERM";
                  SystemCallFilter = [ "@system-service" "~@privileged" ];
                  UMask = "0077";
                } // lib.optionalAttrs (cfg.environmentFile != null) {
                  EnvironmentFile = cfg.environmentFile;
                };
              };
            };
          };
        default = self.nixosModules.pg-whitelist-proxy;
      };
    };
}
