{
  description = "kakehashi - A Tree-sitter Language Server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        # Rust 2024 edition requires nightly or recent stable
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
          ];
        };

        # Tree-sitter grammars for testing (each includes parser + queries)
        treeSitterGrammars = with pkgs.tree-sitter-grammars; [
          tree-sitter-bash
          tree-sitter-c
          tree-sitter-go
          tree-sitter-javascript
          tree-sitter-json
          tree-sitter-lua
          tree-sitter-markdown
          tree-sitter-markdown-inline
          tree-sitter-python
          tree-sitter-rust
          tree-sitter-toml
          tree-sitter-typescript
          tree-sitter-yaml
        ];

        # Combined tree-sitter directory with all grammars
        # Structure: parser/<lang>.so, queries/<lang>/*.scm
        treeSitterCombined = pkgs.runCommand "tree-sitter-grammars-combined" { } ''
          mkdir -p $out/parser $out/queries
          ${pkgs.lib.concatMapStringsSep "\n" (
            grammar:
            let
              # Extract language name from package name (e.g., tree-sitter-lua-0.0.19... -> lua)
              name = grammar.pname or (builtins.baseNameOf grammar);
              lang = builtins.replaceStrings [ "tree-sitter-" ] [ "" ] (pkgs.lib.removeSuffix "-grammar" name);
              # Normalize: markdown-inline -> markdown_inline (underscores for file names)
              normalizedLang = builtins.replaceStrings [ "-" ] [ "_" ] lang;
            in
            ''
              # ${lang}
              if [ -f "${grammar}/parser" ]; then
                ln -sf "${grammar}/parser" "$out/parser/${normalizedLang}.so"
              fi
              if [ -d "${grammar}/queries" ]; then
                mkdir -p "$out/queries/${normalizedLang}"
                for f in "${grammar}/queries"/*; do
                  [ -e "$f" ] && ln -sf "$f" "$out/queries/${normalizedLang}/"
                done
              fi
            ''
          ) treeSitterGrammars}
        '';
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs =
            with pkgs;
            [
              # Rust toolchain
              rustToolchain

              # Build dependencies
              pkg-config
              openssl

              # Tree-sitter CLI (for parser compilation)
              tree-sitter

              # Development tools
              cargo-watch
              cargo-edit

              # For Neovim integration testing
              neovim
              vimPlugins.mini-nvim # Test framework
              git
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              # macOS-specific dependencies
              pkgs.apple-sdk_15
              pkgs.libiconv
            ];

          shellHook = ''
            echo "kakehashi development environment"
            echo "Rust: $(rustc --version)"
            echo "Cargo: $(cargo --version)"
            echo ""
            echo "Available commands: (run 'make help' for more)"
            echo "  cargo build    - Build the project (debug)"
            echo "  make           - Build the project (release)"
            echo "  cargo test     - Run tests"
            echo "  cargo clippy   - Run linter"
            echo "  make test_nvim - Run Neovim tests (no 'make deps' needed!)"
          '';

          # Environment variables
          RUST_BACKTRACE = "1";
          RUST_LOG = "info";

          # Tree-sitter paths for testing
          TREE_SITTER_GRAMMARS = "${treeSitterCombined}";
          MINI_NVIM = "${pkgs.vimPlugins.mini-nvim}";
        };

        formatter = pkgs.nixfmt-tree;

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "kakehashi";
          version = (pkgs.lib.importTOML ./Cargo.toml).package.version;
          src = self;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs =
            with pkgs;
            [
              openssl
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.apple-sdk_15
              pkgs.libiconv
            ];

          # Skip tests that require network access or debug-only assertions
          checkFlags = [
            "--skip"
            "test_clone_repo"
            "--skip"
            "test_edit_info_new_rejects_invalid_in_debug"
            "--skip"
            "test_dynamic_lua_load"
            "--skip"
            "test_compile_parser_with_loader"
            "--skip"
            "test_download_and_extract_archive_for_json_parser"
            "--skip"
            "test_fetch_source_downloads_source_with_parser_c"
            "--skip"
            "test_try_install_returns_already_installing_on_duplicate"
            "--skip"
            "test_language_list_command"
          ];

          meta = with pkgs.lib; {
            description = "kakehashi - A Tree-sitter Language Server";
            homepage = "https://github.com/atusy/kakehashi";
            license = licenses.mit;
            maintainers = with maintainers; [ atusy ];
          };
        };
      }
    );
}
