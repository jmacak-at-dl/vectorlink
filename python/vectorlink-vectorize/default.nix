{ pkgs }:
with pkgs;
with pkgs.python311Packages;
buildPythonPackage rec {
  name = "vectorlink_vectorize";
  src = ./.;
  format = "pyproject";
  propagatedBuildInputs = [
    poetry-core
    numpy
    torch
    transformers
    accelerate
    sentence-transformers
    boto3
    pybars3
    (pkgs.callPackage ../../vectorlink-task-py {})
    #(import ../../vectorlink-task-py { inherit pkgs; })
  ];
}
