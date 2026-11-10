"""Craft Anything's model helpers for headless Blender (run through guard.py).

usage (argv after the script):
  inspect <model file>          prints RESULT {objects, meshes, faces, triangles, vertices, materials,
                                textures, armatures, bones, animations, bounds}
  render <job.json>             job {"items": [{"id", "input", "output"}], "size": 256}: a preview
                                of each model, three-quarter view, neutral light, transparent
                                background; prints PREVIEW {id, ok, error, seconds} per item and
                                RESULT {made, failed, engine} at the end

Models: .glb .gltf .fbx .obj .stl .ply .blend (.dae when the Blender has Collada).
"""
import json
import math
import os
import sys
import time

import bpy
from mathutils import Vector


def clear():
    for o in list(bpy.data.objects):
        bpy.data.objects.remove(o, do_unlink=True)
    for coll in (bpy.data.meshes, bpy.data.materials, bpy.data.images, bpy.data.armatures, bpy.data.actions, bpy.data.cameras, bpy.data.lights):
        for block in list(coll):
            if block.users == 0:
                coll.remove(block)


def load(path):
    ext = os.path.splitext(path)[1].lower()
    if ext in (".glb", ".gltf"):
        bpy.ops.import_scene.gltf(filepath=path)
    elif ext == ".fbx":
        bpy.ops.import_scene.fbx(filepath=path)
    elif ext == ".obj":
        bpy.ops.wm.obj_import(filepath=path)
    elif ext == ".stl":
        bpy.ops.wm.stl_import(filepath=path)
    elif ext == ".ply":
        bpy.ops.wm.ply_import(filepath=path)
    elif ext == ".dae":
        bpy.ops.wm.collada_import(filepath=path)
    elif ext == ".blend":
        with bpy.data.libraries.load(path, link=False) as (src, dst):
            dst.objects = list(src.objects)
        for o in dst.objects:
            if o is not None:
                bpy.context.scene.collection.objects.link(o)
    else:
        raise ValueError(f"{ext or 'this file'} isn't a model format Blender reads here")


def bounds(objs):
    lo, hi = Vector((math.inf,) * 3), Vector((-math.inf,) * 3)
    for o in objs:
        for c in o.bound_box:
            w = o.matrix_world @ Vector(c)
            lo = Vector(map(min, lo, w))
            hi = Vector(map(max, hi, w))
    if lo.x == math.inf:
        return None
    return lo, hi


def inspect(path):
    clear()
    load(path)
    deps = bpy.context.evaluated_depsgraph_get()
    meshes = [o for o in bpy.context.scene.objects if o.type == "MESH"]
    faces = tris = verts = 0
    for o in meshes:
        m = o.evaluated_get(deps).data
        faces += len(m.polygons)
        verts += len(m.vertices)
        m.calc_loop_triangles()
        tris += len(m.loop_triangles)
    arm = [o for o in bpy.context.scene.objects if o.type == "ARMATURE"]
    b = bounds(meshes)
    out = {
        "objects": [{"name": o.name, "type": o.type} for o in bpy.context.scene.objects][:200],
        "object_count": len(bpy.context.scene.objects),
        "meshes": len(meshes),
        "faces": faces,
        "triangles": tris,
        "vertices": verts,
        "materials": sorted({s.material.name for o in meshes for s in o.material_slots if s.material})[:100],
        "textures": [{"name": i.name, "size": list(i.size)} for i in bpy.data.images if i.type == "IMAGE"][:100],
        "armatures": len(arm),
        "bones": sum(len(o.data.bones) for o in arm),
        "animations": [{"name": a.name, "frames": [round(a.frame_range[0], 2), round(a.frame_range[1], 2)]} for a in bpy.data.actions][:100],
        "bounds": None if b is None else {"min": [round(v, 4) for v in b[0]], "max": [round(v, 4) for v in b[1]], "size": [round(v, 4) for v in (b[1] - b[0])]},
    }
    print("RESULT " + json.dumps(out), flush=True)


def set_engine(scene):
    for engine in ("BLENDER_EEVEE_NEXT", "BLENDER_EEVEE", "CYCLES"):
        try:
            scene.render.engine = engine
            break
        except TypeError:
            continue
    if scene.render.engine == "CYCLES":
        scene.cycles.samples = 16
        scene.cycles.device = "GPU"
    elif hasattr(scene, "eevee"):
        scene.eevee.taa_render_samples = 16
    return scene.render.engine


def stage(scene, size):
    scene.render.resolution_x = scene.render.resolution_y = size
    scene.render.resolution_percentage = 100
    scene.render.film_transparent = True
    scene.render.image_settings.file_format = "PNG"
    scene.render.image_settings.color_mode = "RGBA"
    try:
        scene.view_settings.view_transform = "Standard"
    except TypeError:
        pass
    world = scene.world or bpy.data.worlds.new("Preview")
    scene.world = world
    world.use_nodes = True
    bg = world.node_tree.nodes.get("Background")
    if bg:
        bg.inputs[0].default_value = (0.6, 0.6, 0.62, 1)
        bg.inputs[1].default_value = 0.35


GREY = None


def grey():
    global GREY
    if GREY is None or GREY.name not in bpy.data.materials:
        GREY = bpy.data.materials.new("PreviewGrey")
        GREY.diffuse_color = (0.5, 0.5, 0.52, 1)
        try:
            GREY.use_nodes = True
        except AttributeError:
            pass
        bsdf = GREY.node_tree.nodes.get("Principled BSDF") if GREY.node_tree else None
        if bsdf:
            bsdf.inputs["Base Color"].default_value = (0.5, 0.5, 0.52, 1)
            bsdf.inputs["Roughness"].default_value = 0.6
        GREY.use_fake_user = True
    return GREY


def shoot(scene, out):
    meshes = [o for o in scene.objects if o.type == "MESH"]
    if not meshes:
        raise ValueError("it has no mesh")
    for o in meshes:
        if not any(s.material for s in o.material_slots):
            o.data.materials.append(grey())
    lo, hi = bounds(meshes)
    centre = (lo + hi) / 2
    radius = max((hi - lo).length / 2, 1e-4)
    cam_data = bpy.data.cameras.new("PreviewCam")
    cam_data.lens = 50
    cam_data.clip_start = radius / 100
    cam_data.clip_end = radius * 100
    cam = bpy.data.objects.new("PreviewCam", cam_data)
    scene.collection.objects.link(cam)
    direction = Vector((1.0, -1.0, 0.75)).normalized()
    dist = radius / math.sin(cam_data.angle / 2) * 1.08
    cam.location = centre + direction * dist
    cam.rotation_euler = (centre - cam.location).to_track_quat("-Z", "Y").to_euler()
    scene.camera = cam
    for name, energy, rot in (("Key", 2.2, (math.radians(50), 0, math.radians(30))), ("Fill", 0.6, (math.radians(60), 0, math.radians(-120)))):
        light = bpy.data.lights.new(name, "SUN")
        light.energy = energy
        lo_ = bpy.data.objects.new(name, light)
        lo_.rotation_euler = rot
        scene.collection.objects.link(lo_)
    scene.render.filepath = out
    bpy.ops.render.render(write_still=True)


def render(job_path):
    with open(job_path, encoding="utf-8") as f:
        job = json.load(f)
    scene = bpy.context.scene
    engine = set_engine(scene)
    stage(scene, int(job.get("size", 256)))
    made = failed = 0
    for item in job["items"]:
        t0 = time.time()
        res = {"id": item["id"], "ok": False}
        try:
            clear()
            load(item["input"])
            os.makedirs(os.path.dirname(os.path.abspath(item["output"])), exist_ok=True)
            shoot(scene, item["output"])
            res["ok"] = os.path.isfile(item["output"])
            if not res["ok"]:
                res["error"] = "the render wasn't written"
        except Exception as e:  # one model failing doesn't stop the rest
            res["error"] = f"{type(e).__name__}: {e}"[:300]
        res["seconds"] = round(time.time() - t0, 2)
        made += res["ok"]
        failed += not res["ok"]
        print("PREVIEW " + json.dumps(res), flush=True)
    print("RESULT " + json.dumps({"made": made, "failed": failed, "engine": engine}), flush=True)


if __name__ == "__main__":
    mode, arg = sys.argv[1], sys.argv[2]
    if mode == "inspect":
        inspect(arg)
    elif mode == "render":
        render(arg)
    else:
        raise SystemExit(f"unknown mode {mode}")
