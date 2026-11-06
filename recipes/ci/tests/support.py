"""Shared helpers for the CI recipe script tests."""

import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile


def _find_scripts():
    """The scripts' directory: `../scripts` in bootcher's repo, `../ci` in a repo that
    copied them to `ci/` with `tests/` beside it, or `..` with `tests/` inside `ci/`."""
    here = pathlib.Path(__file__).resolve().parent
    for candidate in (here.parent / "scripts", here.parent / "ci", here.parent):
        if (candidate / "base_check.py").exists():
            return candidate
    raise FileNotFoundError(f"no CI scripts found next to {here}")


SCRIPTS = _find_scripts()


def load_script(name):
    """Import `scripts/<name>.py` as a module (its `main()` doesn't run on import)."""
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


# A stand-in for `skopeo`: answers `skopeo inspect <args>` from a JSON table mapping
# the space-joined args to {"json": ...} (printed as JSON), {"raw": "..."} (printed
# as is) or {"error": ...} (exit 1).
_FAKE_SKOPEO = """#!/usr/bin/env python3
import json, os, sys
table = json.load(open(os.environ["FAKE_SKOPEO_TABLE"]))
reply = table.get(" ".join(sys.argv[2:]), {"error": "manifest unknown"})
if "error" in reply:
    sys.stderr.write(reply["error"] + "\\n")
    sys.exit(1)
print(reply["raw"] if "raw" in reply else json.dumps(reply["json"]))
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
        # A minimal env, so the caller's CHANNEL, GITHUB_OUTPUT, ... can't leak in.
        full_env = {k: os.environ[k] for k in ("HOME", "LANG", "LC_ALL") if k in os.environ}
        full_env.update(PATH=f"{bin_dir}:{os.environ.get('PATH', '/usr/bin:/bin')}",
                        FAKE_SKOPEO_TABLE=str(table))
        full_env.update({k: v.replace("{dir}", str(work)) for k, v in (env or {}).items()})
        run = subprocess.run([sys.executable, "-B", str(SCRIPTS / f"{name}.py")], cwd=work,
                             env=full_env, capture_output=True, text=True)
        outputs = {}
        if (work / "base.env").exists():
            outputs["base.env"] = (work / "base.env").read_text()
        if (work / "github_output").exists():
            outputs["github"] = (work / "github_output").read_text()
        return run.returncode, run.stdout, run.stderr, outputs
