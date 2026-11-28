{
  description = "A YouTube desktop client in Rust + GPUI with SponsorBlock, DeArrow, and local subscriptions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;

        runtimeLibs = with pkgs; [
          mpv-unwrapped
          vulkan-loader
          libGL
          wayland
          libxkbcommon
          libx11
          libxcb
          libxcursor
          libxrandr
          libxi
          fontconfig
          freetype
          noto-fonts
          openssl
          zstd
          sqlite
        ];
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
            cmake
            rustPlatform.bindgenHook
            makeWrapper
          ];

          buildInputs = with pkgs; [
            fontconfig
            freetype
            wayland
            libxkbcommon
            libx11
            libxcb
            libxcursor
            libxrandr
            libxi
            vulkan-headers
            vulkan-loader
            openssl
            zstd
            sqlite
          ];

          doCheck = false;

          postInstall = ''
            install -Dm644 packaging/linux/streamtube.desktop $out/share/applications/streamtube.desktop
            install -Dm644 packaging/linux/io.github.avaneesh_inamdar.StreamTube.metainfo.xml $out/share/metainfo/io.github.avaneesh_inamdar.StreamTube.metainfo.xml

            if [ -f assets/logo.png ]; then
              install -Dm644 assets/logo.png $out/share/icons/hicolor/256x256/apps/streamtube.png
            fi
            if [ -f assets/logo.svg ]; then
              install -Dm644 assets/logo.svg $out/share/icons/hicolor/scalable/apps/streamtube.svg
            fi

            wrapProgram $out/bin/streamtube \
              --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath runtimeLibs}" \
              --prefix PATH : "${pkgs.lib.makeBinPath [ pkgs.yt-dlp pkgs.ffmpeg-headless ]}" \
              --set FONTCONFIG_FILE "${pkgs.makeFontsConf { fontDirectories = [ pkgs.noto-fonts ]; }}"
          '';

          meta = with pkgs.lib; {
            description = manifest.description;
            homepage = "https://github.com/Avaneesh-Inamdar/Stream-Tube-Desktop";
            license = licenses.mit;
            platforms = platforms.linux;
            mainProgram = "streamtube";
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [
            rustc
            cargo
            rust-analyzer
            clippy
            rustfmt
          ];
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
        };
      });
}
