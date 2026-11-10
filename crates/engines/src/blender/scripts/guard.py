"""Runs a Python script in headless Blender for Craft Anything, keeping its writes in bounds.

usage: blender -b --python-exit-code 1 --python guard.py -- <job.json>

The job is {"script": "<file>", "argv": [...], "writable": ["<folder>", ...], "out": "<folder>",
"preamble": "<file>"}; with "out", relative paths given to Blender's writers go there, and the
preamble (helpers) runs first in the script's own namespace, with OUT and PROJECT set. Before the script
runs, a Python audit hook refuses (PermissionError) every file write, removal, rename or new folder
outside the writable folders, and every program start; Blender's own writers called through
bpy.ops (exporters, saving, rendering to a file) are checked the same way by their file path.
wm.read_factory_settings is refused: it drops the person's add-ons and GPU setup.

The script runs as __main__ with sys.argv = [script, *argv]. It says what it found by printing one
line "RESULT {json}". Nothing here is a full sandbox: it keeps an honest script from writing in the
wrong place, not a hostile one.
"""
import json
import os
import sys

job_path = sys.argv[sys.argv.index("--") + 1]
with open(job_path, encoding="utf-8") as f:
    JOB = json.load(f)
sys.dont_write_bytecode = True

def _norm(p):
    return os.path.normcase(os.path.abspath(os.path.realpath(p)))

WRITABLE = [_norm(p) for p in JOB.get("writable", [])]
OUT = JOB.get("out")


def allowed(path):
    try:
        if isinstance(path, int):
            return True  # an open file descriptor: its opening was checked
        if isinstance(path, bytes):
            path = os.fsdecode(path)
        p = _norm(os.fspath(path))
    except Exception:
        return False
    return any(p == w or p.startswith(w.rstrip("\\/") + os.sep) for w in WRITABLE)


def refuse(what, path):
    raise PermissionError(f"Craft Anything: {what} {path!r} is outside the project; write inside the project folder.")


WRITE_FLAGS = os.O_WRONLY | os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_TRUNC
PATH_EVENTS = {
    "os.remove": "removing",
    "os.rmdir": "removing",
    "os.mkdir": "making a folder at",
    "os.chmod": "changing",
    "os.truncate": "changing",
    "os.utime": "changing",
    "shutil.rmtree": "removing",
    "shutil.copyfile": None,
    "shutil.move": None,
}
PROCESS_EVENTS = {"subprocess.Popen", "os.system", "os.exec", "os.posix_spawn", "os.spawn", "os.startfile", "os.fork", "os.forkpty"}


def hook(event, args):
    if event == "open":
        path, mode, flags = (list(args) + [None, None, None])[:3]
        writes = (isinstance(mode, str) and any(c in mode for c in "wax+")) or (isinstance(flags, int) and flags & WRITE_FLAGS)
        if writes and path is not None and not allowed(path):
            refuse("writing", path)
    elif event in ("os.rename", "os.link", "os.symlink"):
        if len(args) > 1 and not allowed(args[1]):
            refuse("writing", args[1])
        if event == "os.rename" and args and not allowed(args[0]):
            refuse("moving", args[0])
    elif event in PATH_EVENTS:
        target = args[-1] if PATH_EVENTS[event] is None else args[0]
        if args and not allowed(target):
            refuse(PATH_EVENTS[event] or "writing", target)
    elif event in PROCESS_EVENTS:
        raise PermissionError("Craft Anything: scripts can't start other programs.")


sys.addaudithook(hook)

import bpy  # noqa: E402

WRITERS = ("export", "save", "write", "bake", "opengl")
PATH_KEYS = ("filepath", "directory", "filename")


def check_op(idname, kw):
    if idname in ("wm.read_factory_settings", "wm.read_factory_userpref"):
        raise PermissionError("Craft Anything: read_factory_settings is refused (it drops the add-ons and the GPU setup).")
    writes = any(w in idname for w in WRITERS)
    if writes:
        for k in PATH_KEYS:
            v = kw.get(k)
            if OUT and isinstance(v, str) and v and not os.path.isabs(v) and not v.startswith("//"):
                kw[k] = v = os.path.join(OUT, v)
            if isinstance(v, str) and v and not allowed(v):
                refuse("writing", v)
        if idname == "wm.save_mainfile" and "filepath" not in kw and bpy.data.filepath and not allowed(bpy.data.filepath):
            refuse("saving", bpy.data.filepath)
    if idname == "render.render" and kw.get("write_still"):
        scene = bpy.context.scene
        if OUT and not os.path.isabs(scene.render.filepath) and not scene.render.filepath.startswith("//"):
            scene.render.filepath = os.path.join(OUT, scene.render.filepath or "render.png")
        out = bpy.path.abspath(scene.render.filepath)
        if not allowed(out):
            refuse("rendering to", out)


class _Guarded:
    """An operator, checked before it runs (Blender 4.4 and later make operators in C)."""

    def __init__(self, module, func, op):
        self._idname, self._op = f"{module}.{func}", op

    def __call__(self, *args, **kw):
        check_op(self._idname, kw)
        return self._op(*args, **kw)

    def __getattr__(self, name):
        return getattr(self._op, name)


try:
    if hasattr(bpy.ops, "_op_create_function"):
        _create = bpy.ops._op_create_function
        bpy.ops._op_create_function = lambda module, func: _Guarded(module, func, _create(module, func))
    else:
        _op = bpy.ops._BPyOpsSubModOp
        _call = _op.__call__

        def _guarded(self, *args, **kw):
            check_op(self.idname_py(), kw)
            return _call(self, *args, **kw)

        _op.__call__ = _guarded
except Exception as e:  # another Blender: the audit hook still guards Python writes
    print(f"Craft Anything: bpy.ops isn't guarded here ({e}).", file=sys.stderr)

script = JOB["script"]
sys.argv = [script] + [str(a) for a in JOB.get("argv", [])]
namespace = {"__name__": "__main__", "__file__": script, "OUT": OUT, "PROJECT": (JOB.get("writable") or [None])[0]}
if JOB.get("preamble"):
    with open(JOB["preamble"], encoding="utf-8") as f:
        exec(compile(f.read(), JOB["preamble"], "exec"), namespace)
with open(script, encoding="utf-8") as f:
    code = compile(f.read(), script, "exec")
exec(code, namespace)
