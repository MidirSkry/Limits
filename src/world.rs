//! Voxel world: an open asteroid field. Storage, procedural generation,
//! chunk meshing, and raycasting.
//!
//! Design: space is unbounded, chunked 16³ in all directions. A deterministic
//! hash places one asteroid (or none) in each 56m cell of space; each is a
//! lumpy fbm-displaced sphere with a regolith shell, rock body, ore odds that
//! rise toward the center, and a pure-crystal core. The "home" asteroid sits
//! at the origin with the depot on top. Progression: asteroids further from
//! home are higher tier — harder rock, richer crystal.
//!
//! Worldgen is a pure function of voxel coords (`block_at`), so chunks store
//! one byte per voxel and can be thrown away when the player flies off —
//! player edits (mined voxels) live in a sparse overlay that survives
//! unloading and is re-applied on regeneration.
//!
//! Each chunk renders as TWO meshes: a lit, vertex-colored mesh for ordinary
//! rock, and an unlit mesh whose vertex colors run hot (>1.0) for crystal
//! faces — with the HDR camera + bloom that makes ore veins glow in the dark.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Edge length of one voxel in world units (meters).
pub const VOXEL: f32 = 0.25;
pub const CHUNK: i32 = 16;
const CHUNK_M: f32 = CHUNK as f32 * VOXEL; // 4m

/// Asteroid field: one cell of space may hold one asteroid.
pub const CELL_M: f32 = 56.0;
/// Chance a cell hosts an asteroid. Tuned with the impostor LOD in view:
/// dense enough that a neighbor is always a short flight away, sparse enough
/// that the sky reads as a field, not foam.
const CELL_DENSITY: f32 = 0.20;
/// Asteroid radii (pre-displacement), small ones common, big ones rare.
const R_MIN: f32 = 5.0;
const R_MAX: f32 = 17.0;
/// Radial displacement: surface = r * (BASE + AMP * fbm(dir)).
const DISP_BASE: f32 = 0.84;
const DISP_AMP: f32 = 0.16;
/// The home rock.
pub const HOME_R: f32 = 15.0;
/// Distance from home per difficulty tier ("sector").
pub const TIER_M: f32 = 120.0;

/// Regolith shell thickness (m) on every asteroid.
const SHELL_M: f32 = 0.6;
/// Crystal core: inside this fraction of the surface radius it's all ore.
const CORE_FRAC: f32 = 0.15;
const ORE_CHANCE_BASE: f32 = 0.035;
/// Extra ore chance at the very center (falls off quadratically outward).
const ORE_CHANCE_CORE: f32 = 0.10;

/// Chunk generation radius around the player (chunks) and the larger radius
/// beyond which chunks are unloaded.
const GEN_RADIUS: i32 = 14;
const UNLOAD_RADIUS: i32 = 20;
/// Max chunks generated per frame (nearest first).
const GEN_BUDGET: usize = 28;
/// Max chunk remeshes per frame.
const REMESH_BUDGET: usize = 12;

// ---------------------------------------------------------------------------
// Block ids + stats
// ---------------------------------------------------------------------------

pub const AIR: u8 = 0;
#[allow(dead_code)]
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

// Linear-RGB palettes, cycled per tier. Vertex colors multiply the material's
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

/// Max HP for a block. Doubles per tier — the incremental difficulty wall.
pub fn block_hp(id: u8, tier: i32) -> f32 {
    match id {
        REGOLITH => 3.0,
        ROCK => 6.0 * 2.0f32.powi(tier),
        ORE => 12.0 * 2.0f32.powi(tier),
        BARRIER => f32::INFINITY,
        _ => 0.0,
    }
}

/// Sale value of one crystal from the given tier. Grows faster than HP (2.4x
/// vs 2.0x) so net progression accelerates as you push out.
pub fn ore_value(tier: i32) -> u64 {
    (10.0 * 2.4f64.powi(tier)).round() as u64
}

fn tier_suffix(t: i32) -> String {
    const ROMAN: [&str; 9] = ["", " II", " III", " IV", " V", " VI", " VII", " VIII", " IX"];
    if (t as usize) < ROMAN.len() {
        ROMAN[t as usize].to_string()
    } else {
        format!(" x{}", t + 1)
    }
}

pub fn ore_name(tier: i32) -> String {
    let n = ORE_NAMES.len() as i32;
    format!("{}{}", ORE_NAMES[(tier % n) as usize], tier_suffix(tier / n))
}

pub fn rock_name(tier: i32) -> String {
    let n = ROCK_NAMES.len() as i32;
    format!("{}{}", ROCK_NAMES[(tier % n) as usize], tier_suffix(tier / n))
}

pub fn block_display_name(id: u8, tier: i32) -> String {
    match id {
        REGOLITH => "Regolith".to_string(),
        ROCK => rock_name(tier),
        ORE => format!("{} Crystal", ore_name(tier)),
        BARRIER => "Dense Core".to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Loot — every destroyed voxel drops something. Spoil sells for scraps at
// tier 0; rock value doubles per tier so far spoil isn't pure trash; crystal
// is the prize.
// ---------------------------------------------------------------------------

pub fn loot_value(id: u8, tier: i32) -> u64 {
    match id {
        REGOLITH => 1,
        ROCK => 1u64 << tier.clamp(0, 40),
        ORE => ore_value(tier),
        _ => 0,
    }
}

pub fn loot_name(id: u8, tier: i32) -> String {
    match id {
        REGOLITH => "Regolith".to_string(),
        ROCK => rock_name(tier),
        ORE => ore_name(tier),
        _ => String::new(),
    }
}

/// Base (un-jittered) loot color for drop-item materials.
pub fn loot_color(id: u8, tier: i32) -> [f32; 3] {
    let t = tier.max(0) as usize;
    match id {
        REGOLITH => [0.40, 0.37, 0.33],
        ROCK => ROCK_COLORS[t % ROCK_COLORS.len()],
        ORE => ORE_COLORS[t % ORE_COLORS.len()],
        _ => [1.0, 0.0, 1.0],
    }
}

// ---------------------------------------------------------------------------
// Hashes + noise (deterministic, no state)
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

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Trilinear value noise in [0,1] over an f32 lattice.
fn vnoise(p: Vec3, salt: u64) -> f32 {
    let f = p.floor();
    let (ix, iy, iz) = (f.x as i32, f.y as i32, f.z as i32);
    let u = Vec3::new(smooth(p.x - f.x), smooth(p.y - f.y), smooth(p.z - f.z));
    let c = |dx: i32, dy: i32, dz: i32| hash_unit(IVec3::new(ix + dx, iy + dy, iz + dz), salt);
    let x00 = c(0, 0, 0) * (1.0 - u.x) + c(1, 0, 0) * u.x;
    let x10 = c(0, 1, 0) * (1.0 - u.x) + c(1, 1, 0) * u.x;
    let x01 = c(0, 0, 1) * (1.0 - u.x) + c(1, 0, 1) * u.x;
    let x11 = c(0, 1, 1) * (1.0 - u.x) + c(1, 1, 1) * u.x;
    let y0 = x00 * (1.0 - u.y) + x10 * u.y;
    let y1 = x01 * (1.0 - u.y) + x11 * u.y;
    y0 * (1.0 - u.z) + y1 * u.z
}

fn fbm(p: Vec3, octaves: u32, salt: u64) -> f32 {
    let mut amp = 0.5;
    let mut freq = 1.0;
    let mut sum = 0.0;
    let mut norm = 0.0;
    for o in 0..octaves {
        sum += amp * vnoise(p * freq, salt.wrapping_add(o as u64 * 1469));
        norm += amp;
        amp *= 0.5;
        freq *= 2.13;
    }
    sum / norm
}

// ---------------------------------------------------------------------------
// The asteroid field
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Asteroid {
    /// Center in world meters.
    pub center: Vec3,
    /// Base radius (m) before displacement.
    pub radius: f32,
    pub seed: u64,
    /// Difficulty tier from distance to home.
    pub tier: i32,
}

impl Asteroid {
    /// Conservative outer bound of the displaced surface.
    pub fn reach(&self) -> f32 {
        self.radius * (DISP_BASE + DISP_AMP)
    }

    /// Displaced surface radius along the direction of `p`. Public so the
    /// impostor LOD can build silhouette-matched far meshes from the same
    /// noise.
    pub fn surface_toward(&self, p: Vec3) -> f32 {
        let dir = (p - self.center).normalize_or_zero();
        let s = Vec3::new(
            (self.seed & 0xFFFF) as f32,
            ((self.seed >> 16) & 0xFFFF) as f32,
            ((self.seed >> 32) & 0xFFFF) as f32,
        ) * 0.001;
        self.radius * (DISP_BASE + DISP_AMP * fbm(dir * 2.0 + s, 3, self.seed))
    }

}

// ---------------------------------------------------------------------------
// Surface field — the expensive fbm displacement evaluated on a coarse 1m
// lattice and trilinearly interpolated per voxel. Noise features are ≥2m, so
// the lattice loses nothing visible while cutting per-voxel cost ~5-10x
// (4096 fbm calls per rock chunk used to make flight a slideshow). Both the
// chunked generator and the single-voxel fallback interpolate from the SAME
// pure lattice function, so they stay bit-identical — the
// chunk_storage_matches_pure_gen test enforces it.
// ---------------------------------------------------------------------------

/// Lattice spacing (m).
const FIELD_L: f32 = 1.0;

/// Exact signed "depth inside the surface" at a global lattice point
/// (positive = inside rock). Pure function of (asteroid, lattice coords).
fn field_lattice(a: &Asteroid, q: IVec3) -> f32 {
    let p = q.as_vec3() * FIELD_L;
    a.surface_toward(p) - p.distance(a.center)
}

/// Trilinear interpolation with a fixed operation order, shared by every
/// caller so results are bit-identical regardless of where corners come from.
fn trilerp_at(p: Vec3, mut corner: impl FnMut(IVec3) -> f32) -> f32 {
    let g = p / FIELD_L;
    let f = g.floor();
    let q0 = f.as_ivec3();
    let u = g - f;
    let c = |dx: i32, dy: i32, dz: i32, corner: &mut dyn FnMut(IVec3) -> f32| {
        corner(q0 + IVec3::new(dx, dy, dz))
    };
    let x00 = c(0, 0, 0, &mut corner) * (1.0 - u.x) + c(1, 0, 0, &mut corner) * u.x;
    let x10 = c(0, 1, 0, &mut corner) * (1.0 - u.x) + c(1, 1, 0, &mut corner) * u.x;
    let x01 = c(0, 0, 1, &mut corner) * (1.0 - u.x) + c(1, 0, 1, &mut corner) * u.x;
    let x11 = c(0, 1, 1, &mut corner) * (1.0 - u.x) + c(1, 1, 1, &mut corner) * u.x;
    let y0 = x00 * (1.0 - u.y) + x10 * u.y;
    let y1 = x01 * (1.0 - u.y) + x11 * u.y;
    y0 * (1.0 - u.z) + y1 * u.z
}

/// Precomputed lattice values covering one chunk for one asteroid.
struct FieldGrid {
    origin: IVec3,
    n: IVec3,
    values: Vec<f32>,
}

impl FieldGrid {
    fn for_box(a: &Asteroid, min_m: Vec3, max_m: Vec3) -> Self {
        let origin = (min_m / FIELD_L).floor().as_ivec3();
        let top = (max_m / FIELD_L).ceil().as_ivec3() + IVec3::ONE;
        let n = top - origin + IVec3::ONE;
        let mut values = Vec::with_capacity((n.x * n.y * n.z) as usize);
        for z in 0..n.z {
            for y in 0..n.y {
                for x in 0..n.x {
                    values.push(field_lattice(a, origin + IVec3::new(x, y, z)));
                }
            }
        }
        Self { origin, n, values }
    }

    #[inline]
    fn at(&self, q: IVec3) -> f32 {
        let l = q - self.origin;
        self.values[(l.x + l.y * self.n.x + l.z * self.n.x * self.n.y) as usize]
    }

    #[inline]
    fn sample(&self, p: Vec3) -> f32 {
        trilerp_at(p, |q| self.at(q))
    }
}

/// Field for a single arbitrary point — the fallback/raycast path.
fn field_single(a: &Asteroid, p: Vec3) -> f32 {
    trilerp_at(p, |q| field_lattice(a, q))
}

/// Classify a voxel given its interpolated field depth (None = vacuum).
fn classify(a: &Asteroid, v: IVec3, p: Vec3, f: f32) -> Option<u8> {
    if f <= 0.0 {
        return None;
    }
    if f < SHELL_M {
        return Some(REGOLITH);
    }
    let d = p.distance(a.center);
    let surf = d + f;
    if d < surf * CORE_FRAC {
        return Some(ORE);
    }
    let frac = d / surf;
    let chance = ORE_CHANCE_BASE + ORE_CHANCE_CORE * (1.0 - frac) * (1.0 - frac);
    Some(if hash_unit(v, 0xA17E) < chance { ORE } else { ROCK })
}

fn cell_of(p: Vec3) -> IVec3 {
    (p / CELL_M).floor().as_ivec3()
}

/// The asteroid hosted by a field cell, if any. Pure function of the cell.
pub fn asteroid_in_cell(c: IVec3) -> Option<Asteroid> {
    // The home rock owns the origin cell.
    if c == IVec3::ZERO {
        return Some(Asteroid {
            center: Vec3::ZERO,
            radius: HOME_R,
            seed: 0xCAFE_D00D,
            tier: 0,
        });
    }
    if hash_unit(c, 0xF1E1D) > CELL_DENSITY {
        return None;
    }
    let j = |salt: u64| 0.25 + 0.5 * hash_unit(c, salt);
    let center = (c.as_vec3() + Vec3::new(j(0xA1), j(0xA2), j(0xA3))) * CELL_M;
    let radius = R_MIN + hash_unit(c, 0xA4).powi(2) * (R_MAX - R_MIN);
    // Keep a clear corridor around the home rock.
    if center.length() < HOME_R + radius + 10.0 {
        return None;
    }
    Some(Asteroid {
        center,
        radius,
        seed: hash3(c, 0xA5),
        tier: (center.length() / TIER_M) as i32,
    })
}

/// All asteroids whose displaced surface could intersect the AABB (meters).
pub fn asteroids_overlapping(min_m: Vec3, max_m: Vec3) -> Vec<Asteroid> {
    let pad = R_MAX * (DISP_BASE + DISP_AMP) + 0.5;
    let lo = cell_of(min_m - Vec3::splat(pad));
    let hi = cell_of(max_m + Vec3::splat(pad));
    let mut out = Vec::new();
    for cz in lo.z..=hi.z {
        for cy in lo.y..=hi.y {
            for cx in lo.x..=hi.x {
                if let Some(a) = asteroid_in_cell(IVec3::new(cx, cy, cz)) {
                    let closest = a.center.clamp(min_m, max_m);
                    if closest.distance(a.center) <= a.reach() {
                        out.push(a);
                    }
                }
            }
        }
    }
    out
}

/// The nearest asteroid to `p` other than home — demo + future nav use.
#[allow(dead_code)]
pub fn nearest_asteroid(p: Vec3, exclude_home: bool) -> Option<Asteroid> {
    let pc = cell_of(p);
    let mut best: Option<(f32, Asteroid)> = None;
    for cz in -2..=2 {
        for cy in -2..=2 {
            for cx in -2..=2 {
                let Some(a) = asteroid_in_cell(pc + IVec3::new(cx, cy, cz)) else {
                    continue;
                };
                if exclude_home && a.center == Vec3::ZERO {
                    continue;
                }
                let d = p.distance(a.center) - a.reach();
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, a));
                }
            }
        }
    }
    best.map(|(_, a)| a)
}

#[inline]
fn voxel_center_m(v: IVec3) -> Vec3 {
    (v.as_vec3() + Vec3::splat(0.5)) * VOXEL
}

/// Pure worldgen for one voxel (slow path: looks up its asteroids itself and
/// interpolates the field from raw lattice evaluations).
pub fn block_at(v: IVec3) -> u8 {
    let p = voxel_center_m(v);
    for a in asteroids_overlapping(p, p) {
        if p.distance_squared(a.center) > a.reach() * a.reach() {
            continue;
        }
        if let Some(b) = classify(&a, v, p, field_single(&a, p)) {
            return b;
        }
    }
    AIR
}

/// Tier of the asteroid covering this voxel (distance tier in open space).
pub fn tier_of_voxel(v: IVec3) -> i32 {
    let p = voxel_center_m(v);
    asteroids_overlapping(p, p)
        .iter()
        .find(|a| {
            p.distance_squared(a.center) <= a.reach() * a.reach()
                && field_single(a, p) > 0.0
        })
        .map_or_else(|| (p.length() / TIER_M) as i32, |a| a.tier)
}

/// Where the player materializes: on top of the home rock.
pub fn home_spawn() -> Vec3 {
    static SPAWN: OnceLock<Vec3> = OnceLock::new();
    *SPAWN.get_or_init(|| Vec3::new(0.0, surface_y_at(0.0, 0.0) , 0.0))
}

/// Y (meters) of the first open voxel above the home rock at (x, z).
pub fn surface_y_at(x_m: f32, z_m: f32) -> f32 {
    let top = (HOME_R * (DISP_BASE + DISP_AMP) / VOXEL).ceil() as i32 + 2;
    let (vx, vz) = ((x_m / VOXEL).floor() as i32, (z_m / VOXEL).floor() as i32);
    for vy in (-top..=top).rev() {
        if block_at(IVec3::new(vx, vy, vz)) != AIR {
            return (vy + 1) as f32 * VOXEL;
        }
    }
    0.0
}

/// Far-LOD impostor vertex color: the tier's rock tone washed toward
/// regolith grey (what a voxel asteroid averages to at distance), jittered.
pub fn impostor_color(tier: i32, jitter01: f32) -> [f32; 3] {
    let rock = ROCK_COLORS[(tier.max(0) as usize) % ROCK_COLORS.len()];
    let j = (0.72 + 0.25 * jitter01) * 0.9;
    [
        (rock[0] * 0.6 + 0.24 * 0.4) * j,
        (rock[1] * 0.6 + 0.22 * 0.4) * j,
        (rock[2] * 0.6 + 0.20 * 0.4) * j,
    ]
}

/// Per-vertex linear color for a block, with a little per-voxel brightness
/// jitter so untextured cubes don't read as a flat wall of one color.
fn block_color(id: u8, tier: i32, v: IVec3, shell: bool) -> [f32; 3] {
    let t = tier.max(0) as usize;
    let base = match id {
        REGOLITH => {
            if shell {
                [0.34, 0.325, 0.30]
            } else {
                [0.30, 0.275, 0.25]
            }
        }
        ROCK => ROCK_COLORS[t % ROCK_COLORS.len()],
        ORE => ORE_COLORS[t % ORE_COLORS.len()],
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
pub fn chunk_of(v: IVec3) -> IVec3 {
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

fn chunk_bounds_m(cp: IVec3) -> (Vec3, Vec3) {
    let min = cp.as_vec3() * CHUNK_M;
    (min, min + Vec3::splat(CHUNK_M))
}

#[derive(Resource, Default)]
pub struct VoxelWorld {
    chunks: HashMap<IVec3, Chunk>,
    /// Chunks known to intersect no asteroid: tracked, but no storage/entities.
    empty: HashSet<IVec3>,
    /// Chunks whose mesh is stale and needs a rebuild.
    pub dirty: HashSet<IVec3>,
    /// Remaining HP of partially-mined blocks. Sparse: only blocks under attack.
    pub damage: HashMap<IVec3, f32>,
    /// Player edits (mined voxels), forever. Survives chunk unload; applied on
    /// regeneration. This is also exactly what a save file would serialize.
    edits: HashMap<IVec3, u8>,
}

impl VoxelWorld {
    /// Block id at a voxel. Falls back to pure worldgen (+ edits) for chunks
    /// that aren't materialized, so collision/raycasts are correct anywhere.
    pub fn block(&self, v: IVec3) -> u8 {
        let cp = chunk_of(v);
        if let Some(c) = self.chunks.get(&cp) {
            return c.blocks[local_index(v)];
        }
        if self.empty.contains(&cp) {
            return AIR;
        }
        if let Some(&b) = self.edits.get(&v) {
            return b;
        }
        block_at(v)
    }

    #[inline]
    pub fn solid(&self, v: IVec3) -> bool {
        self.block(v) != AIR
    }

    pub fn is_loaded(&self, cp: IVec3) -> bool {
        self.chunks.contains_key(&cp) || self.empty.contains(&cp)
    }

    /// Generate (or regenerate) a chunk. Returns false if it's pure vacuum.
    pub fn generate_chunk(&mut self, cp: IVec3) -> bool {
        let (min_m, max_m) = chunk_bounds_m(cp);
        let asteroids = asteroids_overlapping(min_m, max_m);
        if asteroids.is_empty() {
            self.empty.insert(cp);
            return false;
        }
        // One coarse field grid per overlapping asteroid; per-voxel work is
        // then a distance check + trilerp, not an fbm evaluation.
        let grids: Vec<(Asteroid, FieldGrid)> = asteroids
            .into_iter()
            .map(|a| {
                let g = FieldGrid::for_box(&a, min_m, max_m);
                (a, g)
            })
            .collect();
        let mut blocks = Box::new([0u8; CHUNK_VOL]);
        let origin = cp * CHUNK;
        let mut any_solid = false;
        for y in 0..CHUNK {
            for z in 0..CHUNK {
                for x in 0..CHUNK {
                    let v = origin + IVec3::new(x, y, z);
                    let b = match self.edits.get(&v) {
                        Some(&e) => e,
                        None => {
                            let p = voxel_center_m(v);
                            let mut b = AIR;
                            for (a, grid) in &grids {
                                if p.distance_squared(a.center) > a.reach() * a.reach() {
                                    continue;
                                }
                                if let Some(id) = classify(a, v, p, grid.sample(p)) {
                                    b = id;
                                    break;
                                }
                            }
                            b
                        }
                    };
                    any_solid |= b != AIR;
                    blocks[local_index(v)] = b;
                }
            }
        }
        if !any_solid {
            self.empty.insert(cp);
            return false;
        }
        self.chunks.insert(cp, Chunk { blocks });
        self.dirty.insert(cp);
        true
    }

    pub fn unload_chunk(&mut self, cp: IVec3) {
        self.chunks.remove(&cp);
        self.dirty.remove(&cp);
    }

    pub fn forget_empty(&mut self, keep_near: IVec3, radius: i32) {
        self.empty
            .retain(|cp| (*cp - keep_near).abs().max_element() <= radius);
    }

    /// Remove a block (mined out). Records the edit and marks the chunk — and
    /// any face-adjacent neighbor chunks — dirty so culled faces get rebuilt.
    pub fn set_air(&mut self, v: IVec3) {
        self.edits.insert(v, AIR);
        let cp = chunk_of(v);
        if let Some(c) = self.chunks.get_mut(&cp) {
            c.blocks[local_index(v)] = AIR;
        }
        self.damage.remove(&v);
        if self.chunks.contains_key(&cp) {
            self.dirty.insert(cp);
        }
        for d in [
            IVec3::X,
            IVec3::NEG_X,
            IVec3::Y,
            IVec3::NEG_Y,
            IVec3::Z,
            IVec3::NEG_Z,
        ] {
            let ncp = chunk_of(v + d);
            if ncp != cp && self.chunks.contains_key(&ncp) {
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

    // One asteroid lookup + coarse field grid per chunk, shared by every
    // voxel: tier/shell per voxel is a trilerp, not an fbm evaluation.
    let (min_m, max_m) = chunk_bounds_m(cp);
    let grids: Vec<(Asteroid, FieldGrid)> = asteroids_overlapping(min_m, max_m)
        .into_iter()
        .map(|a| {
            let g = FieldGrid::for_box(&a, min_m, max_m);
            (a, g)
        })
        .collect();
    let tier_at = |p: Vec3| -> (i32, bool) {
        for (a, grid) in &grids {
            if p.distance_squared(a.center) > a.reach() * a.reach() {
                continue;
            }
            let f = grid.sample(p);
            if f > 0.0 {
                return (a.tier, f < SHELL_M);
            }
        }
        (0, false)
    };

    let origin = cp * CHUNK;
    for y in 0..CHUNK {
        for z in 0..CHUNK {
            for x in 0..CHUNK {
                let v = origin + IVec3::new(x, y, z);
                let id = world.block(v);
                if id == AIR {
                    continue;
                }
                let (tier, shell) = tier_at(voxel_center_m(v));
                let col = block_color(id, tier, v, shell);
                let base = (v.as_vec3()) * VOXEL;
                for face in &FACES {
                    if world.solid(v + face.dir) {
                        continue;
                    }
                    if id == ORE {
                        // Crystal faces: hot vertex colors on the unlit mesh.
                        // 1.6–3.0x pushes them past 1.0 so bloom picks them up;
                        // per-voxel jitter makes a vein shimmer, not a slab.
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

/// Solid + glow mesh entities for each materialized chunk.
#[derive(Resource, Default)]
pub struct ChunkEntities(HashMap<IVec3, (Entity, Entity)>);

#[derive(Resource)]
pub struct WorldAssets {
    pub material: Handle<StandardMaterial>,
    pub glow_material: Handle<StandardMaterial>,
}

#[derive(Component)]
struct ChunkMesh;

/// Chunk offsets sorted nearest-first, precomputed once.
#[derive(Resource)]
struct GenOrder(Vec<IVec3>);

impl Default for GenOrder {
    fn default() -> Self {
        let mut v = Vec::new();
        for z in -GEN_RADIUS..=GEN_RADIUS {
            for y in -GEN_RADIUS..=GEN_RADIUS {
                for x in -GEN_RADIUS..=GEN_RADIUS {
                    let o = IVec3::new(x, y, z);
                    if o.length_squared() <= GEN_RADIUS * GEN_RADIUS {
                        v.push(o);
                    }
                }
            }
        }
        v.sort_by_key(|o| o.length_squared());
        Self(v)
    }
}

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VoxelWorld>()
            .init_resource::<ChunkEntities>()
            .init_resource::<GenOrder>()
            .add_systems(Startup, setup_world_assets)
            .add_systems(
                Update,
                (ensure_chunks, remesh_dirty, unload_far_chunks).chain(),
            );
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

/// Materialize chunks around the player, nearest first, within a per-frame
/// budget. Pure-vacuum chunks cost a set entry, not storage or entities.
fn ensure_chunks(
    player: Res<crate::player::PlayerState>,
    order: Res<GenOrder>,
    mut world: ResMut<VoxelWorld>,
    mut chunk_entities: ResMut<ChunkEntities>,
    mut meshes: ResMut<Assets<Mesh>>,
    assets: Res<WorldAssets>,
    mut commands: Commands,
) {
    let pc = chunk_of((player.pos / VOXEL).floor().as_ivec3());
    let mut budget = GEN_BUDGET;
    for off in &order.0 {
        if budget == 0 {
            break;
        }
        let cp = pc + *off;
        if world.is_loaded(cp) {
            continue;
        }
        budget -= 1;
        if !world.generate_chunk(cp) {
            continue; // vacuum
        }
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

/// Drop chunk storage + mesh entities far behind the player. Edits persist in
/// the overlay, so flying back regenerates the world exactly as you left it.
fn unload_far_chunks(
    player: Res<crate::player::PlayerState>,
    mut world: ResMut<VoxelWorld>,
    mut chunk_entities: ResMut<ChunkEntities>,
    mut commands: Commands,
    mut tick: Local<u32>,
) {
    *tick = tick.wrapping_add(1);
    if *tick % 97 != 0 {
        return; // sweep occasionally, not every frame
    }
    let pc = chunk_of((player.pos / VOXEL).floor().as_ivec3());
    let far: Vec<IVec3> = chunk_entities
        .0
        .keys()
        .filter(|cp| (**cp - pc).abs().max_element() > UNLOAD_RADIUS)
        .copied()
        .collect();
    for cp in far {
        if let Some((a, b)) = chunk_entities.0.remove(&cp) {
            commands.entity(a).despawn();
            commands.entity(b).despawn();
        }
        world.unload_chunk(cp);
    }
    world.forget_empty(pc, UNLOAD_RADIUS + 8);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Materialize every chunk within `r_m` meters of a point.
    fn gen_sphere(w: &mut VoxelWorld, center: Vec3, r_m: f32) {
        let r_c = (r_m / CHUNK_M).ceil() as i32;
        let cc = chunk_of((center / VOXEL).floor().as_ivec3());
        for z in -r_c..=r_c {
            for y in -r_c..=r_c {
                for x in -r_c..=r_c {
                    w.generate_chunk(cc + IVec3::new(x, y, z));
                }
            }
        }
    }

    #[test]
    fn home_rock_exists_and_has_a_crystal_core() {
        // The voxel at the very center of the home rock is core crystal.
        assert_eq!(block_at(IVec3::ZERO), ORE);
        // Well outside the reach: vacuum.
        assert_eq!(block_at(IVec3::new(0, (40.0 / VOXEL) as i32, 0)), AIR);
        // Somewhere mid-shell: solid.
        assert_ne!(block_at(IVec3::new((HOME_R * 0.5 / VOXEL) as i32, 0, 0)), AIR);
    }

    #[test]
    fn worldgen_is_deterministic() {
        let v = IVec3::new(17, -40, 31);
        assert_eq!(block_at(v), block_at(v));
        let a = asteroid_in_cell(IVec3::new(3, 1, -2));
        let b = asteroid_in_cell(IVec3::new(3, 1, -2));
        assert_eq!(a.is_some(), b.is_some());
        if let (Some(a), Some(b)) = (a, b) {
            assert_eq!(a.center, b.center);
            assert_eq!(a.radius, b.radius);
        }
    }

    #[test]
    fn field_has_neighbors_with_distance_tiers() {
        // Some asteroid must exist within a few cells of home...
        let near = nearest_asteroid(Vec3::ZERO, true).expect("field should not be empty");
        assert!(near.center.length() > HOME_R);
        // ...and tiers grow with distance.
        let far_tier = ((5.0 * CELL_M) / TIER_M) as i32;
        assert!(far_tier >= 1);
        assert_eq!(near.tier, (near.center.length() / TIER_M) as i32);
    }

    #[test]
    fn stats_scale_with_tier() {
        assert!(block_hp(ROCK, 3) > block_hp(ROCK, 0));
        assert!(ore_value(3) > ore_value(0));
        // Value must outgrow HP for progression to accelerate.
        let hp_ratio = block_hp(ROCK, 1) / block_hp(ROCK, 0);
        let val_ratio = ore_value(1) as f32 / ore_value(0) as f32;
        assert!(val_ratio > hp_ratio);
    }

    #[test]
    fn raycast_hits_home_surface_from_above() {
        let mut w = VoxelWorld::default();
        gen_sphere(&mut w, Vec3::ZERO, HOME_R * 1.4);
        let spawn = home_spawn();
        let eye = spawn + Vec3::Y * 1.25;
        let hit = raycast(&w, eye, Vec3::NEG_Y, 10.0).expect("should hit home rock");
        assert_eq!(hit.normal, IVec3::Y);
        // Looking up: sky, no hit.
        assert!(raycast(&w, eye, Vec3::Y, 30.0).is_none());
    }

    #[test]
    fn edits_survive_unload_and_regeneration() {
        let mut w = VoxelWorld::default();
        // A voxel just under the home surface at the spawn column.
        let spawn = home_spawn();
        let v = IVec3::new(0, ((spawn.y - 0.3) / VOXEL).floor() as i32, 0);
        gen_sphere(&mut w, Vec3::ZERO, 6.0);
        assert!(w.solid(v), "test voxel should start solid");
        w.set_air(v);
        assert!(!w.solid(v));
        // Unload the chunk entirely: fallback path must still honor the edit.
        let cp = chunk_of(v);
        w.unload_chunk(cp);
        assert!(!w.solid(v), "edit must survive via fallback");
        // Regenerate: the edit must be baked back into chunk storage.
        w.generate_chunk(cp);
        assert!(!w.solid(v), "edit must survive regeneration");
    }

    #[test]
    fn vacuum_chunks_are_cheap_and_air() {
        let mut w = VoxelWorld::default();
        // Pick a chunk far from any cell center but inside no asteroid: probe
        // a few until one is vacuum (the field is ~50% dense per CELL, and
        // cells are 14 chunks wide — most chunks are vacuum).
        let mut found = false;
        for i in 0..200 {
            let cp = IVec3::new(40 + i * 3, 7, -9);
            if !w.generate_chunk(cp) {
                assert!(w.is_loaded(cp));
                let v = cp * CHUNK + IVec3::splat(8);
                assert_eq!(w.block(v), AIR);
                found = true;
                break;
            }
        }
        assert!(found, "expected to find at least one vacuum chunk");
    }

    /// Chunk contents must equal pure worldgen everywhere (the block()
    /// fallback path depends on it). Run with --nocapture to also eyeball an
    /// ASCII slice of the home rock.
    #[test]
    fn chunk_storage_matches_pure_gen() {
        let mut w = VoxelWorld::default();
        for &(c, r) in &[(Vec3::ZERO, 22.0), (Vec3::new(33.0, 31.0, -31.0), 18.0)] {
            let rc = (r / CHUNK_M).ceil() as i32;
            let cc = chunk_of((c / VOXEL).floor().as_ivec3());
            for z in -rc..=rc {
                for y in -rc..=rc {
                    for x in -rc..=rc {
                        w.generate_chunk(cc + IVec3::new(x, y, z));
                    }
                }
            }
        }
        let mut mismatches = 0;
        for &(cx, cy, cz) in &[(0i32, 40i32, 0i32), (132, 124, -124)] {
            for dy in -20..20 {
                for dx in -20..20 {
                    let v = IVec3::new(cx + dx, cy + dy, cz);
                    let a = w.block(v);
                    let b = block_at(v);
                    if a != b {
                        mismatches += 1;
                        if mismatches < 10 {
                            println!("MISMATCH at {v:?}: chunk={a} gen={b}");
                        }
                    }
                }
            }
        }
        println!("total mismatches: {mismatches}");
        let glyph = |b: u8| match b {
            AIR => '.',
            REGOLITH => 'r',
            ROCK => '#',
            ORE => '*',
            _ => '?',
        };
        println!("--- home, vertical slice z=0 (top of rock) ---");
        for vy in (40..58).rev() {
            let row: String = (-24..24)
                .map(|vx| glyph(w.block(IVec3::new(vx, vy, 0))))
                .collect();
            println!("{row}");
        }
        assert_eq!(mismatches, 0);
    }

    /// Perf probe: worldgen + meshing cost per rock chunk. Not pass/fail.
    /// Run with: cargo test bench_chunk --release -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_chunk_gen_and_mesh() {
        use std::time::Instant;
        let mut w = VoxelWorld::default();
        let cc = chunk_of((Vec3::ZERO / VOXEL).floor().as_ivec3());
        let r = 4; // 9^3 chunks around home = mix of rock + vacuum
        let mut rock_chunks = Vec::new();

        let t = Instant::now();
        for z in -r..=r {
            for y in -r..=r {
                for x in -r..=r {
                    let cp = cc + IVec3::new(x, y, z);
                    if w.generate_chunk(cp) {
                        rock_chunks.push(cp);
                    }
                }
            }
        }
        let gen_ms = t.elapsed().as_secs_f64() * 1000.0;
        let total = (2 * r + 1) * (2 * r + 1) * (2 * r + 1);

        let t = Instant::now();
        for cp in &rock_chunks {
            let _ = mesh_chunk(&w, *cp);
        }
        let mesh_ms = t.elapsed().as_secs_f64() * 1000.0;

        println!(
            "gen: {gen_ms:.1} ms for {total} chunks ({} rock) = {:.3} ms/rock-chunk",
            rock_chunks.len(),
            gen_ms / rock_chunks.len().max(1) as f64
        );
        println!(
            "mesh: {mesh_ms:.1} ms = {:.3} ms/rock-chunk",
            mesh_ms / rock_chunks.len().max(1) as f64
        );
    }

    #[test]
    fn surface_chunk_meshes_nonempty() {
        let mut w = VoxelWorld::default();
        gen_sphere(&mut w, Vec3::ZERO, HOME_R * 1.4);
        // A chunk straddling the home surface at the spawn column — a chunk
        // fully inside the rock would correctly mesh to nothing (all faces
        // culled), so the deep-interior chunk is the wrong thing to assert on.
        let spawn = home_spawn();
        let cp = chunk_of((spawn / VOXEL).floor().as_ivec3());
        let (solid, _glow) = mesh_chunk(&w, cp);
        assert!(solid.count_vertices() > 0);
    }
}
