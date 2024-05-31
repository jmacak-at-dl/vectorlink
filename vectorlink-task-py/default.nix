{ buildWorkspacePackage, python311Packages, maturin }:
buildWorkspacePackage rec {
  projectPath = ./.;

  # so we aren't actually going to build a rust package. instead, we needed to get this far just so we are in a position to run maturin.
  nativeBuildInputs = [
    python311Packages.pip
    maturin
  ];

  buildPhase = ''
maturin build --frozen --manylinux off --strip --release -m vectorlink-task-py/Cargo.toml
mkdir -p dist
cp target/wheels/*.whl dist
'';
  installPhase = ''
pip install dist/*.whl --no-index --no-warn-script-location --prefix="$out" --no-cache
'';
}
