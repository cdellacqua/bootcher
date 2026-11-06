"""Shared helpers for the CI recipe script tests."""

import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile

SCRIPTS = pathlib.Path(__file__).resolve().parent.parent / "scripts"


def load_script(name):
    """Import `scripts/<name>.py` as a module (its `main()` doesn't run on import)."""
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# A stand-in for `skopeo`: answers `skopeo inspect <args>` from a JSON table mapping
# the space-joined args to {"json": ...} (printed) or {"error": ...} (exit 1).
_FAKE_SKOPEO = """#!/usr/bin/env python3
import json, os, sys
table = json.load(open(os.environ["FAKE_SKOPEO_TABLE"]))
reply = table.get(" ".join(sys.argv[2:]), {"error": "manifest unknown"})
if "error" in reply:
    sys.stderr.write(reply["error"] + "\\n")
    sys.exit(1)
print(json.dumps(reply["json"]))
"""


def run_script(name, *, files=None, skopeo=None, env=None):
    """Run `scripts/<name>.py` in a fresh directory with a fake `skopeo` on PATH.

    `files` are written into the directory first, `skopeo` is the fake's answer table.
    Returns `(returncode, stdout, stderr, outputs)`, where `outputs` holds the
    contents of `base.env` and of the GITHUB_OUTPUT file (keys `base.env`/`github`),
    when written.
    """
    with tempfile.TemporaryDirectory() as tmp:
        tmp = pathlib.Path(tmp)
        bin_dir = tmp / "bin"
        bin_dir.mkdir()
        fake = bin_dir / "skopeo"
        fake.write_text(_FAKE_SKOPEO)
        fake.chmod(0o755)
        table = tmp / "skopeo.json"
        table.write_text(json.dumps(skopeo or {}))
        work = tmp / "work"
        work.mkdir()
        for rel, content in (files or {}).items():
            (work / rel).write_text(content)
        full_env = dict(os.environ, PATH=f"{bin_dir}:{os.environ['PATH']}",
                        FAKE_SKOPEO_TABLE=str(table))
        full_env.pop("GITHUB_OUTPUT", None)
        full_env.update({k: v.replace("{dir}", str(work)) for k, v in (env or {}).items()})
        run = subprocess.run([sys.executable, "-B", str(SCRIPTS / f"{name}.py")], cwd=work,
                             env=full_env, capture_output=True, text=True)
        outputs = {}
        if (work / "base.env").exists():
            outputs["base.env"] = (work / "base.env").read_text()
        if (work / "github_output").exists():
            outputs["github"] = (work / "github_output").read_text()
        return run.returncode, run.stdout, run.stderr, outputs
