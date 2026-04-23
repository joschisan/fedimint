{
  lib,
  stdenv,
  fetchCrate,
  rustPlatform,
}:
rustPlatform.buildRustPackage rec {
  pname = "honggfuzz";
  # last tagged version is far behind master
  version = "0.5.60";

  src = fetchCrate {
    inherit pname version;
    sha256 = "sha256-btHYe+rN28bVeDWZB3AQCeF5mk30YNIINMXOOoTIjJk=";
  };

  cargoHash = "sha256-9jlu9PDqQRW3r+ZJrGxDXB533gTa8XexZuK5LXcNY3s=";

  buildInputs = lib.optionals stdenv.isDarwin [ ];
}
