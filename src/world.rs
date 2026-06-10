//! Voxel world: storage, procedural generation, chunk meshing, and raycasting.
//!
//! Design: the world is a fixed 96x96-voxel mining claim (24m x 24m at 0.25m
//! voxels) sunk into the surface of a very large asteroid, extending downward
//! indefinitely. Chunks are 16^3 voxels, generated lazily as the player
//! descends. Each voxel is one byte (block id); block stats (HP, value, color,
//! name) are pure functions of (id, depth) so deeper strata get harder and
//! richer without storing anything per voxel.
//!
//! Each chunk renders as TWO meshes: a lit, vertex-colored mesh for ordinary
//! rock, and an unlit mesh whose vertex colors run hot (>1.0) for crystal
//! faces — with the HDR camera + bloom that makes ore veins glow in the dark.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Edge length of one voxel in world units (meters). A quarter meter — the
/// player is ~6 voxels tall, so destruction reads as genuinely fine-grained.
pub const VOXEL: f32 = 0.25;
pub const CHUNK: i32 = 16;
/// World is 6x6 chunk columns = 96x96 voxels = 24m x 24m.
pub const WORLD_CHUNKS_XZ: i32 = 6;
pub const WORLD_VOXELS_XZ: i32 = CHUNK * WORLD_CHUNKS_XZ;

/// Rim wall height above the surface (voxels) so you can't walk off the claim.
const WALL_TOP: i32 = 6;
/// Loose regolith thickness (layers) before solid rock starts.
const REGOLITH_LAYERS: i32 = 8;
/// Layers per difficulty band (16m). Each band doubles block HP and ~2.4x's ore value.
pub const BAND_LAYERS: i32 = 64;
/// Chance for a buried block to be crystal instead of rock.
const ORE_CHANCE: f32 = 0.05;
/// No crystal in the first couple of meters — forces an early "sell cheap carbon" loop.
const ORE_MIN_DEPTH: i32 = 10;

/// How many voxels below the player's feet we keep generated (20m).
const GEN_LOOKAHEAD_VOXELS: i32 = 80;
/// Max chunk remeshes per frame (each is ~4k voxels; keeps frame spikes down).
const REMESH_BUDGET: usize = 12;

// ---------------------------------------------------------------------------
// Block ids + stats
// ---------------------------------------------------------------------------

pub const AIR: u8 = 0;
pub const BARRIER: u8 = 1;
pub const REGOLITH: u8 = 2;
pub const ROCK: u8 = 3;
pub const ORE: u8 = 4;

const ROCK_NAMES: [&str; 8] = [
    "Chondrite", "Basalt", "Magnetite", "Hematite", "Pallasite", "Obsidian", "Deepcore",
    "Voidrock",
];
const ORE_NAMES: [&str; 8] = [
    "Carbon", "Ferrite", "Cobalt", "Titania", "Argent", "Aurium", "Cryonite", "Stellarite",
];

// Linear-RGB palettes, cycled per band. Vertex colors multiply the material's
// white base color, so these are the on-screen block colors.
const ROCK_COLORS: [[f32; 3]; 8] = [
    [0.38, 0.36, 0.34], // Chondrite — dusty grey-brown
    [0.26, 0.26, 0.29], // Basalt — dark blue-grey
    [0.33, 0.30, 0.36], // Magnetite — purple-grey
    [0.42, 0.30, 0.26], // Hematite — rust
    [0.45, 0.42, 0.34], // Pallasite — olive metal
    [0.12, 0.10, 0.16], // Obsidian — near-black violet
    [0.18, 0.24, 0.28], // Deepcore — cold teal-grey
    [0.28, 0.13, 0.28], // Voidrock — bruised purple
];
const ORE_COLORS: [[f32; 3]; 8] = [
    [0.55, 0.48, 0.34], // Carbon — warm amber glint
    [0.95, 0.45, 0.15], // Ferrite — ember orange
    [0.25, 0.45, 1.00], // Cobalt — electric blue
    [0.90, 0.90, 0.95], // Titania — white
    [0.75, 0.85, 1.00], // Argent — ice blue
    [1.00, 0.78, 0.20], // Aurium — gold
    [0.20, 0.95, 0.90], // Cryonite — cyan
    [0.95, 0.30, 0.85], // Stellarite — magenta
];

/// Depth layer of a voxel: surface layer (v.y == -1) is depth 0.
#[inline]
pub fn depth_of(v: IVec3) -> i32 {
    -1 - v.y
}

#[inline]
pub fn band_of_depth(depth: i32) -> i32 {
    (depth / BAND_LAYERS).max(0)
}

/// Max HP for a block. Doubles per band — the incremental difficulty wall.
pub fn block_hp(id: u8, depth: i32) -> f32 {
    let b = band_of_depth(depth);
    match id {
        REGOLITH => 3.0,
        ROCK => 6.0 * 2.0f32.powi(b),
        ORE => 12.0 * 2.0f32.powi(b),
        BARRIER => f32::INFINITY,
        _ => 0.0,
    }
}

/// Sale value of one crystal from the given band. Grows faster than HP (2.4x vs
/// 2.0x) so net progression accelerates as you push deeper.
pub fn ore_value(band: i32) -> u64 {
    (10.0 * 2.4f64.powi(band)).round() as u64
}

fn tier_suffix(tier: i32) -> String {
    const ROMAN: [&str; 9] = ["", " II", " III", " IV", " V", " VI", " VII", " VIII", " IX"];
    if (tier as usize) < ROMAN.len() {
        ROMAN[tier as usize].to_string()
    } else {
        format!(" x{}", tier + 1)
    }
}

pub fn ore_name(band: i32) -> String {
    let n = ORE_NAMES.len() as i32;
    format!("{}{}", ORE_NAMES[(band % n) as usize], tier_suffix(band / n))
}

pub fn rock_name(band: i32) -> String {
    let n = ROCK_NAMES.len() as i32;
    format!("{}{}", ROCK_NAMES[(band % n) as usize], tier_suffix(band / n))
}

pub fn block_display_name(id: u8, depth: i32) -> String {
    match id {
        REGOLITH => "Regolith".to_string(),
        ROCK => rock_name(band_of_depth(depth)),
        ORE => format!("{} Crystal", ore_name(band_of_depth(depth))),
        BARRIER => "Dense Core".to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Loot — every destroyed voxel drops something. Spoil sells for scraps at band
// 0; rock value doubles per band so deep spoil isn't pure trash; crystal is the
// prize.
// ---------------------------------------------------------------------------

pub fn loot_value(id: u8, band: i32) -> u64 {
    match id {
        REGOLITH => 1,
        ROCK => 1u64 << band.clamp(0, 40),
        ORE => ore_value(band),
        _ => 0,
    }
}

pub fn loot_name(id: u8, band: i32) -> String {
    match id {
        REGOLITH => "Regolith".to_string(),
        ROCK => rock_name(band),
        ORE => ore_name(band),
        _ => String::new(),
    }
}

/// Base (un-jittered) loot color for drop-item materials.
pub fn loot_color(id: u8, band: i32) -> [f32; 3] {
    let b = band.max(0) as usize;
    match id {
        REGOLITH => [0.40, 0.37, 0.33],
        ROCK => ROCK_COLORS[b % ROCK_COLORS.len()],
        ORE => ORE_COLORS[b % ORE_COLORS.len()],
        _ => [1.0, 0.0, 1.0],
    }
}

// ---------------------------------------------------------------------------
// Worldgen — pure function of voxel coords, so chunks regenerate identically.
// ---------------------------------------------------------------------------

// splitmix64 over packed coords. Cheap, deterministic, good enough scatter.
fn hash3(v: IVec3, salt: u64) -> u64 {
    let mut x = (v.x as u64 & 0x1F_FFFF)
        | ((v.y as u64 & 0x1F_FFFF) << 21)
        | ((v.z as u64 & 0x1F_FFFF) << 42);
    x = x.wrapping_add(salt).wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[inline]
fn hash_unit(v: IVec3, salt: u64) -> f32 {
    ((hash3(v, salt) >> 40) as f32) / ((1u64 << 24) as f32)
}

pub fn block_at(v: IVec3) -> u8 {
    if v.x < 0 || v.x >= WORLD_VOXELS_XZ || v.z < 0 || v.z >= WORLD_VOXELS_XZ {
        return BARRIER;
    }
    let rim =
        v.x == 0 || v.x == WORLD_VOXELS_XZ - 1 || v.z == 0 || v.z == WORLD_VOXELS_XZ - 1;
    if v.y >= 0 {
        // Above ground: air, except the rim wall.
        if rim && v.y < WALL_TOP {
            BARRIER
        } else {
            AIR
        }
    } else {
        if rim {
            return BARRIER;
        }
        let d = depth_of(v);
        if d < REGOLITH_LAYERS {
            REGOLITH
        } else if d >= ORE_MIN_DEPTH && hash_unit(v, 0xA17E) < ORE_CHANCE {
            ORE
        } else {
            ROCK
        }
    }
}

/// Per-vertex linear color for a block, with a little per-voxel brightness
/// jitter so untextured cubes don't read as a flat wall of one color.
fn block_color(id: u8, v: IVec3) -> [f32; 3] {
    let d = depth_of(v);
    let b = band_of_depth(d) as usize;
    let base = match id {
        REGOLITH => {
            if d == 0 {
                [0.37, 0.355, 0.33] // sun-bleached surface dust
            } else {
                [0.30, 0.275, 0.25] // packed regolith
            }
        }
        ROCK => ROCK_COLORS[b % ROCK_COLORS.len()],
        ORE => ORE_COLORS[b % ORE_COLORS.len()],
        BARRIER => [0.09, 0.10, 0.12],
        _ => [1.0, 0.0, 1.0],
    };
    let j = 0.85 + 0.15 * hash_unit(v, 0xC0102);
    [base[0] * j, base[1] * j, base[2] * j]
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

const CHUNK_VOL: usize = (CHUNK * CHUNK * CHUNK) as usize;

struct Chunk {
    blocks: Box<[u8; CHUNK_VOL]>,
}

#[inline]
fn chunk_of(v: IVec3) -> IVec3 {
    IVec3::new(
        v.x.div_euclid(CHUNK),
        v.y.div_euclid(CHUNK),
        v.z.div_euclid(CHUNK),
    )
}

#[inline]
fn local_index(v: IVec3) -> usize {
    let l = IVec3::new(
        v.x.rem_euclid(CHUNK),
        v.y.rem_euclid(CHUNK),
        v.z.rem_euclid(CHUNK),
    );
    (l.x + l.z * CHUNK + l.y * CHUNK * CHUNK) as usize
}

#[derive(Resource, Default)]
pub struct VoxelWorld {
    chunks: HashMap<IVec3, Chunk>,
    /// Chunks whose mesh is stale and needs a rebuild.
    pub dirty: HashSet<IVec3>,
    /// Remaining HP of partially-mined blocks. Sparse: only blocks under attack.
    pub damage: HashMap<IVec3, f32>,
}

impl VoxelWorld {
    /// Block id at a voxel. Ungenerated space below is solid (BARRIER) so the
    /// player can never fall or see through the world; high above is open air.
    pub fn block(&self, v: IVec3) -> u8 {
        if v.x < 0 || v.x >= WORLD_VOXELS_XZ || v.z < 0 || v.z >= WORLD_VOXELS_XZ {
            return BARRIER;
        }
        if v.y >= CHUNK {
            return AIR;
        }
        match self.chunks.get(&chunk_of(v)) {
            Some(c) => c.blocks[local_index(v)],
            None => BARRIER,
        }
    }

    #[inline]
    pub fn solid(&self, v: IVec3) -> bool {
        self.block(v) != AIR
    }

    pub fn has_chunk(&self, cp: IVec3) -> bool {
        self.chunks.contains_key(&cp)
    }

    pub fn generate_chunk(&mut self, cp: IVec3) {
        let mut blocks = Box::new([0u8; CHUNK_VOL]);
        let origin = cp * CHUNK;
        for y in 0..CHUNK {
            for z in 0..CHUNK {
                for x in 0..CHUNK {
                    let v = origin + IVec3::new(x, y, z);
                    blocks[local_index(v)] = block_at(v);
                }
            }
        }
        self.chunks.insert(cp, Chunk { blocks });
        self.dirty.insert(cp);
    }

    /// Remove a block (mined out). Marks the chunk — and any face-adjacent
    /// neighbor chunks — dirty so culled faces get rebuilt.
    pub fn set_air(&mut self, v: IVec3) {
        let cp = chunk_of(v);
        if let Some(c) = self.chunks.get_mut(&cp) {
            c.blocks[local_index(v)] = AIR;
        }
        self.damage.remove(&v);
        self.dirty.insert(cp);
        for d in [
            IVec3::X,
            IVec3::NEG_X,
            IVec3::Y,
            IVec3::NEG_Y,
            IVec3::Z,
            IVec3::NEG_Z,
        ] {
            let ncp = chunk_of(v + d);
            if ncp != cp && self.has_chunk(ncp) {
                self.dirty.insert(ncp);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Raycast — Amanatides & Woo voxel DDA, in voxel space.
// ---------------------------------------------------------------------------

pub struct RayHit {
    pub voxel: IVec3,
    /// Unit-axis normal of the face that was hit (points back toward the ray).
    pub normal: IVec3,
    /// Ray parameter (world units) at the hit — origin + dir * t is the
    /// point on the struck face. Used to land the laser beam exactly.
    pub t: f32,
}

pub fn raycast(world: &VoxelWorld, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RayHit> {
    let dir = dir.normalize_or_zero();
    if dir == Vec3::ZERO {
        return None;
    }
    let p = origin / VOXEL;
    let mut cell = p.floor().as_ivec3();
    let max_t = max_dist / VOXEL;

    let step = IVec3::new(
        if dir.x > 0.0 { 1 } else { -1 },
        if dir.y > 0.0 { 1 } else { -1 },
        if dir.z > 0.0 { 1 } else { -1 },
    );
    let inv = Vec3::new(
        if dir.x != 0.0 { 1.0 / dir.x } else { f32::INFINITY },
        if dir.y != 0.0 { 1.0 / dir.y } else { f32::INFINITY },
        if dir.z != 0.0 { 1.0 / dir.z } else { f32::INFINITY },
    );
    let mut t_max = Vec3::new(
        axis_t(p.x, cell.x, step.x, inv.x),
        axis_t(p.y, cell.y, step.y, inv.y),
        axis_t(p.z, cell.z, step.z, inv.z),
    );
    let t_delta = inv.abs();

    let mut normal = IVec3::ZERO;
    let mut t = 0.0f32;
    while t <= max_t {
        if world.solid(cell) {
            return Some(RayHit {
                voxel: cell,
                normal,
                t: t * VOXEL,
            });
        }
        if t_max.x <= t_max.y && t_max.x <= t_max.z {
            t = t_max.x;
            t_max.x += t_delta.x;
            cell.x += step.x;
            normal = IVec3::new(-step.x, 0, 0);
        } else if t_max.y <= t_max.z {
            t = t_max.y;
            t_max.y += t_delta.y;
            cell.y += step.y;
            normal = IVec3::new(0, -step.y, 0);
        } else {
            t = t_max.z;
            t_max.z += t_delta.z;
            cell.z += step.z;
            normal = IVec3::new(0, 0, -step.z);
        }
    }
    None
}

/// Ray parameter at which the ray leaves `cell` along one axis.
fn axis_t(p: f32, cell: i32, step: i32, inv: f32) -> f32 {
    if inv.is_infinite() {
        return f32::INFINITY;
    }
    let bound = if step > 0 { cell as f32 + 1.0 } else { cell as f32 };
    (bound - p) * inv
}

// ---------------------------------------------------------------------------
// Meshing — naive face culling, two meshes per chunk (lit rock + unlit glow),
// verts in world space.
// ---------------------------------------------------------------------------

// Per-face corner offsets (CCW from outside) + normal + baked shade factor.
// Shade fakes directional lighting so geometry reads even in headlamp-only dark.
struct Face {
    corners: [[f32; 3]; 4],
    normal: [f32; 3],
    dir: IVec3,
    shade: f32,
}

const FACES: [Face; 6] = [
    Face {
        corners: [[1., 0., 1.], [1., 0., 0.], [1., 1., 0.], [1., 1., 1.]],
        normal: [1., 0., 0.],
        dir: IVec3::X,
        shade: 0.80,
    },
    Face {
        corners: [[0., 0., 0.], [0., 0., 1.], [0., 1., 1.], [0., 1., 0.]],
        normal: [-1., 0., 0.],
        dir: IVec3::NEG_X,
        shade: 0.80,
    },
    Face {
        corners: [[0., 1., 0.], [0., 1., 1.], [1., 1., 1.], [1., 1., 0.]],
        normal: [0., 1., 0.],
        dir: IVec3::Y,
        shade: 1.00,
    },
    Face {
        corners: [[0., 0., 0.], [1., 0., 0.], [1., 0., 1.], [0., 0., 1.]],
        normal: [0., -1., 0.],
        dir: IVec3::NEG_Y,
        shade: 0.50,
    },
    Face {
        corners: [[0., 0., 1.], [1., 0., 1.], [1., 1., 1.], [0., 1., 1.]],
        normal: [0., 0., 1.],
        dir: IVec3::Z,
        shade: 0.65,
    },
    Face {
        corners: [[1., 0., 0.], [0., 0., 0.], [0., 1., 0.], [1., 1., 0.]],
        normal: [0., 0., -1.],
        dir: IVec3::NEG_Z,
        shade: 0.65,
    },
];

struct MeshScratch {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    indices: Vec<u32>,
}

impl MeshScratch {
    fn new() -> Self {
        Self {
            positions: Vec::with_capacity(4096),
            normals: Vec::with_capacity(4096),
            colors: Vec::with_capacity(4096),
            indices: Vec::with_capacity(6144),
        }
    }

    fn push_face(&mut self, base: Vec3, face: &Face, color: [f32; 4]) {
        let i0 = self.positions.len() as u32;
        for c in &face.corners {
            self.positions.push([
                base.x + c[0] * VOXEL,
                base.y + c[1] * VOXEL,
                base.z + c[2] * VOXEL,
            ]);
            self.normals.push(face.normal);
            self.colors.push(color);
        }
        self.indices
            .extend_from_slice(&[i0, i0 + 1, i0 + 2, i0, i0 + 2, i0 + 3]);
    }

    fn into_mesh(self) -> Mesh {
        Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        )
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, self.colors)
        .with_inserted_indices(Indices::U32(self.indices))
    }
}

/// Build both meshes for a chunk: (lit rock mesh, unlit HDR crystal-glow mesh).
pub fn mesh_chunk(world: &VoxelWorld, cp: IVec3) -> (Mesh, Mesh) {
    // Event-driven (not per-frame-hot): a few transient Vecs per remesh is fine.
    let mut solid = MeshScratch::new();
    let mut glow = MeshScratch::new();

    let origin = cp * CHUNK;
    for y in 0..CHUNK {
        for z in 0..CHUNK {
            for x in 0..CHUNK {
                let v = origin + IVec3::new(x, y, z);
                let id = world.block(v);
                if id == AIR {
                    continue;
                }
                let col = block_color(id, v);
                let base = (v.as_vec3()) * VOXEL;
                for face in &FACES {
                    if world.solid(v + face.dir) {
                        continue;
                    }
                    if id == ORE {
                        // Crystal faces: hot vertex colors on the unlit mesh.
                        // 1.6–3.0x pushes them past 1.0 so bloom picks them up;
                        // per-voxel jitter makes a vein shimmer, not a flat slab.
                        let h = 1.6 + 1.4 * hash_unit(v, 0x91F7);
                        glow.push_face(
                            base,
                            face,
                            [col[0] * h, col[1] * h, col[2] * h, 1.0],
                        );
                    } else {
                        let s = face.shade;
                        solid.push_face(base, face, [col[0] * s, col[1] * s, col[2] * s, 1.0]);
                    }
                }
            }
        }
    }

    (solid.into_mesh(), glow.into_mesh())
}

// ---------------------------------------------------------------------------
// Systems — chunk lifecycle
// ---------------------------------------------------------------------------

/// Solid + glow mesh entities for each chunk.
#[derive(Resource, Default)]
pub struct ChunkEntities(HashMap<IVec3, (Entity, Entity)>);

#[derive(Resource)]
pub struct WorldAssets {
    pub material: Handle<StandardMaterial>,
    pub glow_material: Handle<StandardMaterial>,
}

#[derive(Component)]
struct ChunkMesh;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VoxelWorld>()
            .init_resource::<ChunkEntities>()
            .add_systems(Startup, setup_world_assets)
            .add_systems(Update, (ensure_chunks, remesh_dirty).chain());
    }
}

fn setup_world_assets(mut commands: Commands, mut materials: ResMut<Assets<StandardMaterial>>) {
    commands.insert_resource(WorldAssets {
        material: materials.add(StandardMaterial {
            base_color: Color::WHITE,
            perceptual_roughness: 0.95,
            ..default()
        }),
        // Unlit: vertex colors pass through at full (HDR) brightness, so
        // crystals glow in pitch-dark tunnels and feed the bloom pass.
        glow_material: materials.add(StandardMaterial {
            base_color: Color::WHITE,
            unlit: true,
            ..default()
        }),
    });
}

/// Keep chunks generated from the surface down to GEN_LOOKAHEAD below the
/// player. Chunks are never despawned — a fully-hollowed column above you is
/// a few hundred cheap entities at most.
fn ensure_chunks(
    player: Res<crate::player::PlayerState>,
    mut world: ResMut<VoxelWorld>,
    mut chunk_entities: ResMut<ChunkEntities>,
    mut meshes: ResMut<Assets<Mesh>>,
    assets: Res<WorldAssets>,
    mut commands: Commands,
) {
    let feet_voxel_y = (player.pos.y / VOXEL).floor() as i32;
    let lowest_cy = (feet_voxel_y - GEN_LOOKAHEAD_VOXELS).div_euclid(CHUNK);
    for cy in (lowest_cy..=0).rev() {
        for cx in 0..WORLD_CHUNKS_XZ {
            for cz in 0..WORLD_CHUNKS_XZ {
                let cp = IVec3::new(cx, cy, cz);
                if world.has_chunk(cp) {
                    continue;
                }
                world.generate_chunk(cp);
                let mut spawn_mesh = |material: &Handle<StandardMaterial>| {
                    let handle = meshes.add(Mesh::new(
                        PrimitiveTopology::TriangleList,
                        RenderAssetUsages::default(),
                    ));
                    commands
                        .spawn((
                            Mesh3d(handle),
                            MeshMaterial3d(material.clone()),
                            Transform::IDENTITY,
                            Visibility::Hidden,
                            ChunkMesh,
                        ))
                        .id()
                };
                let solid = spawn_mesh(&assets.material);
                let glow = spawn_mesh(&assets.glow_material);
                chunk_entities.0.insert(cp, (solid, glow));
            }
        }
    }
}

fn remesh_dirty(
    mut world: ResMut<VoxelWorld>,
    chunk_entities: Res<ChunkEntities>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut chunks: Query<(&Mesh3d, &mut Visibility), With<ChunkMesh>>,
) {
    if world.dirty.is_empty() {
        return;
    }
    let batch: Vec<IVec3> = world.dirty.iter().copied().take(REMESH_BUDGET).collect();
    for cp in batch {
        world.dirty.remove(&cp);
        let Some(&(solid_e, glow_e)) = chunk_entities.0.get(&cp) else {
            continue;
        };
        let (solid_mesh, glow_mesh) = mesh_chunk(&world, cp);
        for (entity, mesh) in [(solid_e, solid_mesh), (glow_e, glow_mesh)] {
            let empty = mesh.count_vertices() == 0;
            if let Ok((mesh3d, mut vis)) = chunks.get_mut(entity) {
                // Can only fail if the handle's asset id is stale, which would
                // mean the chunk entity itself is gone — nothing useful to do.
                let _ = meshes.insert(&mesh3d.0, mesh);
                *vis = if empty {
                    Visibility::Hidden
                } else {
                    Visibility::Visible
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_world() -> VoxelWorld {
        let mut w = VoxelWorld::default();
        for cy in -3..=0 {
            for cx in 0..WORLD_CHUNKS_XZ {
                for cz in 0..WORLD_CHUNKS_XZ {
                    w.generate_chunk(IVec3::new(cx, cy, cz));
                }
            }
        }
        w
    }

    #[test]
    fn worldgen_surface_and_rim() {
        let mid = WORLD_VOXELS_XZ / 2;
        // Center of the claim: air above ground, regolith at the surface layer.
        assert_eq!(block_at(IVec3::new(mid, 0, mid)), AIR);
        assert_eq!(block_at(IVec3::new(mid, -1, mid)), REGOLITH);
        // Rim is indestructible at depth and forms a wall above the surface.
        assert_eq!(block_at(IVec3::new(0, -10, mid)), BARRIER);
        assert_eq!(block_at(IVec3::new(0, 1, mid)), BARRIER);
        // Outside the claim is treated as barrier.
        assert_eq!(block_at(IVec3::new(-1, -5, mid)), BARRIER);
        assert_eq!(block_at(IVec3::new(WORLD_VOXELS_XZ, -5, mid)), BARRIER);
    }

    #[test]
    fn worldgen_is_deterministic() {
        let v = IVec3::new(17, -40, 31);
        assert_eq!(block_at(v), block_at(v));
    }

    #[test]
    fn stats_scale_with_depth() {
        assert!(block_hp(ROCK, 100) > block_hp(ROCK, 10));
        assert!(ore_value(3) > ore_value(0));
        // Value must outgrow HP for progression to accelerate.
        let hp_ratio = block_hp(ROCK, BAND_LAYERS) / block_hp(ROCK, 0);
        let val_ratio = ore_value(1) as f32 / ore_value(0) as f32;
        assert!(val_ratio > hp_ratio);
        assert!(block_hp(BARRIER, 0).is_infinite());
    }

    #[test]
    fn raycast_hits_ground_from_above() {
        let w = test_world();
        // Standing at the spawn point looking straight down: must hit the
        // surface layer (voxel y == -1).
        let eye = Vec3::new(12.0, 1.25, 12.0);
        let hit = raycast(&w, eye, Vec3::NEG_Y, 10.0).expect("should hit ground");
        assert_eq!(hit.voxel.y, -1);
        assert_eq!(hit.normal, IVec3::Y);
        // The reported hit parameter must land the ray on the struck face.
        let p = eye + Vec3::NEG_Y * hit.t;
        assert!((p.y - 0.0).abs() < 1e-3);
        // Looking up: sky, no hit.
        assert!(raycast(&w, eye, Vec3::Y, 10.0).is_none());
    }

    #[test]
    fn set_air_opens_the_voxel_and_dirties_chunks() {
        let mut w = test_world();
        let v = IVec3::new(24, -1, 24);
        assert!(w.solid(v));
        w.dirty.clear();
        w.set_air(v);
        assert!(!w.solid(v));
        assert!(w.dirty.contains(&chunk_of(v)));
        // Surface voxel borders the above-ground chunk, which must remesh too.
        assert!(w.dirty.contains(&chunk_of(v + IVec3::Y)));
    }

    #[test]
    fn surface_chunk_meshes_nonempty() {
        let w = test_world();
        let (solid, _glow) = mesh_chunk(&w, IVec3::new(1, -1, 1));
        assert!(solid.count_vertices() > 0);
    }

    #[test]
    fn ore_faces_go_to_glow_mesh() {
        let mut w = VoxelWorld::default();
        w.generate_chunk(IVec3::new(2, -2, 2));
        // Carve out a pocket around a deep voxel and force its neighbors open
        // so whatever block is there gets faces. Then check ore voxels emit
        // glow geometry: find an ore voxel in this chunk.
        let origin = IVec3::new(2, -2, 2) * CHUNK;
        let mut ore_voxel = None;
        // Interior voxels only, so the exposing neighbor stays inside this
        // (single generated) chunk — set_air on an ungenerated chunk no-ops.
        for y in 1..CHUNK - 1 {
            for z in 1..CHUNK - 1 {
                for x in 1..CHUNK - 1 {
                    let v = origin + IVec3::new(x, y, z);
                    if w.block(v) == ORE {
                        ore_voxel = Some(v);
                    }
                }
            }
        }
        let Some(v) = ore_voxel else {
            return; // no interior ore in this chunk for this seed — fine
        };
        w.set_air(v + IVec3::X); // expose at least one face
        let (_solid, glow) = mesh_chunk(&w, IVec3::new(2, -2, 2));
        assert!(glow.count_vertices() > 0);
    }

    /// Perf probe for the "blow up a lot of cubes" future. Not a pass/fail
    /// test — prints numbers. Run with:
    ///   cargo test bench_remesh -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_remesh_and_explosion() {
        use std::time::Instant;

        let mut w = VoxelWorld::default();
        let depth_chunks = 8; // generate this many 16-voxel layers below ground

        let t = Instant::now();
        let mut all_chunks = Vec::new();
        for cy in -depth_chunks..=0 {
            for cx in 0..WORLD_CHUNKS_XZ {
                for cz in 0..WORLD_CHUNKS_XZ {
                    let cp = IVec3::new(cx, cy, cz);
                    w.generate_chunk(cp);
                    all_chunks.push(cp);
                }
            }
        }
        let gen_ms = t.elapsed().as_secs_f64() * 1000.0;

        let tri_count =
            |m: &Mesh| m.indices().map_or(0, |i| i.len() / 3);

        // Full world mesh (worst case: every chunk at once, e.g. first load).
        let t = Instant::now();
        let mut tris = 0usize;
        for &cp in &all_chunks {
            let (s, g) = mesh_chunk(&w, cp);
            tris += tri_count(&s) + tri_count(&g);
        }
        let mesh_ms = t.elapsed().as_secs_f64() * 1000.0;

        // Simulated explosion: carve a 3m-radius sphere centered 10m down.
        let half_m = WORLD_VOXELS_XZ as f32 * VOXEL * 0.5;
        let center = Vec3::new(half_m, -10.0, half_m);
        let r_m = 3.0;
        let r_vox = (r_m / VOXEL).ceil() as i32;
        let c_vox = (center / VOXEL).floor().as_ivec3();

        w.dirty.clear();
        let t = Instant::now();
        let mut carved = 0usize;
        for dy in -r_vox..=r_vox {
            for dz in -r_vox..=r_vox {
                for dx in -r_vox..=r_vox {
                    let v = c_vox + IVec3::new(dx, dy, dz);
                    let p = (v.as_vec3() + Vec3::splat(0.5)) * VOXEL;
                    if p.distance(center) <= r_m && w.block(v) != AIR && w.block(v) != BARRIER
                    {
                        w.set_air(v);
                        carved += 1;
                    }
                }
            }
        }
        let carve_ms = t.elapsed().as_secs_f64() * 1000.0;

        let dirty: Vec<IVec3> = w.dirty.iter().copied().collect();
        let t = Instant::now();
        let mut blast_tris = 0usize;
        for &cp in &dirty {
            let (s, g) = mesh_chunk(&w, cp);
            blast_tris += tri_count(&s) + tri_count(&g);
        }
        let remesh_ms = t.elapsed().as_secs_f64() * 1000.0;

        println!(
            "--- bench: voxel={VOXEL}m, world {0}x{0} voxels, {1} chunks ---",
            WORLD_VOXELS_XZ,
            all_chunks.len()
        );
        println!("worldgen:        {gen_ms:8.2} ms total");
        println!(
            "full mesh:       {mesh_ms:8.2} ms total, {tris} tris, {:.3} ms/chunk",
            mesh_ms / all_chunks.len() as f64
        );
        println!("explosion (r=3m): carved {carved} voxels in {carve_ms:.2} ms");
        println!(
            "blast remesh:    {remesh_ms:8.2} ms for {} dirty chunks ({:.3} ms/chunk), {blast_tris} tris",
            dirty.len(),
            remesh_ms / dirty.len().max(1) as f64
        );
    }
}
