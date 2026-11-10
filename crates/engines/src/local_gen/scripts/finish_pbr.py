"""Blender (headless), for Craft Anything's local generation: finish a TRELLIS.2 model (UV-unwrapped,
with baked PBR maps) for a game engine.

blender -b --python finish_pbr.py -- <job.json>
job: {"model": GLB, "maps": {"basecolor"|"metallic"|"roughness"|"normal"|"ao": PNG}, "out_base": path,
      "faces": budget, "height_m": metres, "lods": [0.5, 0.25]}
Writes <out_base>.glb (LOD0, its maps inside), <out_base>.fbx (LOD0 and <name>_LOD1, <name>_LOD2),
<out_base>_BaseColor.png, <out_base>_Normal.png (DirectX: green flipped), <out_base>_ORM.png
(R ambient occlusion, G roughness, B metallic) and <out_base>_preview.png; prints FINISH {json}.
"""
import bpy, sys, os, json, time
import numpy as np
from mathutils import Vector

job = json.load(open(sys.argv[sys.argv.index("--") + 1], encoding="utf-8"))
model = os.path.abspath(job["model"])
out_base = os.path.abspath(job["out_base"])
maps = {k: os.path.abspath(v) for k, v in (job.get("maps") or {}).items() if v and os.path.isfile(v)}
target = int(job.get("faces") or 20000)
height = float(job.get("height_m") or 1.0)
lods = [float(r) for r in job.get("lods", [0.5, 0.25])]
t0 = time.time()
res = {"maps_in": sorted(maps)}
name = os.path.basename(out_base)


def bounds(objs):
    bb = [o.matrix_world @ Vector(c) for o in objs for c in o.bound_box]
    return (Vector((min(v.x for v in bb), min(v.y for v in bb), min(v.z for v in bb))),
            Vector((max(v.x for v in bb), max(v.y for v in bb), max(v.z for v in bb))))


def select_only(objs):
    bpy.ops.object.select_all(action="DESELECT")
    for o in objs:
        o.select_set(True)
    bpy.context.view_layer.objects.active = objs[0]


def decimate(o, ratio):
    m = o.modifiers.new("dec", "DECIMATE"); m.ratio = max(min(ratio, 1.0), 0.01)
    select_only([o]); bpy.ops.object.modifier_apply(modifier="dec")


for o in list(bpy.data.objects):
    bpy.data.objects.remove(o, do_unlink=True)
bpy.ops.import_scene.gltf(filepath=model)
meshes = [o for o in bpy.context.scene.objects if o.type == "MESH"]
select_only(meshes)
if len(meshes) > 1:
    bpy.ops.object.join()
obj = bpy.context.view_layer.objects.active
bpy.ops.object.parent_clear(type="CLEAR_KEEP_TRANSFORM")
for o in list(bpy.context.scene.objects):
    if o is not obj:
        bpy.data.objects.remove(o, do_unlink=True)
obj.name = obj.data.name = name
select_only([obj]); bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
res["faces_in"] = len(obj.data.polygons)
res["uv_layers"] = len(obj.data.uv_layers)

# The budget: the server already decimated to it; only a surface well over it is collapsed further
# (collapse keeps the UVs, so the baked maps still fit).
if len(obj.data.polygons) > target * 1.05:
    for _ in range(4):
        n = len(obj.data.polygons)
        if n <= target:
            break
        decimate(obj, target / n * 0.98)
        if len(obj.data.polygons) >= n:
            break
    res["decimated"] = True
res["faces_out"] = len(obj.data.polygons)

# Size and pivot: height in metres, base on the ground, centred.
mn, mx = bounds([obj])
s = height / max(mx.z - mn.z, 1e-6)
obj.scale = (s, s, s); select_only([obj]); bpy.ops.object.transform_apply(scale=True)
mn, mx = bounds([obj])
obj.location -= Vector(((mn.x + mx.x) / 2, (mn.y + mx.y) / 2, mn.z)); bpy.ops.object.transform_apply(location=True)
mn, mx = bounds([obj])
res["size_m"] = [round(v, 3) for v in (mx - mn)]


# The maps: the PNGs the server saved, else the pictures inside the GLB's material.
def material_image(socket_name):
    for mat in obj.data.materials:
        if not mat or not mat.use_nodes:
            continue
        for n in mat.node_tree.nodes:
            if n.type != "BSDF_PRINCIPLED":
                continue
            stack = [l.from_node for l in n.inputs[socket_name].links]
            while stack:
                f = stack.pop()
                if f.type == "TEX_IMAGE" and f.image:
                    return f.image
                stack += [l.from_node for i in f.inputs for l in i.links]
    return None


def pixels(key, socket=None, channel=0):
    """A map as an (h, w, 4) float array, or None."""
    if key in maps:
        img = bpy.data.images.load(maps[key])
    elif socket:
        img = material_image(socket)
    else:
        return None
    if img is None:
        return None
    w, h = img.size
    a = np.empty(w * h * 4, dtype=np.float32); img.pixels.foreach_get(a)
    return a.reshape(h, w, 4)


def fit(a, h, w):
    """Nearest-neighbour resize to (h, w)."""
    if a.shape[0] == h and a.shape[1] == w:
        return a
    ys = (np.arange(h) * a.shape[0] // h).clip(0, a.shape[0] - 1)
    xs = (np.arange(w) * a.shape[1] // w).clip(0, a.shape[1] - 1)
    return a[ys][:, xs]


def save_png(a, path, color=True):
    h, w = a.shape[:2]
    img = bpy.data.images.new(os.path.basename(path), w, h, alpha=True)
    if not color:
        img.colorspace_settings.name = "Non-Color"
    img.pixels.foreach_set(np.ascontiguousarray(a, dtype=np.float32).ravel())
    img.filepath_raw = path; img.file_format = "PNG"; img.save()
    return os.path.getsize(path)


made = {}
base = pixels("basecolor", "Base Color")
if base is not None:
    made["BaseColor"] = save_png(base, out_base + "_BaseColor.png")
normal = pixels("normal", "Normal")
if normal is not None:
    dx = normal.copy(); dx[:, :, 1] = 1.0 - dx[:, :, 1]
    made["Normal"] = save_png(dx, out_base + "_Normal.png", color=False)
rough, metal, ao = pixels("roughness", "Roughness"), pixels("metallic", "Metallic"), pixels("ao")
size = next((a.shape[:2] for a in (base, rough, metal, ao) if a is not None), None)
if size is not None and any(a is not None for a in (rough, metal, ao)):
    h, w = size
    # The glTF importer gives metallic and roughness as one picture: roughness in G, metallic in B.
    orm = np.ones((h, w, 4), dtype=np.float32)
    orm[:, :, 0] = fit(ao, h, w)[:, :, 0] if ao is not None else 1.0
    orm[:, :, 1] = fit(rough, h, w)[:, :, 0 if "roughness" in maps else 1] if rough is not None else 1.0
    orm[:, :, 2] = fit(metal, h, w)[:, :, 0 if "metallic" in maps else 2] if metal is not None else 0.0
    made["ORM"] = save_png(orm, out_base + "_ORM.png", color=False)
res["maps_out"] = sorted(made)

# Levels of detail for the engine: copies collapsed to each ratio, named <name>_LOD1, _LOD2…
lod_objs = []
for i, r in enumerate(lods, start=1):
    o = obj.copy(); o.data = obj.data.copy(); o.name = o.data.name = f"{name}_LOD{i}"
    bpy.context.scene.collection.objects.link(o)
    decimate(o, r)
    lod_objs.append(o)
res["lod_faces"] = [len(o.data.polygons) for o in lod_objs]

# Exports: the GLB is LOD0 alone, the FBX carries every level.
os.makedirs(os.path.dirname(out_base), exist_ok=True)
select_only([obj])
bpy.ops.export_scene.gltf(filepath=out_base + ".glb", use_selection=True, export_image_format="AUTO")
select_only([obj] + lod_objs)
bpy.ops.export_scene.fbx(filepath=out_base + ".fbx", use_selection=True, apply_scale_options="FBX_SCALE_ALL", path_mode="COPY", embed_textures=True)
for o in lod_objs:
    o.hide_render = True

# Preview: three-quarter view, Cycles on the GPU, the same framing as the older route's.
scene = bpy.context.scene
scene.render.engine = "CYCLES"; scene.cycles.device = "GPU"; scene.cycles.samples = 32
scene.render.resolution_x = scene.render.resolution_y = 512
scene.render.film_transparent = False
world = bpy.data.worlds.new("w"); world.use_nodes = True; world.node_tree.nodes["Background"].inputs[0].default_value = (0.08, 0.08, 0.09, 1)
scene.world = world
cam = bpy.data.objects.new("cam", bpy.data.cameras.new("cam")); scene.collection.objects.link(cam)
d = max(res["size_m"]) * 2.2
cam.location = (d * 0.7, -d, d * 0.55 + (mx.z - mn.z) * 0.3)
target_pt = Vector((0, 0, (mx.z - mn.z) * 0.45))
cam.rotation_euler = (target_pt - cam.location).to_track_quat("-Z", "Y").to_euler()
scene.camera = cam
sun = bpy.data.objects.new("sun", bpy.data.lights.new("sun", "SUN")); sun.data.energy = 3.5
sun.rotation_euler = (0.8, 0.2, 0.6); scene.collection.objects.link(sun)
scene.render.filepath = out_base + "_preview.png"
bpy.ops.render.render(write_still=True)
res["seconds"] = round(time.time() - t0, 1)
res["files"] = {e: os.path.getsize(out_base + e) for e in (".glb", ".fbx", "_preview.png")}
res["files"].update({f"_{k}.png": v for k, v in made.items()})
print("FINISH " + json.dumps(res))
