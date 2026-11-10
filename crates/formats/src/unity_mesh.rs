//! Unity Mesh objects (class 43) as plain geometry, and as a Wavefront OBJ file Blender reads.
//!
//! The fields, in order, for Unity 2019.1 through 6000 without type trees: the name; the
//! submeshes (first index byte, index count, topology, base vertex, first vertex, vertex count,
//! bounds); blend shapes (vertices, shapes, channels, full weights); bind poses; bone name hashes
//! and the root's; bone bounds; variable bone-count weights; the compression level and three
//! flags, aligned; the index format (16 or 32 bits); the index buffer; the vertex data (vertex
//! count, channels, the bytes); the compressed mesh (packed vectors); bounds, usage flags, from
//! 2022 cooking options, two baked collision meshes, two metrics; where streamed vertex data is
//! (an offset, a size and a path, often `archive:/…/….resS`).
//!
//! Everything is checked (counts that fit, a vertex layout whose size matches its bytes, indices
//! inside the vertices) so a layout this module doesn't know is refused rather than read wrong.
//! Unity is left-handed: OBJ output mirrors X and turns the faces round.

use crate::common::FormatError;
use crate::unity_serialized::{Cur, version_numbers};
use crate::unity_texture::StreamRef;
use std::fmt::Write as _;

/// The oldest Unity version whose mesh layout is read.
pub const MIN_VERSION: (u32, u32) = (2019, 1);
/// Most vertices taken from one mesh.
const MAX_VERTICES: u32 = 4_000_000;

/// One vertex channel: which stream, where in a vertex, its number format, how many numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    pub stream: u8,
    pub offset: u8,
    pub format: u8,
    pub dimension: u8,
}

/// A part of the mesh drawn with one material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubMesh {
    pub first_byte: u32,
    pub index_count: u32,
    pub topology: i32,
    pub base_vertex: u32,
}

/// A mesh's description: enough to find and read its geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshInfo {
    pub name: String,
    pub submeshes: Vec<SubMesh>,
    pub index_32: bool,
    pub index_buffer: Vec<u8>,
    pub vertex_count: u32,
    pub channels: Vec<Channel>,
    /// The vertex bytes when they're inside the object: where they start, and how many.
    pub inline: Option<(usize, usize)>,
    /// Where the vertex bytes are when they're streamed from another file.
    pub stream: Option<StreamRef>,
    /// Positions and triangles from a compressed mesh, when it is one.
    pub compressed: Option<(Vec<[f32; 3]>, Vec<u32>)>,
}

/// Geometry: positions, normals and first UVs when there are, and each submesh's triangles.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Geometry {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub parts: Vec<Vec<[u32; 3]>>,
}

impl Geometry {
    pub fn triangles(&self) -> usize {
        self.parts.iter().map(Vec::len).sum()
    }
}

fn string(c: &mut Cur, what: &str) -> Result<String, FormatError> {
    let n = c.count(what, 1)?;
    let s = String::from_utf8_lossy(c.take(n, what)?).into_owned();
    c.align4()?;
    Ok(s)
}

fn skip_array(c: &mut Cur, what: &str, each: usize) -> Result<(), FormatError> {
    let n = c.count(what, each)?;
    c.take(n * each, what).map(|_| ())
}

fn bytes<'a>(c: &mut Cur<'a>, what: &str) -> Result<(usize, &'a [u8]), FormatError> {
    let n = c.count(what, 1)?;
    let at = c.pos;
    let b = c.take(n, what)?;
    c.align4()?;
    Ok((at, b))
}

fn f32(c: &mut Cur, what: &str) -> Result<f32, FormatError> {
    Ok(f32::from_bits(c.u32(what)?))
}

/// A packed vector: (items, bits each, range, start, bytes).
struct Packed<'a> {
    items: u32,
    bits: u8,
    range: f32,
    start: f32,
    data: &'a [u8],
}

fn packed<'a>(c: &mut Cur<'a>, floats: bool, what: &str) -> Result<Packed<'a>, FormatError> {
    let items = c.u32(what)?;
    let (range, start) = if floats { (f32(c, what)?, f32(c, what)?) } else { (0.0, 0.0) };
    let (_, data) = bytes(c, what)?;
    let bits = c.u8(what)?;
    c.align4()?;
    Ok(Packed { items, bits, range, start, data })
}

impl Packed<'_> {
    fn ints(&self) -> Option<Vec<u32>> {
        let bits = u32::from(self.bits);
        if bits > 32 || (self.items > 0 && bits == 0) || u64::from(self.items) * u64::from(bits) > self.data.len() as u64 * 8 {
            return None;
        }
        let mut out = Vec::with_capacity(self.items as usize);
        for i in 0..u64::from(self.items) {
            let mut v: u64 = 0;
            for b in 0..u64::from(bits) {
                let at = i * u64::from(bits) + b;
                let byte = *self.data.get((at / 8) as usize)?;
                v |= u64::from((byte >> (at % 8)) & 1) << b;
            }
            out.push(v as u32);
        }
        Some(out)
    }

    fn floats(&self) -> Option<Vec<f32>> {
        let max = ((1u64 << self.bits) - 1).max(1) as f32;
        Some(self.ints()?.into_iter().map(|v| self.start + self.range * (v as f32 / max)).collect())
    }
}

/// Read a Mesh object written by Unity `unity_version`.
pub fn parse(object: &[u8], unity_version: &str, big_endian: bool) -> Result<MeshInfo, FormatError> {
    let v = version_numbers(unity_version).ok_or_else(|| FormatError::Malformed(format!("Unity version {unity_version:?}")))?;
    if v < MIN_VERSION {
        return Err(FormatError::Unsupported(format!("meshes from Unity {unity_version} (before 2019.1)")));
    }
    let unknown = |why: &str| FormatError::Unsupported(format!("this mesh's layout (Unity {unity_version}) isn't one that's read here ({why})"));
    let mut c = Cur { data: object, pos: 0, big: big_endian };
    let name = string(&mut c, "name")?;
    let n = c.count("submeshes", 48)?;
    let mut submeshes = Vec::with_capacity(n);
    for _ in 0..n {
        let s = SubMesh { first_byte: c.u32("submesh")?, index_count: c.u32("submesh")?, topology: c.i32("submesh")?, base_vertex: c.u32("submesh")? };
        c.take(8 + 24, "submesh")?;
        submeshes.push(s);
    }
    // Blend shapes.
    skip_array(&mut c, "blend shape vertices", 40)?;
    skip_array(&mut c, "blend shapes", 12)?;
    let channels = c.count("blend shape channels", 16)?;
    for _ in 0..channels {
        string(&mut c, "blend shape channel")?;
        c.take(12, "blend shape channel")?;
    }
    skip_array(&mut c, "blend shape weights", 4)?;
    skip_array(&mut c, "bind poses", 64)?;
    skip_array(&mut c, "bone name hashes", 4)?;
    c.u32("root bone name hash")?;
    skip_array(&mut c, "bone bounds", 24)?;
    skip_array(&mut c, "bone weights", 4)?;
    let compression = c.u8("mesh compression")?;
    c.take(3, "flags")?;
    c.align4()?;
    let index_format = c.i32("index format")?;
    if !(0..=1).contains(&index_format) || compression > 3 {
        return Err(unknown("its flags"));
    }
    let (_, index_buffer) = bytes(&mut c, "index buffer")?;
    let vertex_count = c.u32("vertex count")?;
    let nch = c.count("channels", 4)?;
    if nch > 32 || vertex_count > MAX_VERTICES {
        return Err(unknown("its vertex channels"));
    }
    let mut chans = Vec::with_capacity(nch);
    for _ in 0..nch {
        let b = c.take(4, "channel")?;
        chans.push(Channel { stream: b[0], offset: b[1], format: b[2], dimension: b[3] & 0xf });
    }
    let (data_at, data) = bytes(&mut c, "vertex data")?;
    // The compressed mesh.
    let vertices = packed(&mut c, true, "compressed vertices")?;
    for what in ["compressed uv", "compressed normals", "compressed tangents"] {
        packed(&mut c, true, what)?;
    }
    for what in ["compressed weights", "compressed normal signs", "compressed tangent signs"] {
        packed(&mut c, false, what)?;
    }
    packed(&mut c, true, "compressed colours")?;
    packed(&mut c, false, "compressed bone indices")?;
    let triangles = packed(&mut c, false, "compressed triangles")?;
    c.u32("uv info")?;
    c.take(24, "bounds")?;
    c.i32("usage flags")?;
    let rest = c.pos;
    // From 2022 cooking options sit here; the streamed data's path tells which layout it is.
    let stream = [false, true]
        .iter()
        .find_map(|cooking| {
            let mut t = Cur { data: object, pos: rest, big: big_endian };
            if *cooking {
                t.i32("cooking options").ok()?;
            }
            bytes(&mut t, "convex collision mesh").ok()?;
            bytes(&mut t, "triangle collision mesh").ok()?;
            t.take(8, "metrics").ok()?;
            t.align4().ok()?;
            let offset = t.u64("stream offset").ok()?;
            let size = t.u32("stream size").ok()?;
            let path = string(&mut t, "stream path").ok()?;
            let resource = path.starts_with("archive:") || path.ends_with(".resS") || path.ends_with(".resource");
            let plausible = (path.is_empty() && size == 0) || (resource && size > 0 && path.len() < 512 && path.chars().all(|ch| !ch.is_control()));
            // Unity 6 keeps mesh LOD details after it (not needed here).
            let ends = t.pos == object.len() || (v >= (6000, 0) && t.pos < object.len() && object.len() - t.pos <= 4096);
            (plausible && ends).then_some((offset, size, path))
        })
        .ok_or_else(|| unknown("where its vertices are"))?;
    let compressed = if vertex_count == 0 && vertices.items > 0 {
        let p = vertices.floats().ok_or_else(|| unknown("its compressed vertices"))?;
        let t = triangles.ints().ok_or_else(|| unknown("its compressed triangles"))?;
        Some((p.as_chunks::<3>().0.to_vec(), t))
    } else {
        None
    };
    let stream = (stream.1 > 0).then_some(StreamRef { path: stream.2, offset: stream.0, size: stream.1 });
    let inline = (!data.is_empty()).then_some((data_at, data.len()));
    Ok(MeshInfo {
        name,
        submeshes,
        index_32: index_format == 1,
        index_buffer: index_buffer.to_vec(),
        vertex_count,
        channels: chans,
        inline,
        stream,
        compressed,
    })
}

/// Bytes per number of a vertex format (Unity 2019 and later).
fn format_size(f: u8) -> Option<usize> {
    Some(match f {
        0 | 10 | 11 => 4,
        1 | 4 | 5 | 8 | 9 => 2,
        2 | 3 | 6 | 7 => 1,
        _ => return None,
    })
}

fn number(b: &[u8], f: u8, big: bool) -> Option<f32> {
    let u16v = |b: &[u8]| -> Option<u16> {
        let a = [*b.first()?, *b.get(1)?];
        Some(if big { u16::from_be_bytes(a) } else { u16::from_le_bytes(a) })
    };
    let u32v = |b: &[u8]| -> Option<u32> {
        let a: [u8; 4] = b.get(..4)?.try_into().ok()?;
        Some(if big { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) })
    };
    Some(match f {
        0 => f32::from_bits(u32v(b)?),
        1 => half(u16v(b)?),
        2 => f32::from(*b.first()?) / 255.0,
        3 => (f32::from(*b.first()? as i8) / 127.0).max(-1.0),
        4 => f32::from(u16v(b)?) / 65535.0,
        5 => (f32::from(u16v(b)? as i16) / 32767.0).max(-1.0),
        6 => f32::from(*b.first()?),
        7 => f32::from(*b.first()? as i8),
        8 => f32::from(u16v(b)?),
        9 => f32::from(u16v(b)? as i16),
        10 => u32v(b)? as f32,
        11 => u32v(b)? as i32 as f32,
        _ => return None,
    })
}

fn half(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let frac = f32::from(h & 0x3ff);
    sign * match exp {
        0 => frac * 2f32.powi(-24),
        31 => {
            if frac == 0.0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        e => (1.0 + frac / 1024.0) * 2f32.powi(e - 15),
    }
}

/// The geometry of `mesh`, its vertex bytes being `data` (inline or streamed).
pub fn decode(mesh: &MeshInfo, data: &[u8], big_endian: bool) -> Result<Geometry, FormatError> {
    let bad = |why: &str| FormatError::Malformed(format!("mesh {:?}: {why}", mesh.name));
    let mut g = Geometry::default();
    let indices: Vec<u32> = if let Some((positions, tris)) = &mesh.compressed {
        g.positions = positions.clone();
        tris.clone()
    } else {
        let n = mesh.vertex_count as usize;
        // Streams one after another, each aligned to 16 bytes.
        let streams = mesh.channels.iter().map(|c| usize::from(c.stream)).max().map_or(0, |m| m + 1);
        let mut starts = Vec::with_capacity(streams);
        let mut strides = Vec::with_capacity(streams);
        let mut at = 0usize;
        for s in 0..streams {
            let stride: usize = mesh
                .channels
                .iter()
                .filter(|c| usize::from(c.stream) == s && c.dimension > 0)
                .map(|c| format_size(c.format).map(|z| z * usize::from(c.dimension)))
                .sum::<Option<usize>>()
                .ok_or_else(|| bad("a vertex format that isn't known"))?;
            starts.push(at);
            strides.push(stride);
            at = (at + n * stride).div_ceil(16) * 16;
        }
        if data.len() < at.saturating_sub(15) || at == 0 {
            return Err(bad(&format!("its vertex bytes ({}) don't match its layout ({at})", data.len())));
        }
        let read = |ch: usize, v: usize| -> Option<Vec<f32>> {
            let c = mesh.channels.get(ch).filter(|c| c.dimension > 0)?;
            let size = format_size(c.format)?;
            let base = starts.get(usize::from(c.stream))? + v * strides.get(usize::from(c.stream))? + usize::from(c.offset);
            (0..usize::from(c.dimension)).map(|k| number(data.get(base + k * size..)?, c.format, big_endian)).collect()
        };
        for v in 0..n {
            let p = read(0, v).filter(|p| p.len() >= 3).ok_or_else(|| bad("a vertex without a position"))?;
            g.positions.push([p[0], p[1], p[2]]);
            if let Some(nrm) = read(1, v).filter(|x| x.len() >= 3) {
                g.normals.push([nrm[0], nrm[1], nrm[2]]);
            }
            if let Some(uv) = read(4, v).filter(|x| x.len() >= 2) {
                g.uvs.push([uv[0], uv[1]]);
            }
        }
        if g.normals.len() != n {
            g.normals.clear();
        }
        if g.uvs.len() != n {
            g.uvs.clear();
        }
        let size = if mesh.index_32 { 4 } else { 2 };
        mesh.index_buffer
            .chunks_exact(size)
            .map(|b| if size == 4 { number(b, 10, big_endian).unwrap_or(0.0) as u32 } else { number(b, 8, big_endian).unwrap_or(0.0) as u32 })
            .collect()
    };
    let size = if mesh.index_32 || mesh.compressed.is_some() { 4 } else { 2 };
    let count = g.positions.len() as u64;
    for s in &mesh.submeshes {
        if s.topology != 0 {
            continue; // lines and points aren't surfaces
        }
        let first = (s.first_byte as usize) / if mesh.compressed.is_some() { 2 } else { size };
        let idx = indices.get(first..first + s.index_count as usize).ok_or_else(|| bad("a submesh past its indices"))?;
        let mut part = Vec::with_capacity(idx.len() / 3);
        for t in idx.as_chunks::<3>().0 {
            let tri = [t[0] + s.base_vertex, t[1] + s.base_vertex, t[2] + s.base_vertex];
            if tri.iter().any(|i| u64::from(*i) >= count) {
                return Err(bad("an index past its vertices"));
            }
            part.push(tri);
        }
        g.parts.push(part);
    }
    if g.positions.iter().flatten().any(|f| !f.is_finite()) {
        return Err(bad("positions that aren't numbers"));
    }
    Ok(g)
}

/// The geometry as a Wavefront OBJ file (right-handed, Y up: X mirrored, faces turned round).
pub fn to_obj(g: &Geometry, name: &str) -> String {
    let mut s = String::with_capacity(g.positions.len() * 40 + g.triangles() * 30);
    let clean: String = name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    let _ = writeln!(s, "# Taken out of a Unity Mesh by Craft Anything\no {clean}");
    for p in &g.positions {
        let _ = writeln!(s, "v {} {} {}", -p[0], p[1], p[2]);
    }
    for t in &g.uvs {
        let _ = writeln!(s, "vt {} {}", t[0], t[1]);
    }
    for n in &g.normals {
        let _ = writeln!(s, "vn {} {} {}", -n[0], n[1], n[2]);
    }
    let (uv, nrm) = (!g.uvs.is_empty(), !g.normals.is_empty());
    for (i, part) in g.parts.iter().enumerate() {
        let _ = writeln!(s, "g {clean}_{i}");
        for t in part {
            s.push('f');
            for v in [t[0], t[2], t[1]] {
                let v = v + 1;
                match (uv, nrm) {
                    (true, true) => {
                        let _ = write!(s, " {v}/{v}/{v}");
                    }
                    (true, false) => {
                        let _ = write!(s, " {v}/{v}");
                    }
                    (false, true) => {
                        let _ = write!(s, " {v}//{v}");
                    }
                    (false, false) => {
                        let _ = write!(s, " {v}");
                    }
                }
            }
            s.push('\n');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-triangle mesh in Unity 2022.3's layout, its vertices inline.
    pub(crate) fn triangle_mesh(cooking: bool, streamed: bool) -> Vec<u8> {
        let mut b = Vec::new();
        let u32le = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_le_bytes());
        let align = |b: &mut Vec<u8>| {
            while !b.len().is_multiple_of(4) {
                b.push(0);
            }
        };
        u32le(&mut b, 3);
        b.extend_from_slice(b"Tri");
        align(&mut b);
        u32le(&mut b, 1); // one submesh
        for v in [0u32, 3, 0, 0, 0, 3] {
            u32le(&mut b, v);
        }
        b.extend_from_slice(&[0; 24]);
        for _ in 0..5 {
            u32le(&mut b, 0); // blend shape vertices, shapes, channels, weights, bind poses
        }
        u32le(&mut b, 0); // bone hashes
        u32le(&mut b, 0); // root bone
        u32le(&mut b, 0); // bone bounds
        u32le(&mut b, 0); // variable weights
        b.extend_from_slice(&[0, 1, 0, 0]); // compression, readable, keep vertices, keep indices
        u32le(&mut b, 0); // 16-bit indices
        u32le(&mut b, 6);
        b.extend_from_slice(&[0, 0, 1, 0, 2, 0]);
        align(&mut b);
        u32le(&mut b, 3); // vertices
        u32le(&mut b, 1); // one channel: position, float x3
        b.extend_from_slice(&[0, 0, 0, 3]);
        let verts: Vec<u8> = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0].iter().flat_map(|f| f.to_le_bytes()).collect();
        if streamed {
            u32le(&mut b, 0);
        } else {
            u32le(&mut b, verts.len() as u32);
            b.extend_from_slice(&verts);
        }
        // An empty compressed mesh: 4 float vectors, 3 int vectors, 1 float, 2 int.
        for floats in [true, true, true, true, false, false, false, true, false, false] {
            u32le(&mut b, 0);
            if floats {
                u32le(&mut b, 0);
                u32le(&mut b, 0);
            }
            u32le(&mut b, 0);
            b.extend_from_slice(&[0, 0, 0, 0]);
        }
        u32le(&mut b, 0); // uv info
        b.extend_from_slice(&[0; 24]);
        u32le(&mut b, 1); // usage
        if cooking {
            u32le(&mut b, 30);
        }
        u32le(&mut b, 0);
        u32le(&mut b, 0);
        b.extend_from_slice(&[0; 8]);
        if streamed {
            b.extend_from_slice(&64u64.to_le_bytes());
            u32le(&mut b, verts.len() as u32);
            let p = b"archive:/CAB-1/CAB-1.resS";
            u32le(&mut b, p.len() as u32);
            b.extend_from_slice(p);
            align(&mut b);
        } else {
            b.extend_from_slice(&0u64.to_le_bytes());
            u32le(&mut b, 0);
            u32le(&mut b, 0);
        }
        b
    }

    #[test]
    fn a_triangle_is_read_inline_or_streamed_and_written_as_obj() {
        for cooking in [false, true] {
            let bytes = triangle_mesh(cooking, false);
            let m = parse(&bytes, "2022.3.62f1", false).unwrap();
            assert_eq!((m.name.as_str(), m.vertex_count, m.submeshes.len()), ("Tri", 3, 1));
            let (at, len) = m.inline.unwrap();
            let g = decode(&m, &bytes[at..at + len], false).unwrap();
            assert_eq!(g.positions, [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]]);
            assert_eq!(g.parts, [vec![[0, 1, 2]]]);
            let obj = to_obj(&g, "Tri");
            assert!(obj.contains("v -1 0 0") && obj.contains("f 1 3 2"), "{obj}");
        }
        // Unity 6 has more after it.
        let mut six = triangle_mesh(true, true);
        six.extend_from_slice(&[0; 28]);
        assert!(parse(&six, "2022.3.62f1", false).is_err(), "not before Unity 6");
        assert_eq!(parse(&six, "6000.4.7f1", false).unwrap().stream.map(|s| s.size), Some(36));
        let s = parse(&triangle_mesh(true, true), "2022.3.62f1", false).unwrap();
        assert_eq!(s.stream.as_ref().map(|r| (r.offset, r.size, r.path.as_str())), Some((64, 36, "archive:/CAB-1/CAB-1.resS")));
        assert!(s.inline.is_none());
    }

    #[test]
    fn odd_layouts_and_old_versions_are_refused() {
        let bytes = triangle_mesh(false, false);
        assert!(matches!(parse(&bytes, "2018.4.1f1", false), Err(FormatError::Unsupported(_))));
        assert!(parse(&bytes[..bytes.len() - 6], "2022.3.62f1", false).is_err(), "cut short");
        let m = parse(&bytes, "2022.3.62f1", false).unwrap();
        assert!(decode(&m, &[0; 8], false).is_err(), "too few vertex bytes");
        let mut wrong = m.clone();
        wrong.index_buffer = vec![9, 0, 1, 0, 2, 0];
        let (at, len) = m.inline.unwrap();
        assert!(decode(&wrong, &bytes[at..at + len], false).is_err(), "an index past the vertices");
        assert_eq!(half(0x3c00), 1.0);
        assert_eq!(half(0xc000), -2.0);
    }

    #[test]
    fn packed_vectors_unpack() {
        let p = Packed { items: 3, bits: 4, range: 0.0, start: 0.0, data: &[0x21, 0x03] };
        assert_eq!(p.ints(), Some(vec![1, 2, 3]));
        let f = Packed { items: 2, bits: 8, range: 2.0, start: -1.0, data: &[0, 255] };
        assert_eq!(f.floats(), Some(vec![-1.0, 1.0]));
        assert!(Packed { items: 9, bits: 8, range: 0.0, start: 0.0, data: &[0] }.ints().is_none());
    }
}
