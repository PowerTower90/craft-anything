"""Helpers every blender_run script has (Craft Anything). OUT is the run's own folder inside the
project (relative paths go there), PROJECT the project folder: write nowhere else.

clear_scene()                                   remove every object and unused data
new_material(name, rgba, metallic=0, roughness=0.5)  a Principled material
frame_camera(objs=None, view="three_quarter")   a camera aimed at objs (all meshes when None) and
                                                fitted to their bounds, with neutral lights;
                                                view: three_quarter, front, side, top
render_preview(path="preview.png", size=512)    frame (when no camera aims yet) and render a PNG
                                                with a transparent background; its full path
export_glb(path="model.glb", objs=None)         GLB of objs (everything when None); its full path
export_fbx(path="model.fbx", objs=None)         FBX the same way
result(**data)                                  print the RESULT line the tool returns
"""
import json as _json
import math as _math
import os as _os

import bpy
from mathutils import Vector as _Vector

_os.makedirs(OUT, exist_ok=True)  # noqa: F821 (set by the guard)


def _out(path):
    return path if _os.path.isabs(path) else _os.path.join(OUT, path)  # noqa: F821


def clear_scene():
    for o in list(bpy.data.objects):
        bpy.data.objects.remove(o, do_unlink=True)
    for coll in (bpy.data.meshes, bpy.data.materials, bpy.data.images, bpy.data.cameras, bpy.data.lights, bpy.data.armatures, bpy.data.actions):
        for block in list(coll):
            if block.users == 0:
                coll.remove(block)


def new_material(name, rgba=(0.8, 0.8, 0.8, 1.0), metallic=0.0, roughness=0.5):
    m = bpy.data.materials.new(name)
    m.diffuse_color = tuple(rgba)
    try:
        m.use_nodes = True
    except AttributeError:
        pass
    bsdf = m.node_tree.nodes.get("Principled BSDF") if m.node_tree else None
    if bsdf:
        bsdf.inputs["Base Color"].default_value = tuple(rgba)
        bsdf.inputs["Metallic"].default_value = metallic
        bsdf.inputs["Roughness"].default_value = roughness
    return m


def _bounds(objs):
    lo, hi = _Vector((_math.inf,) * 3), _Vector((-_math.inf,) * 3)
    for o in objs:
        for c in o.bound_box:
            w = o.matrix_world @ _Vector(c)
            lo = _Vector(map(min, lo, w))
            hi = _Vector(map(max, hi, w))
    return lo, hi


_VIEWS = {"three_quarter": (1.0, -1.0, 0.75), "front": (0.0, -1.0, 0.15), "side": (1.0, 0.0, 0.15), "top": (0.0, -0.01, 1.0)}


def frame_camera(objs=None, view="three_quarter"):
    scene = bpy.context.scene
    objs = [o for o in (objs or scene.objects) if o.type == "MESH"]
    if not objs:
        raise ValueError("frame_camera: there's no mesh to look at")
    for o in [o for o in scene.objects if o.get("craft_preview")]:
        bpy.data.objects.remove(o, do_unlink=True)
    lo, hi = _bounds(objs)
    centre, radius = (lo + hi) / 2, max((hi - lo).length / 2, 1e-4)
    data = bpy.data.cameras.new("PreviewCam")
    data.lens, data.clip_start, data.clip_end = 50, radius / 100, radius * 100
    cam = bpy.data.objects.new("PreviewCam", data)
    cam["craft_preview"] = True
    scene.collection.objects.link(cam)
    direction = _Vector(_VIEWS.get(view, _VIEWS["three_quarter"])).normalized()
    cam.location = centre + direction * (radius / _math.sin(data.angle / 2) * 1.08)
    cam.rotation_euler = (centre - cam.location).to_track_quat("-Z", "Y").to_euler()
    scene.camera = cam
    for name, energy, rot in (("Key", 2.2, (0.87, 0, 0.52)), ("Fill", 0.6, (1.05, 0, -2.1))):
        light = bpy.data.objects.new(name, bpy.data.lights.new(name, "SUN"))
        light.data.energy = energy
        light.rotation_euler = rot
        light["craft_preview"] = True
        scene.collection.objects.link(light)
    return cam


def render_preview(path="preview.png", size=512, transparent=True):
    scene = bpy.context.scene
    for engine in ("BLENDER_EEVEE_NEXT", "BLENDER_EEVEE", "CYCLES"):
        try:
            scene.render.engine = engine
            break
        except TypeError:
            continue
    if scene.camera is None:
        frame_camera()
    scene.render.resolution_x = scene.render.resolution_y = int(size)
    scene.render.resolution_percentage = 100
    scene.render.film_transparent = transparent
    scene.render.image_settings.file_format = "PNG"
    scene.render.filepath = _out(path)
    bpy.ops.render.render(write_still=True)
    return scene.render.filepath


def _select(objs):
    if objs is None:
        return False
    bpy.ops.object.select_all(action="DESELECT")
    for o in objs:
        o.select_set(True)
    return True


def export_glb(path="model.glb", objs=None):
    full = _out(path)
    bpy.ops.export_scene.gltf(filepath=full, export_format="GLB", use_selection=_select(objs))
    return full


def export_fbx(path="model.fbx", objs=None):
    full = _out(path)
    bpy.ops.export_scene.fbx(filepath=full, use_selection=_select(objs))
    return full


def result(**data):
    print("RESULT " + _json.dumps(data, default=str), flush=True)
