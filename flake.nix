{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = nixpkgs.lib;

      screentime-web = pkgs.rustPlatform.buildRustPackage {
        pname = "screentime-web";
        version = (lib.importTOML ./Cargo.toml).package.version;
        src = lib.fileset.toSource {
          root = ./.;
          fileset = lib.fileset.unions [
            ./Cargo.toml
            ./Cargo.lock
            ./src
            ./static # index.html is embedded via include_str!
          ];
        };
        cargoLock.lockFile = ./Cargo.lock;

        meta = {
          description = "screentime-web";
          license = lib.licenses.mit;
          mainProgram = "screentime-web";
        };
      };
    in
    {
      packages.${system} = {
        default = screentime-web;
        inherit screentime-web;
      };
    };
}
