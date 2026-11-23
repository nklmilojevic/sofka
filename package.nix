{ lib, stdenv, craneLib, cmake, installShellFiles }:

let
  cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

  commonArgs = {
    pname = cargoToml.package.name;
    version = cargoToml.package.version;

    src = lib.cleanSource ./.;

    strictDeps = true;

    # aws-lc-sys (the TLS client's crypto backend) builds AWS-LC's C sources
    # with cmake. Bindings are pregenerated for all platforms this package
    # targets, so no bindgen/libclang is needed here.
    nativeBuildInputs = [ cmake ];

    # The test suite builds a rustls-backed kube::Client (even for the "fake"
    # test cluster), which needs native root CA certs — unavailable in Nix's
    # network-less build sandbox. Tests run properly in CI via `cargo test`;
    # this only skips re-running them inside the hermetic package build.
    doCheck = false;
  };

  # Dependencies build in their own derivation, keyed only on Cargo.toml and
  # Cargo.lock, so Cachix serves them to every rev that leaves them unchanged
  # and only sofka itself is recompiled.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (
  commonArgs
    // {
    inherit cargoArtifacts;

    # Kept out of commonArgs so the cached dependency build is unchanged.
    nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ installShellFiles ];

    postInstall = lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
      installShellCompletion --cmd sofka \
        --bash <($out/bin/sofka completion bash) \
        --fish <($out/bin/sofka completion fish) \
        --zsh <($out/bin/sofka completion zsh)
    '';

    meta = {
      description = "A Kubernetes TUI, reimagined in Rust";
      homepage = "https://github.com/nklmilojevic/sofka";
      mainProgram = "sofka";
      platforms = lib.platforms.unix;
      license = with lib.licenses; [ mit asl20 ];
    };
  }
)
