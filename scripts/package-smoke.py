"""Exercise native package installs and Rojo builds in disposable directories."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import xml.etree.ElementTree as ET


def run(arguments, *, cwd, environment, capture=False):
    result = subprocess.run(
        [str(argument) for argument in arguments],
        cwd=cwd,
        env=environment,
        check=True,
        text=True,
        stdout=subprocess.PIPE if capture else None,
    )
    return result.stdout


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def project(directory, package_directory):
    (directory / "default.project.json").write_text(
        json.dumps(
            {
                "name": "package-smoke",
                "tree": {
                    "$className": "DataModel",
                    "ReplicatedStorage": {"Packages": {"$path": package_directory}},
                },
            }
        ),
        encoding="utf-8",
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for tool in ("rojo", "wally", "pesde", "lune"):
        parser.add_argument(f"--{tool}", type=Path, required=True)
    options = parser.parse_args()
    tools = {name: getattr(options, name).resolve(strict=True) for name in ("rojo", "wally", "pesde", "lune")}
    with tempfile.TemporaryDirectory(prefix="rojo-package-smoke-") as temporary:
        root = Path(temporary)
        environment = dict(os.environ)
        environment["PESDE_HOME"] = str(root / "pesde-home")
        environment["PATH"] = os.pathsep.join(
            [str(tools["lune"].parent), str(tools["rojo"].parent), environment.get("PATH", "")]
        )
        for tool, binary in tools.items():
            print(f"{tool}: {run([binary, '--version'], cwd=root, environment=environment, capture=True).strip()}")

        native = root / "local-library"
        (native / "src").mkdir(parents=True)
        (native / "src/init.luau").write_text("return { packageSmoke = true }\n", encoding="utf-8")
        (native / "pesde.toml").write_text(
            'name = "codex/local_library"\nversion = "0.1.0"\nprivate = true\n'
            '[target]\nenvironment = "roblox"\nlib = "src/init.luau"\nbuild_files = ["src"]\n',
            encoding="utf-8",
        )
        for manager, packages in (("wally", "Packages"), ("pesde", "roblox_packages")):
            directory = root / manager
            directory.mkdir()
            project(directory, packages)
            manifest = (
                '[package]\nname = "codex/package-smoke"\nversion = "0.1.0"\nrealm = "shared"\n'
                'registry = "https://github.com/UpliftGames/wally-index"\n'
                '[dependencies]\nPromise = "evaera/promise@4.0.0"\n'
                if manager == "wally" else
                'name = "codex/package_smoke"\nversion = "0.1.0"\nprivate = true\n'
                '[target]\nenvironment = "roblox"\n'
                '[scripts]\nroblox_sync_config_generator = ".pesde/scripts/roblox_sync_config_generator.luau"\n'
                'sourcemap_generator = ".pesde/scripts/sourcemap_generator.luau"\n'
                '[indices]\ndefault = "https://github.com/pesde-pkg/index"\n'
                '[wally_indices]\ndefault = "https://github.com/UpliftGames/wally-index"\n'
                '[dependencies]\npromise = { wally = "evaera/promise", version = "=4.0.0" }\n'
                'native = { path = "../local-library" }\n'
                '[dev_dependencies]\nscripts = { name = "pesde/scripts_rojo", version = "=0.2.0", target = "lune" }\n'
                'rojo = { name = "pesde/rojo", version = "^7.5.1", target = "lune" }\n'
            )
            (directory / f"{manager}.toml").write_text(manifest, encoding="utf-8")
            install = [tools["rojo"], "packages", "install", directory, "--manager", manager, "--executable", tools[manager]]
            run(install, cwd=root, environment=environment)
            lockfile = directory / f"{manager}.lock"
            before = digest(lockfile)
            run(install + (["--locked"] if manager == "pesde" else []), cwd=root, environment=environment)
            assert digest(lockfile) == before, f"{manager} changed its lockfile on a second install"
            report = json.loads(run([tools["rojo"], "packages", "doctor", directory, "--json"], cwd=root, environment=environment, capture=True))
            assert report["schemaVersion"] == 1 and report["ok"], report
            assert not report["findings"], report["findings"]
            output = directory / "output.rbxlx"
            run([tools["rojo"], "build", directory, "--output", output], cwd=root, environment=environment)
            model = ET.parse(output)
            assert model.findall(".//Item[@class='ModuleScript']"), f"{manager} build has no package modules"
            if manager == "pesde":
                assert (directory / ".pesde/scripts/roblox_sync_config_generator.luau").is_file()
                generated = directory / "roblox_packages/.pesde/codex+local_library/0.1.0/local_library/default.project.json"
                assert json.loads(generated.read_text(encoding="utf-8"))["tree"]["src"]["$path"] == "src"
                assert "packageSmoke = true" in output.read_text(encoding="utf-8")
                manifest_path = directory / "pesde.toml"
                original_manifest = manifest_path.read_bytes()
                try:
                    manifest_path.write_bytes(original_manifest.replace(b'version = "=4.0.0"', b'version = "=3.1.0"'))
                    failed = subprocess.run(
                        [str(argument) for argument in install + ["--locked"]],
                        cwd=root, env=environment, text=True,
                        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False,
                    )
                    assert failed.returncode != 0, "pesde --locked accepted an outdated lockfile"
                    assert digest(lockfile) == before, "a rejected locked install modified pesde.lock"
                finally:
                    manifest_path.write_bytes(original_manifest)
            print(f"{manager}: install, repeat install, lockfile hash, doctor, generated configuration and XML build passed")


if __name__ == "__main__":
    main()
