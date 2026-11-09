"""Blender (headless), for Craft Anything's local generation: turn a generated shape into a
game-ready model textured from its reference picture.

blender -b --python finish_model.py -- <shape.glb> <reference_rgba.png> <out_base> [target_faces] [height_m]
Writes <out_base>.glb, <out_base>.fbx and <out_base>_preview.png, and prints FINISH {json}.
The reference is a front view, ideally with a transparent background (else the subject is what isn't near-white);
it is projected along the view axis onto the front and mirrored onto the back, so every face gets the picture's
colours (sides take the outline's colours).
"""
import bpy, bmesh, sys, os, json, time
import numpy as np
from mathutils import Vector

argv = sys.argv[sys.argv.index("--") + 1:]
shape, ref, out_base = (os.path.abspath(p) for p in argv[:3])
target = int(argv[3]) if len(argv) > 3 else 20000
height = float(argv[4]) if len(argv) > 4 else 1.0
t0 = time.time()
res = {}


def bounds(o):
    bb = [o.matrix_world @ Vector(c) for c in o.bound_box]
    return (Vector((min(v.x for v in bb), min(v.y for v in bb), min(v.z for v in bb))),
            Vector((max(v.x for v in bb), max(v.y for v in bb), max(v.z for v in bb))))


for o in list(bpy.data.objects):
    bpy.data.objects.remove(o, do_unlink=True)
bpy.ops.import_scene.gltf(filepath=shape)
meshes = [o for o in bpy.context.scene.objects if o.type == "MESH"]
bpy.ops.object.select_all(action="DESELECT")
for o in meshes:
    o.select_set(True)
bpy.context.view_layer.objects.active = meshes[0]
if len(meshes) > 1:
    bpy.ops.object.join()
obj = bpy.context.view_layer.objects.active
bpy.ops.object.parent_clear(type="CLEAR_KEEP_TRANSFORM")
for o in list(bpy.context.scene.objects):
    if o is not obj:
        bpy.data.objects.remove(o, do_unlink=True)
obj.name = os.path.basename(out_base)
bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
res["faces_in"] = len(obj.data.polygons)

# Clean: merge doubles, consistent normals, then decimate to the budget.
bm = bmesh.new(); bm.from_mesh(obj.data)
bmesh.ops.remove_doubles(bm, verts=bm.verts, dist=1e-5)
bm.to_mesh(obj.data); bm.free()
bpy.ops.object.mode_set(mode="EDIT"); bpy.ops.mesh.select_all(action="SELECT")
bpy.ops.mesh.normals_make_consistent(inside=False); bpy.ops.object.mode_set(mode="OBJECT")
# A generated shape is millions of faces on a surface that collapse decimation can barely touch
# (one pass left 2.5 million of 3 million), so it's rebuilt first as a clean, closed surface of
# about four times the budget (voxel remesh), then decimated to the budget.
if len(obj.data.polygons) > target:
    d = obj.dimensions
    area = 2 * (d.x * d.y + d.y * d.z + d.x * d.z)
    rm = obj.modifiers.new("remesh", "REMESH"); rm.mode = "VOXEL"
    rm.voxel_size = max((area / (target * 4)) ** 0.5, 1e-4)
    bpy.ops.object.modifier_apply(modifier="remesh")
    res["remeshed"] = True
# Collapse passes until it's under the budget (or stops shrinking).
for _ in range(8):
    n = len(obj.data.polygons)
    if n <= target:
        break
    m = obj.modifiers.new("dec", "DECIMATE"); m.ratio = max(min(target / n * 0.97, 0.99), 0.01)
    bpy.ops.object.modifier_apply(modifier="dec")
    if len(obj.data.polygons) >= n:
        break
bpy.ops.object.shade_smooth_by_angle(angle=0.6)
res["faces_out"] = len(obj.data.polygons)

# Size and pivot: height in metres, base on the ground, centred.
mn, mx = bounds(obj)
s = height / max(mx.z - mn.z, 1e-6)
obj.scale = (s, s, s); bpy.ops.object.transform_apply(scale=True)
mn, mx = bounds(obj)
obj.location -= Vector(((mn.x + mx.x) / 2, (mn.y + mx.y) / 2, mn.z)); bpy.ops.object.transform_apply(location=True)
mn, mx = bounds(obj)
res["size_m"] = [round(v, 3) for v in (mx - mn)]

# The subject's box in the picture (alpha > 0.5).
img = bpy.data.images.load(ref)
w, h = img.size
px = np.array(img.pixels[:], dtype=np.float32).reshape(h, w, 4)  # rows bottom-up
mask = px[:, :, 3] > 0.5
if mask.all():
    # No transparency: the subject is what isn't (near) white.
    mask = px[:, :, :3].min(axis=2) < 0.94
ys, xs = np.nonzero(mask)
if len(xs) == 0:
    x0, x1, y0, y1 = 0, w, 0, h
else:
    x0, x1, y0, y1 = xs.min(), xs.max() + 1, ys.min(), ys.max() + 1
res["subject_px"] = [int(x0), int(y0), int(x1), int(y1)]

# Planar UVs from the front: Blender's front view looks along +Y, so u follows x and v follows z,
# fitted to the subject's box; faces pointing away (+Y) get the mirrored picture.
me = obj.data
uv = me.uv_layers.new(name="UVMap")
for poly in me.polygons:
    back = poly.normal.y > 0.2
    for li in poly.loop_indices:
        co = me.vertices[me.loops[li].vertex_index].co
        fx = (co.x - mn.x) / max(mx.x - mn.x, 1e-6)
        fz = (co.z - mn.z) / max(mx.z - mn.z, 1e-6)
        if back:
            fx = 1.0 - fx
        uv.data[li].uv = ((x0 + fx * (x1 - x0)) / w, (y0 + fz * (y1 - y0)) / h)

mat = bpy.data.materials.new(obj.name + "_Mat"); mat.use_nodes = True
nt = mat.node_tree; bsdf = nt.nodes["Principled BSDF"]
tex = nt.nodes.new("ShaderNodeTexImage"); tex.image = img; tex.extension = "EXTEND"
nt.links.new(tex.outputs["Color"], bsdf.inputs["Base Color"])
bsdf.inputs["Roughness"].default_value = 0.8
me.materials.clear(); me.materials.append(mat)

# Exports.
os.makedirs(os.path.dirname(os.path.abspath(out_base)), exist_ok=True)
bpy.ops.object.select_all(action="DESELECT"); obj.select_set(True); bpy.context.view_layer.objects.active = obj
bpy.ops.export_scene.gltf(filepath=out_base + ".glb", use_selection=True, export_image_format="AUTO")
bpy.ops.export_scene.fbx(filepath=out_base + ".fbx", use_selection=True, apply_scale_options="FBX_SCALE_ALL",
                         path_mode="COPY", embed_textures=True)

# Preview: three-quarter view, Cycles on the GPU.
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
print("FINISH " + json.dumps(res))
