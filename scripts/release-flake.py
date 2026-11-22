"""Package a Nix launcher for the native archives of one telega release."""

import argparse
import json
from pathlib import Path
import shutil
import tarfile
import tempfile


SYSTEMS = ("aarch64-darwin", "aarch64-linux", "x86_64-linux")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("assets", type=Path)
    parser.add_argument("tag")
    parser.add_argument("--program", choices=("telega", "tgp-viewer"), default="telega")
    parser.add_argument("--system", choices=SYSTEMS, help="package only this platform for local distribution")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    systems = [args.system] if args.system else SYSTEMS
    with tempfile.TemporaryDirectory(dir=args.assets) as temp:
        launcher = Path(temp) / f"{args.program}-nix"
        launcher.mkdir()
        shutil.copyfile(root / "nix/release-flake.nix", launcher / "flake.nix")
        shutil.copyfile(root / "nix/wrap.nix", launcher / "wrap.nix")
        shutil.copyfile(root / "flake.lock", launcher / "flake.lock")
        (launcher / "release.json").write_text(json.dumps({"tag": args.tag, "systems": systems, "program": args.program}))
        output = Path(temp) / f"{args.program}-nix.tar.gz"
        with tarfile.open(output, "w:gz", compresslevel=1) as archive:
            archive.add(launcher, arcname=launcher.name)
            for system in systems:
                native = args.assets / f"{args.program}-{system}.tar.gz"
                archive.add(native, arcname=f"{launcher.name}/{native.name}")
        output.replace(args.assets / output.name)


if __name__ == "__main__":
    main()
