import json
import os
from collections import Counter
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

import msgpack


EXE = Path(sys.argv[1]).resolve()
BASE = Path(tempfile.mkdtemp(prefix="rojo-watcher-proof-")).resolve()
ROOT = Path(tempfile.mkdtemp(prefix="watcher-smoke-", dir=BASE)).resolve()
REPORT = {"fixture": str(ROOT), "binary": str(EXE), "stages": []}
# Keeps console windows from popping up on Windows. Other platforms do not
# have this flag.
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


def git(*args):
    return subprocess.run(
        ["git", "-c", "core.autocrlf=false", "-c", "user.name=Rojo smoke", "-c", "user.email=smoke@example.invalid", *args],
        cwd=ROOT, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        creationflags=NO_WINDOW,
    ).stdout.strip()


def remove_tree(path):
    path = path.resolve()
    assert path.is_relative_to(ROOT) and path != ROOT
    if path.exists():
        shutil.rmtree(path)


def branch_files(label):
    remove_tree(ROOT / "src")
    for i in range(180):
        folder = ROOT / "src" / f"group{i % 18:02d}"
        folder.mkdir(parents=True, exist_ok=True)
        (folder / f"{label}{i:03d}.luau").write_text(f"return '{label}-{i}'\n", encoding="utf-8")
    (ROOT / "src" / "Shared.luau").write_text(f"return '{label}'\n", encoding="utf-8")


def package_files(label):
    remove_tree(ROOT / "Packages")
    for i in range(140):
        folder = ROOT / "Packages" / f"package{i % 14:02d}" / "nested"
        folder.mkdir(parents=True, exist_ok=True)
        (folder / f"Dep{i:03d}.luau").write_text(f"return '{label}-{i}'\n", encoding="utf-8")
    module = ROOT / "Packages" / "InitModule"
    module.mkdir()
    (module / "init.luau").write_text(f"return '{label}-init'\n", encoding="utf-8")


# The project maps these folders to these children of the root instance.
MAPPED = {"Branches": "src", "Packages": "Packages"}


def expected_entries(path, instance_path):
    """Yields the (path, Source) of every instance Rojo should make from the
    folder or script at `path`, where Source is None for folders."""
    if path.is_file():
        yield instance_path, path.read_text(encoding="utf-8")
        return
    init = path / "init.luau"
    yield instance_path, init.read_text(encoding="utf-8") if init.exists() else None
    for child in path.iterdir():
        if child == init:
            continue
        name = child.stem if child.is_file() else child.name
        yield from expected_entries(child, f"{instance_path}/{name}")


def expected_tree():
    """Counts every instance expected below the root, keyed by its path and
    Source. Folders count too, so a stale folder left behind is noticed."""
    return Counter(
        entry
        for name, folder in MAPPED.items()
        for entry in expected_entries(ROOT / folder, name)
    )


class Server:
    def __init__(self, label, verbatim=False):
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.base = f"http://127.0.0.1:{self.port}"
        self.log_path = ROOT / f"{label}.log"
        self.log = self.log_path.open("wb")
        project = str(ROOT / "default.project.json")
        # Verbatim paths only exist on Windows.
        if verbatim and os.name == "nt":
            project = "\\\\?\\" + project
        self.process = subprocess.Popen(
            [str(EXE), "serve", project, "--port", str(self.port)], cwd=ROOT,
            stdout=self.log, stderr=subprocess.STDOUT, creationflags=NO_WINDOW,
        )
        start = time.perf_counter()
        while time.perf_counter() - start < 15:
            self.ensure_alive()
            try:
                self.info = self.request("/api/rojo")
                break
            except OSError:
                time.sleep(0.025)
        else:
            raise AssertionError("server startup timed out")
        self.root_id = self.info["rootInstanceId"]
        self.startup_ms = round((time.perf_counter() - start) * 1000, 2)

    def ensure_alive(self):
        assert self.process.poll() is None, self.log_path.read_text(errors="replace")

    def request(self, path, payload=None):
        self.ensure_alive()
        data = None if payload is None else msgpack.packb(payload, use_bin_type=True)
        req = urllib.request.Request(self.base + path, data=data)
        if data:
            req.add_header("Content-Type", "application/msgpack")
        with urllib.request.urlopen(req, timeout=3) as response:
            return msgpack.unpackb(response.read(), raw=False, strict_map_key=False)

    def read(self):
        return self.request("/api/read/" + self.root_id)

    def tree(self, read):
        """Counts every instance below the root like `expected_tree`.

        Sibling instances with the same name share a path, so duplicates show
        up as a count above one instead of collapsing into one entry."""
        instances = read["instances"]
        entries = Counter()

        def visit(instance_id, path):
            inst = instances[instance_id]
            source = inst["Properties"].get("Source")
            entries[(path, None if source is None else source["String"])] += 1
            for child in inst["Children"]:
                visit(child, f"{path}/{instances[child]['Name']}")

        for child in instances[self.root_id]["Children"]:
            visit(child, instances[child]["Name"])
        return entries

    def converge(self, label, expected):
        start = time.perf_counter()
        actual = Counter()
        while time.perf_counter() - start < 12:
            read = self.read()
            actual = self.tree(read)
            if actual == expected:
                scripts = sum(count for (_, source), count in actual.items() if source is not None)
                stage = {"label": label, "converged_ms": round((time.perf_counter() - start) * 1000, 2), "scripts": scripts, "instances": sum(actual.values()), "cursor": read["messageCursor"]}
                REPORT["stages"].append(stage)
                print(json.dumps(stage), flush=True)
                return read
            time.sleep(0.025)
        expected_sources = {path: source for path, source in expected}
        actual_sources = {path: source for path, source in actual}
        missing = sorted(expected_sources.keys() - actual_sources.keys())[:10]
        extra = sorted(actual_sources.keys() - expected_sources.keys())[:10]
        changed = sorted(path for path in expected_sources.keys() & actual_sources.keys() if expected_sources[path] != actual_sources[path])[:10]
        paths = Counter(path for (path, _), count in actual.items() for _ in range(count))
        duplicates = sorted(path for path, count in paths.items() if count > 1)[:10]
        raise AssertionError(f"{label}: missing={missing}, extra={extra}, changed={changed}, duplicates={duplicates}")

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=10)
        self.log.close()
        text = self.log_path.read_text(errors="replace")
        assert "Rojo crashed" not in text and "panicked" not in text, text


server = None
try:
    git("init", "--initial-branch=blue")
    (ROOT / "default.project.json").write_text(json.dumps({"name": "watcher-smoke", "tree": {"$className": "Folder", "Branches": {"$path": "src"}, "Packages": {"$path": "Packages"}}}), encoding="utf-8")
    (ROOT / ".gitignore").write_text("Packages/\n*.log\n", encoding="utf-8")
    branch_files("Blue")
    package_files("initial")
    git("add", ".")
    git("commit", "-m", "Create blue fixture (AI-assisted)")
    git("checkout", "-b", "amber")
    branch_files("Amber")
    git("add", "-A")
    git("commit", "-m", "Create amber fixture (AI-assisted)")
    git("checkout", "blue")
    server = Server("serve-first")
    REPORT["first_startup_ms"] = server.startup_ms
    REPORT["server_version"] = server.info["serverVersion"]
    server.converge("initial_read", expected_tree())

    for i in range(10):
        branch = "amber" if i % 2 == 0 else "blue"
        start = time.perf_counter()
        git("checkout", branch)
        server.converge(f"checkout_{i}_{branch}", expected_tree())
        REPORT["stages"][-1]["checkout_and_converge_ms"] = round((time.perf_counter() - start) * 1000, 2)

    start = time.perf_counter()
    for i in range(12):
        package_files(f"burst{i}")
    server.converge("package_burst_12_replacements", expected_tree())
    REPORT["stages"][-1]["replace_and_converge_ms"] = round((time.perf_counter() - start) * 1000, 2)

    # Replacing Packages faster than the watcher debounce leaves no separate
    # removal event. A watch still bound to the deleted folder would ignore
    # these edits, so they must converge too.
    (ROOT / "Packages" / "package00" / "nested" / "Dep000.luau").write_text("return 'edited-after-burst'\n", encoding="utf-8")
    (ROOT / "Packages" / "package01" / "nested" / "AddedAfterBurst.luau").write_text("return 'added-after-burst'\n", encoding="utf-8")
    server.converge("edit_inside_packages_after_burst", expected_tree())

    remove_tree(ROOT / "Packages")
    time.sleep(0.5)
    package_files("delayed-recreate")
    server.converge("mapped_root_recreated_after_deletion_settles", expected_tree())

    read = server.read()
    init_id = next(key for key, inst in read["instances"].items() if inst["Name"] == "InitModule")
    source = "return 'from-client'\n"
    server.request("/api/write", {"sessionId": server.info["sessionId"], "removed": [], "updated": [{"id": init_id, "changedName": None, "changedClassName": None, "changedMetadata": None, "changedProperties": {"Source": {"String": source}}}]})
    init_path = ROOT / "Packages" / "InitModule" / "init.luau"
    deadline = time.perf_counter() + 5
    while init_path.read_text(encoding="utf-8") != source and time.perf_counter() < deadline:
        time.sleep(0.01)
    assert init_path.read_text(encoding="utf-8") == source
    server.converge("two_way_init_write", expected_tree())
    server.stop()
    server = Server("serve-restarted-verbatim", verbatim=True)
    REPORT["restart_startup_ms"] = server.startup_ms
    server.converge("restart_initial_read_verbatim_path", expected_tree())
    (ROOT / "src" / "Shared.luau").write_text("return 'post-restart'\n", encoding="utf-8")
    server.converge("write_after_restart", expected_tree())
    REPORT["success"] = True
finally:
    stop_error = None
    if server is not None:
        try:
            server.stop()
        except Exception as err:
            # After a failure, raising here would hide the error that ended
            # the run, so only report it.
            if REPORT.get("success"):
                REPORT["success"] = False
                stop_error = err
            else:
                print(f"Stopping the server failed too: {err}", file=sys.stderr, flush=True)
    (BASE / "watcher-smoke-results.json").write_text(json.dumps(REPORT, indent=2), encoding="utf-8")
    print(json.dumps({"report": str(BASE / "watcher-smoke-results.json"), "fixture": str(ROOT), "success": REPORT.get("success", False)}), flush=True)
    if stop_error is not None:
        raise stop_error
