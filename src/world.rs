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
/// Chance a cell hosts an asteroid. Deliberately sparse — the planets and the
/// black hole are the landmarks now, and each rock should feel like a
/// destination. A starter rock is guaranteed within a couple of cells of home
/// (see `starter_cell`).
const CELL_DENSITY: f32 = 0.018;
/// Asteroid radii (pre-displacement), small ones common, big ones rare
/// (cubic skew). Rarer field = bigger rocks, so a find is worth the flight.
const R_MIN: f32 = 6.0;
const R_MAX: f32 = 30.0;
/// Per-asteroid shape ranges (derived from the seed): ellipsoid stretch per
/// axis, displacement amplitude, and noise frequency. `reach()` and the
/// overlap pad must bound the extremes.
const STRETCH_MIN: f32 = 0.78;
const STRETCH_MAX: f32 = 1.40;
const AMP_MIN: f32 = 0.10;
const AMP_MAX: f32 = 0.30;
const FREQ_MIN: f32 = 1.6;
const FREQ_MAX: f32 = 3.2;
/// The home rock.
pub const HOME_R: f32 = 15.0;
/// Distance from home per difficulty tier ("sector").
pub const TIER_M: f32 = 120.0;

/// The solar system's axis: home sits at the system's edge (the belt), the
/// dying star / black hole at the far end of this ray, the planets strung
/// out between. blackhole.rs places the singularity on this same axis.
pub const SYSTEM_AXIS: Vec3 = Vec3::new(-0.65, -0.08, 0.62);
/// Where the star sits at day start (m from origin) — planets are placed
/// relative to this so the closest one orbits deep in the kill zone.
pub const STAR_DIST: f32 = 5_200.0;
pub const PLANET_COUNT: usize = 6;
/// Planet radii (pre-displacement). HUGE relative to asteroids (up to ~25x
/// the biggest belt rock) — landable worlds, mined exactly like asteroids
/// (same voxel pipeline), but cratered, saturated, and haloed in atmosphere.
const PLANET_R_MIN: f32 = 160.0;
const PLANET_R_MAX: f32 = 380.0;

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

// Rock identity is a per-asteroid SPECIES (from its seed) — fly past a grey
// chondrite ball, an icy blue shard, a rust-red ellipsoid. Ore identity stays
// tier-based so the progression ladder still reads.
const ROCK_NAMES: [&str; 8] = [
    "Chondrite", "Basalt", "Magnetite", "Hematite", "Pallasite", "Obsidian", "Deepcore",
    "Voidrock",
];
const ORE_NAMES: [&str; 8] = [
    "Carbon", "Ferrite", "Cobalt", "Titania", "Argent", "Aurium", "Cryonite", "Stellarite",
];

// Linear-RGB palettes. ROCK_COLORS indexes by asteroid species; ORE_COLORS by
// tier. Vertex colors multiply the material's white base color, so these are
// the on-screen block colors.
const ROCK_COLORS: [[f32; 3]; 8] = [
    [0.40, 0.37, 0.33], // Chondrite — dusty grey-brown
    [0.22, 0.24, 0.30], // Basalt — dark blue-grey
    [0.36, 0.29, 0.44], // Magnetite — violet sheen
    [0.52, 0.26, 0.17], // Hematite — rust red
    [0.42, 0.45, 0.28], // Pallasite — olive metal
    [0.13, 0.11, 0.17], // Obsidian — near-black violet
    [0.30, 0.46, 0.50], // Deepcore — glacial blue-teal
    [0.38, 0.16, 0.36], // Voidrock — bruised purple
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

pub fn rock_name(species: u8) -> String {
    ROCK_NAMES[(species as usize) % ROCK_NAMES.len()].to_string()
}

pub fn block_display_name(id: u8, tier: i32, species: u8) -> String {
    match id {
        REGOLITH => "Regolith".to_string(),
        ROCK => rock_name(species),
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

#[allow(dead_code)] // save-file / tooltip use; keep aligned with loot_value
pub fn loot_name(id: u8, tier: i32) -> String {
    match id {
        REGOLITH => "Regolith".to_string(),
        ROCK => rock_name((tier % ROCK_NAMES.len() as i32) as u8),
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
// World salt — the only mutable input to worldgen. Each black-hole "day"
// reseeds the asteroid field (home rock excepted) so every run is a fresh
// claim. Everything else stays a pure function of (coords, salt).
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};

static WORLD_SALT: AtomicU64 = AtomicU64::new(0);

/// NOTE: process-global. Unit tests share one process and run in parallel, so
/// tests must never call this — they all assume salt 0.
pub fn set_world_salt(salt: u64) {
    WORLD_SALT.store(salt, Ordering::Relaxed);
}

#[inline]
fn salted(salt: u64) -> u64 {
    salt ^ WORLD_SALT
        .load(Ordering::Relaxed)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
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
    /// Visual identity: ROCK_COLORS / ROCK_NAMES row.
    pub species: u8,
    /// Ellipsoid stretch per axis — potatoes, shards, and near-spheres.
    pub stretch: Vec3,
    /// Displacement amplitude and noise frequency — smooth blob vs crag.
    pub amp: f32,
    pub freq: f32,
    /// True for the big landable worlds: crater relief, saturated palette,
    /// atmosphere halo on the impostor.
    pub is_planet: bool,
    /// Crater fields (xyz = unit direction, w = angular radius). Depth is a
    /// fixed fraction of the angular radius; bowls only (no raised rim), so
    /// `reach()` stays a valid outer bound.
    pub craters: [Vec4; 4],
}

/// Per-seed scalar in [0,1) for shape parameters.
#[inline]
fn seed_unit(seed: u64, k: u64) -> f32 {
    let mut x = seed ^ k.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((x >> 40) as f32) / ((1u64 << 24) as f32)
}

impl Asteroid {
    fn shaped(center: Vec3, radius: f32, seed: u64, tier: i32) -> Self {
        let u = |k: u64| seed_unit(seed, k);
        Self {
            center,
            radius,
            seed,
            tier,
            species: ((seed >> 17) % ROCK_COLORS.len() as u64) as u8,
            stretch: Vec3::new(
                STRETCH_MIN + (STRETCH_MAX - STRETCH_MIN) * u(1),
                STRETCH_MIN + (STRETCH_MAX - STRETCH_MIN) * u(2),
                STRETCH_MIN + (STRETCH_MAX - STRETCH_MIN) * u(3),
            ),
            amp: AMP_MIN + (AMP_MAX - AMP_MIN) * u(4),
            freq: FREQ_MIN + (FREQ_MAX - FREQ_MIN) * u(5),
            is_planet: false,
            craters: [Vec4::ZERO; 4],
        }
    }

    /// Conservative outer bound of the displaced surface (craters only ever
    /// subtract, so they can't exceed this).
    pub fn reach(&self) -> f32 {
        self.radius * self.stretch.max_element() * (0.95 + self.amp)
    }

    /// Displaced surface radius along the direction of `p`. Public so the
    /// impostor LOD can build silhouette-matched far meshes from the same
    /// noise. Base shape is an ellipsoid (per-axis stretch), displaced by
    /// per-asteroid fbm; planets additionally carry crater bowls.
    pub fn surface_toward(&self, p: Vec3) -> f32 {
        let dir = (p - self.center).normalize_or_zero();
        let e = dir / self.stretch;
        let base = self.radius / e.length().max(1e-4);
        let s = Vec3::new(
            (self.seed & 0xFFFF) as f32,
            ((self.seed >> 16) & 0xFFFF) as f32,
            ((self.seed >> 32) & 0xFFFF) as f32,
        ) * 0.001;
        let mut r =
            base * ((0.95 - self.amp) + 2.0 * self.amp * fbm(dir * self.freq + s, 3, self.seed));
        if self.is_planet {
            for c in &self.craters {
                if c.w <= 0.0 {
                    continue;
                }
                let d = (dir - c.truncate()).length();
                if d < c.w {
                    // Smooth bowl: deepest at the center, flush at the lip.
                    let x = d / c.w;
                    let bowl = 1.0 - x * x * (3.0 - 2.0 * x);
                    r -= base * c.w * 0.55 * bowl;
                }
            }
        }
        r
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

// ---------------------------------------------------------------------------
// Planets — a handful of huge landable bodies strung along the system axis.
// The closer a planet orbits to the dying star, the higher its tier: endgame
// HP walls guarding endgame crystal, and the black hole sweeps them up one by
// one as it advances through the day. Pure function of the world salt.
// ---------------------------------------------------------------------------

/// Build the day's planets from a salt: ORBITS around the star's seat, the
/// way they sat when it was a sun. A shared ecliptic plane (containing the
/// home belt — we're part of this system), orbital radii from deep in the
/// kill zone out to belt range, and golden-angle longitude spacing so the
/// worlds scatter all around the star instead of clumping on one bearing.
/// Index 0 is the innermost orbit (highest tier — the endgame lives closest
/// to the dying star); the last is the outermost (lowest tier).
fn build_planets(salt: u64) -> Vec<Asteroid> {
    let axis = SYSTEM_AXIS.normalize();
    let seat = axis * STAR_DIST;
    // Ecliptic basis: the plane spanned by the system axis and a horizon
    // vector — home sits (roughly) in this plane, like a belt should.
    let side = axis.cross(Vec3::Y).normalize();
    let up = axis.cross(side).normalize();
    const GOLDEN: f32 = 2.399_963; // radians — maximally spread longitudes
    (0..PLANET_COUNT)
        .map(|i| {
            let seed = hash3(
                IVec3::new(i as i32 + 11, 47, -23),
                salt.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x7AB1_E7,
            );
            let u = |k: u64| seed_unit(seed, k);
            let t = i as f32 / (PLANET_COUNT - 1) as f32;
            // Orbital radius from the seat: 1.05km (kill zone) .. 4.6km.
            let orbit_r = 1_050.0 + t * 3_550.0;
            let lon = i as f32 * GOLDEN + (u(2) - 0.5) * 0.8 + salt as f32 * 0.7;
            // Slight per-planet inclination so the band has depth.
            let incl = (u(6) - 0.5) * 0.24;
            let center = seat
                + (axis * lon.cos() + side * lon.sin()) * orbit_r * incl.cos()
                + up * orbit_r * incl.sin();
            let radius = PLANET_R_MIN + (PLANET_R_MAX - PLANET_R_MIN) * u(3);
            // Tier falls with orbital distance: 12 (innermost) .. 4 (outer).
            let tier = 12 - (t * 8.0).round() as i32;
            let mut p = Asteroid::shaped(center, radius, seed, tier);
            // Worlds, not potatoes: rounder, gentler base relief, then
            // cratered so the surface reads planetary at any distance.
            p.stretch = Vec3::ONE.lerp(p.stretch, 0.12);
            p.amp = 0.04 + 0.06 * u(4);
            p.freq = 1.2 + 0.8 * u(5);
            p.is_planet = true;
            for (k, slot) in p.craters.iter_mut().enumerate() {
                let kk = 0x40 + k as u64;
                let z = seed_unit(seed, kk) * 2.0 - 1.0;
                let a = seed_unit(seed, kk ^ 0x9) * std::f32::consts::TAU;
                let r = (1.0 - z * z).max(0.0).sqrt();
                *slot = Vec4::new(
                    r * a.cos(),
                    z,
                    r * a.sin(),
                    0.18 + 0.30 * seed_unit(seed, kk ^ 0x33),
                );
            }
            p
        })
        .collect()
}

/// The day's planets, cached per world salt (this is called from hot worldgen
/// paths; rebuilding ~6 structs from hashes per query would add up).
pub fn planet_list() -> std::sync::Arc<Vec<Asteroid>> {
    use std::sync::{Arc, RwLock};
    static CACHE: RwLock<Option<(u64, Arc<Vec<Asteroid>>)>> = RwLock::new(None);
    let salt = WORLD_SALT.load(Ordering::Relaxed);
    if let Some((s, list)) = CACHE.read().unwrap().as_ref() {
        if *s == salt {
            return list.clone();
        }
    }
    let list = Arc::new(build_planets(salt));
    *CACHE.write().unwrap() = Some((salt, list.clone()));
    list
}

// ---------------------------------------------------------------------------
// Bodies — every asteroid/planet is a rigid voxel BODY that the black hole
// physically drags in. Terrain stays a pure function of "gen space" (the
// body's original frame: storage, edits, meshes never change), and the body
// carries a displacement toward the hole. World↔gen mapping is a voxel-
// quantized offset, so all grid math (collision, raycast, mining) works
// unchanged; meshes ride a per-body root transform (smooth). When a body's
// true position crosses the horizon its chunks are despawned — consumed for
// real, not as an effect.
// ---------------------------------------------------------------------------

/// Body identity: belt rocks use their field cell; the home rock is ZERO;
/// planets use sentinel keys outside any reachable cell range.
const PLANET_KEY_BASE: i32 = 1_000_000;

pub fn planet_key(i: usize) -> IVec3 {
    IVec3::new(PLANET_KEY_BASE + i as i32, 0, 0)
}

/// The asteroid for a body key (gen-space description).
pub fn body_by_key(key: IVec3) -> Option<Asteroid> {
    if key.x >= PLANET_KEY_BASE {
        planet_list().get((key.x - PLANET_KEY_BASE) as usize).copied()
    } else {
        asteroid_in_cell(key)
    }
}

/// All bodies whose GEN-SPACE surface could intersect the AABB, with keys.
#[allow(dead_code)] // test helper API
pub fn bodies_overlapping(min_m: Vec3, max_m: Vec3) -> Vec<(IVec3, Asteroid)> {
    let pad = R_MAX * STRETCH_MAX * (0.95 + AMP_MAX) + 0.5;
    let lo = cell_of(min_m - Vec3::splat(pad));
    let hi = cell_of(max_m + Vec3::splat(pad));
    let mut out = Vec::new();
    for cz in lo.z..=hi.z {
        for cy in lo.y..=hi.y {
            for cx in lo.x..=hi.x {
                let c = IVec3::new(cx, cy, cz);
                if let Some(a) = asteroid_in_cell(c) {
                    let closest = a.center.clamp(min_m, max_m);
                    if closest.distance(a.center) <= a.reach() {
                        out.push((c, a));
                    }
                }
            }
        }
    }
    for (i, p) in planet_list().iter().enumerate() {
        let closest = p.center.clamp(min_m, max_m);
        if closest.distance(p.center) <= p.reach() {
            out.push((planet_key(i), *p));
        }
    }
    out
}

/// A body's displacement state. `offset` is smooth (drives the mesh root);
/// `off_vox` is the voxel-quantized version every collision/storage mapping
/// uses; `delta` is how far the quantized position moved this frame (the
/// player standing on the body is carried by exactly this).
#[derive(Clone, Copy, Default)]
pub struct BodyMotion {
    pub offset: Vec3,
    pub off_vox: IVec3,
    pub delta: Vec3,
    pub eaten: bool,
    backfilled: bool,
}

/// The cell guaranteed to host a starter rock near home — the field is sparse
/// now, so without this an unlucky day could strand a fresh claim. Direction
/// varies with the world salt; always within 2 cells (~120m, tier 0/1).
fn starter_cell() -> IVec3 {
    let s = WORLD_SALT.load(Ordering::Relaxed);
    let pick = |k: u64| {
        // One of {-2, -1, 1, 2} — never 0, so it can't collide with home.
        let u = seed_unit(s.wrapping_add(0xB00B5), k);
        let v = (u * 4.0) as i32 - 2;
        if v >= 0 { v + 1 } else { v }
    };
    IVec3::new(pick(11), pick(22), pick(33))
}

/// The asteroid hosted by a field cell, if any. Pure function of the cell
/// and the world salt.
pub fn asteroid_in_cell(c: IVec3) -> Option<Asteroid> {
    // The home rock owns the origin cell — fixed across days so the depot
    // never moves.
    if c == IVec3::ZERO {
        return Some(Asteroid {
            center: Vec3::ZERO,
            radius: HOME_R,
            seed: 0xCAFE_D00D,
            tier: 0,
            species: 0,
            stretch: Vec3::ONE,
            amp: 0.16,
            freq: 2.0,
            is_planet: false,
            craters: [Vec4::ZERO; 4],
        });
    }
    let starter = c == starter_cell();
    if !starter && hash_unit(c, salted(0xF1E1D)) > CELL_DENSITY {
        return None;
    }
    let j = |salt: u64| 0.25 + 0.5 * hash_unit(c, salted(salt));
    let center = (c.as_vec3() + Vec3::new(j(0xA1), j(0xA2), j(0xA3))) * CELL_M;
    let radius = if starter {
        // A respectable first target, never a pebble.
        R_MIN + 4.0 + hash_unit(c, salted(0xA4)) * 6.0
    } else {
        R_MIN + hash_unit(c, salted(0xA4)).powi(3) * (R_MAX - R_MIN)
    };
    // Keep a clear corridor around the home rock.
    if !starter && center.length() < HOME_R + radius + 10.0 {
        return None;
    }
    // No belt rocks embedded in (or grazing) a planet.
    for p in planet_list().iter() {
        if center.distance(p.center) < p.reach() + radius + 8.0 {
            return None;
        }
    }
    Some(Asteroid::shaped(
        center,
        radius,
        hash3(c, salted(0xA5)),
        // The starter rock is always tier 0 — it's the tutorial target.
        if starter { 0 } else { (center.length() / TIER_M) as i32 },
    ))
}

/// All bodies (belt asteroids AND planets) whose displaced surface could
/// intersect the AABB (meters).
pub fn asteroids_overlapping(min_m: Vec3, max_m: Vec3) -> Vec<Asteroid> {
    let pad = R_MAX * STRETCH_MAX * (0.95 + AMP_MAX) + 0.5;
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
    // Planets are too big for the cell registry — checked directly.
    for p in planet_list().iter() {
        let closest = p.center.clamp(min_m, max_m);
        if closest.distance(p.center) <= p.reach() {
            out.push(*p);
        }
    }
    out
}

/// The nearest asteroid to `p` other than home (gen-space positions), with
/// its body key — demo + future nav use.
pub fn nearest_asteroid_keyed(p: Vec3, exclude_home: bool) -> Option<(IVec3, Asteroid)> {
    let pc = cell_of(p);
    let mut best: Option<(f32, IVec3, Asteroid)> = None;
    for cz in -2..=2 {
        for cy in -2..=2 {
            for cx in -2..=2 {
                let c = pc + IVec3::new(cx, cy, cz);
                let Some(a) = asteroid_in_cell(c) else {
                    continue;
                };
                if exclude_home && a.center == Vec3::ZERO {
                    continue;
                }
                let d = p.distance(a.center) - a.reach();
                if best.is_none_or(|(bd, _, _)| d < bd) {
                    best = Some((d, c, a));
                }
            }
        }
    }
    best.map(|(_, c, a)| (c, a))
}

/// The nearest asteroid to `p` other than home — gen-space convenience.
#[allow(dead_code)]
pub fn nearest_asteroid(p: Vec3, exclude_home: bool) -> Option<Asteroid> {
    nearest_asteroid_keyed(p, exclude_home).map(|(_, a)| a)
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

/// (tier, species) of the asteroid covering this voxel — distance tier and
/// default species in open space.
#[allow(dead_code)] // pure-gen variant kept for tests
pub fn voxel_env(v: IVec3) -> (i32, u8) {
    let p = voxel_center_m(v);
    asteroids_overlapping(p, p)
        .iter()
        .find(|a| {
            p.distance_squared(a.center) <= a.reach() * a.reach()
                && field_single(a, p) > 0.0
        })
        .map_or_else(|| ((p.length() / TIER_M) as i32, 0), |a| (a.tier, a.species))
}

/// Tier of the asteroid covering this voxel (distance tier in open space).
#[allow(dead_code)] // pure-gen variant kept for tests
pub fn tier_of_voxel(v: IVec3) -> i32 {
    voxel_env(v).0
}

/// Where the player materializes: on top of the home rock.
pub fn home_spawn() -> Vec3 {
    static SPAWN: OnceLock<Vec3> = OnceLock::new();
    *SPAWN.get_or_init(|| Vec3::new(0.0, surface_y_at(0.0, 0.0) , 0.0))
}

/// Y (meters) of the first open voxel above the home rock at (x, z).
pub fn surface_y_at(x_m: f32, z_m: f32) -> f32 {
    // Home rock shape: stretch 1, amp 0.16 — bound is r * (0.95 + amp).
    let top = (HOME_R * (0.95 + 0.16) / VOXEL).ceil() as i32 + 2;
    let (vx, vz) = ((x_m / VOXEL).floor() as i32, (z_m / VOXEL).floor() as i32);
    for vy in (-top..=top).rev() {
        if block_at(IVec3::new(vx, vy, vz)) != AIR {
            return (vy + 1) as f32 * VOXEL;
        }
    }
    0.0
}

/// Far-LOD impostor vertex color. Belt rocks: the species tone washed a
/// little toward regolith grey and kept DARK with strong per-vertex contrast
/// — space rock, not candy. Planets: the species tone nearly full-strength
/// and brighter, so a world reads as a colored body from across the system.
pub fn impostor_color(species: u8, jitter01: f32, planet: bool) -> [f32; 3] {
    let rock = ROCK_COLORS[(species as usize) % ROCK_COLORS.len()];
    if planet {
        let j = 0.75 + 0.35 * jitter01;
        [
            (rock[0] * 1.15 + 0.03) * j,
            (rock[1] * 1.15 + 0.03) * j,
            (rock[2] * 1.15 + 0.03) * j,
        ]
    } else {
        let j = (0.38 + 0.50 * jitter01) * 0.8;
        [
            (rock[0] * 0.7 + 0.20 * 0.3) * j,
            (rock[1] * 0.7 + 0.18 * 0.3) * j,
            (rock[2] * 0.7 + 0.17 * 0.3) * j,
        ]
    }
}

/// The species' base rock tone — atmosphere tints and other dressing.
pub fn species_tint(species: u8) -> [f32; 3] {
    ROCK_COLORS[(species as usize) % ROCK_COLORS.len()]
}

/// Per-vertex linear color for a block, with strong per-voxel brightness
/// jitter so untextured cubes read as rubble, not a flat wall. Rock body
/// color follows the asteroid's species; ore follows the tier. On planets
/// the regolith takes a heavy species tint (rust-world dust is rust-colored)
/// so a landed world feels nothing like a belt rock.
fn block_color(id: u8, tier: i32, species: u8, planet: bool, v: IVec3, shell: bool) -> [f32; 3] {
    let t = tier.max(0) as usize;
    let s = species as usize % ROCK_COLORS.len();
    let base = match id {
        REGOLITH => {
            let rock = ROCK_COLORS[s];
            let grey = if shell {
                [0.34, 0.325, 0.30]
            } else {
                [0.30, 0.275, 0.25]
            };
            let k = if planet { 0.62 } else { 0.25 };
            [
                grey[0] * (1.0 - k) + rock[0] * k,
                grey[1] * (1.0 - k) + rock[1] * k,
                grey[2] * (1.0 - k) + rock[2] * k,
            ]
        }
        ROCK => {
            let rock = ROCK_COLORS[s];
            if planet {
                [rock[0] * 1.2 + 0.02, rock[1] * 1.2 + 0.02, rock[2] * 1.2 + 0.02]
            } else {
                rock
            }
        }
        ORE => ORE_COLORS[t % ORE_COLORS.len()],
        BARRIER => [0.09, 0.10, 0.12],
        _ => [1.0, 0.0, 1.0],
    };
    let j = 0.72 + 0.28 * hash_unit(v, 0xC0102);
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

/// A body within collision/interaction range of the player, with the
/// precomputed world↔gen mapping. Refreshed once per frame.
#[derive(Clone, Copy)]
pub struct NearBody {
    pub key: IVec3,
    pub a: Asteroid,
    /// Quantized offset, voxels and meters (always VOXEL-aligned).
    pub off_vox: IVec3,
    pub off_m: Vec3,
    /// Gen-space voxel bounding box for cheap rejection.
    vmin: IVec3,
    vmax: IVec3,
}

impl NearBody {
    fn new(key: IVec3, a: Asteroid, off_vox: IVec3) -> Self {
        let r = a.reach() + VOXEL;
        let vmin = ((a.center - Vec3::splat(r)) / VOXEL).floor().as_ivec3();
        let vmax = ((a.center + Vec3::splat(r)) / VOXEL).ceil().as_ivec3();
        Self {
            key,
            a,
            off_vox,
            off_m: off_vox.as_vec3() * VOXEL,
            vmin,
            vmax,
        }
    }
}

#[derive(Resource, Default)]
pub struct VoxelWorld {
    /// Per-body chunk storage in GEN space, keyed (body, chunk).
    chunks: HashMap<(IVec3, IVec3), Chunk>,
    /// (body, chunk) pairs known to hold none of that body's blocks.
    empty: HashSet<(IVec3, IVec3)>,
    /// Chunks whose mesh is stale and needs a rebuild.
    pub dirty: HashSet<(IVec3, IVec3)>,
    /// Remaining HP of partially-mined blocks, keyed by GEN-space voxel
    /// (bodies are disjoint in gen space, so no body key needed).
    damage: HashMap<IVec3, f32>,
    /// Player edits (mined voxels) in GEN space, forever. Survives unload
    /// AND body movement; applied on regeneration.
    edits: HashMap<IVec3, u8>,
    /// Displacement state per body key. Home (ZERO) never moves.
    motion: HashMap<IVec3, BodyMotion>,
    /// Bodies within interaction range, with mappings. Refreshed per frame.
    nearby: Vec<NearBody>,
}

impl VoxelWorld {
    /// Rebuild the nearby-body list around `center`. Candidates: gen cells in
    /// range (un-moved bodies), every tracked moving body, home, and planets.
    pub fn refresh_nearby(&mut self, center: Vec3, radius: f32) {
        self.nearby.clear();
        let r2 = |a: &Asteroid, off: Vec3| {
            let d = a.center + off - center;
            d.length() < radius + a.reach()
        };
        // Tracked bodies (have motion entries — possibly far from gen cell).
        for (key, m) in &self.motion {
            if m.eaten {
                continue;
            }
            if let Some(a) = body_by_key(*key) {
                if r2(&a, m.offset) {
                    self.nearby.push(NearBody::new(*key, a, m.off_vox));
                }
            }
        }
        // Un-tracked belt cells in gen range (offset zero by definition).
        let lo = cell_of(center - Vec3::splat(radius + CELL_M));
        let hi = cell_of(center + Vec3::splat(radius + CELL_M));
        for cz in lo.z..=hi.z {
            for cy in lo.y..=hi.y {
                for cx in lo.x..=hi.x {
                    let c = IVec3::new(cx, cy, cz);
                    if self.motion.contains_key(&c) {
                        continue; // already added (or eaten)
                    }
                    if let Some(a) = asteroid_in_cell(c) {
                        if r2(&a, Vec3::ZERO) {
                            self.nearby.push(NearBody::new(c, a, IVec3::ZERO));
                        }
                    }
                }
            }
        }
        // Planets not yet tracked (early frames).
        for (i, p) in planet_list().iter().enumerate() {
            let key = planet_key(i);
            if !self.motion.contains_key(&key) && r2(p, Vec3::ZERO) {
                self.nearby.push(NearBody::new(key, *p, IVec3::ZERO));
            }
        }
    }

    pub fn nearby(&self) -> &[NearBody] {
        &self.nearby
    }

    /// Resolve a WORLD voxel to (body, gen voxel), if any nearby body covers it.
    #[inline]
    fn resolve(&self, v: IVec3) -> Option<(&NearBody, IVec3)> {
        for nb in &self.nearby {
            let g = v - nb.off_vox;
            if g.cmplt(nb.vmin).any() || g.cmpgt(nb.vmax).any() {
                continue;
            }
            return Some((nb, g));
        }
        None
    }

    /// Block id at a WORLD voxel: maps through each nearby body's offset,
    /// then storage → edits → pure single-body gen.
    pub fn block(&self, v: IVec3) -> u8 {
        for nb in &self.nearby {
            let g = v - nb.off_vox;
            if g.cmplt(nb.vmin).any() || g.cmpgt(nb.vmax).any() {
                continue;
            }
            let b = self.block_gen(nb.key, &nb.a, g);
            if b != AIR {
                return b;
            }
        }
        AIR
    }

    /// Block id in one body's GEN space (storage / edits / pure gen).
    fn block_gen(&self, key: IVec3, a: &Asteroid, g: IVec3) -> u8 {
        let cp = chunk_of(g);
        if let Some(c) = self.chunks.get(&(key, cp)) {
            return c.blocks[local_index(g)];
        }
        if self.empty.contains(&(key, cp)) {
            return AIR;
        }
        if let Some(&b) = self.edits.get(&g) {
            return b;
        }
        let p = voxel_center_m(g);
        if p.distance_squared(a.center) > a.reach() * a.reach() {
            return AIR;
        }
        classify(a, g, p, field_single(a, p)).unwrap_or(AIR)
    }

    #[inline]
    pub fn solid(&self, v: IVec3) -> bool {
        self.block(v) != AIR
    }

    /// (tier, species) governing a WORLD voxel.
    pub fn env_of(&self, v: IVec3) -> (i32, u8) {
        match self.resolve(v) {
            Some((nb, _)) => (nb.a.tier, nb.a.species),
            None => (((v.as_vec3() * VOXEL).length() / TIER_M) as i32, 0),
        }
    }

    /// Remaining-HP accessors for a WORLD voxel (stored in gen space).
    pub fn damage_of(&self, v: IVec3) -> Option<f32> {
        let (_, g) = self.resolve(v)?;
        self.damage.get(&g).copied()
    }
    pub fn set_damage(&mut self, v: IVec3, hp: f32) {
        if let Some((_, g)) = self.resolve(v) {
            self.damage.insert(g, hp);
        }
    }

    /// The smooth visual offset of a body (ZERO if untracked).
    pub fn offset_of(&self, key: IVec3) -> Vec3 {
        self.motion.get(&key).map_or(Vec3::ZERO, |m| m.offset)
    }
    pub fn is_eaten(&self, key: IVec3) -> bool {
        self.motion.get(&key).is_some_and(|m| m.eaten)
    }
    #[allow(dead_code)] // test access
    pub fn motion_mut(&mut self) -> &mut HashMap<IVec3, BodyMotion> {
        &mut self.motion
    }

    /// How far the body under the player's feet moved this frame — applied
    /// to the player so a falling rock carries its passenger. Also answers
    /// for a player EMBEDDED in a body (walls moved into them).
    pub fn carrier_delta(&self, feet: Vec3) -> Vec3 {
        for probe in [
            feet + Vec3::new(0.0, -0.05, 0.0),
            feet,
            feet + Vec3::new(0.0, 0.7, 0.0),
        ] {
            let v = (probe / VOXEL).floor().as_ivec3();
            if let Some((nb, g)) = self.resolve(v) {
                if self.block_gen(nb.key, &nb.a, g) != AIR {
                    return self.motion.get(&nb.key).map_or(Vec3::ZERO, |m| m.delta);
                }
            }
        }
        Vec3::ZERO
    }

    pub fn is_loaded(&self, key: IVec3, cp: IVec3) -> bool {
        self.chunks.contains_key(&(key, cp)) || self.empty.contains(&(key, cp))
    }

    /// Generate (or regenerate) one body's chunk in gen space. Returns false
    /// if it holds none of that body's blocks.
    pub fn generate_chunk(&mut self, key: IVec3, a: &Asteroid, cp: IVec3) -> bool {
        let (min_m, max_m) = chunk_bounds_m(cp);
        let closest = a.center.clamp(min_m, max_m);
        if closest.distance(a.center) > a.reach() {
            self.empty.insert((key, cp));
            return false;
        }
        let grid = FieldGrid::for_box(a, min_m, max_m);
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
                            if p.distance_squared(a.center) > a.reach() * a.reach() {
                                AIR
                            } else {
                                classify(a, v, p, grid.sample(p)).unwrap_or(AIR)
                            }
                        }
                    };
                    any_solid |= b != AIR;
                    blocks[local_index(v)] = b;
                }
            }
        }
        if !any_solid {
            self.empty.insert((key, cp));
            return false;
        }
        self.chunks.insert((key, cp), Chunk { blocks });
        self.dirty.insert((key, cp));
        true
    }

    pub fn unload_chunk(&mut self, key: IVec3, cp: IVec3) {
        self.chunks.remove(&(key, cp));
        self.dirty.remove(&(key, cp));
    }

    /// Drop ALL storage for a consumed body. Edits are kept (harmless — gen
    /// space is never re-queried for an eaten body this day).
    pub fn consume_body(&mut self, key: IVec3) {
        self.chunks.retain(|(k, _), _| *k != key);
        self.empty.retain(|(k, _)| *k != key);
        self.dirty.retain(|(k, _)| *k != key);
        if let Some(m) = self.motion.get_mut(&key) {
            m.eaten = true;
        }
    }

    pub fn forget_empty(&mut self, keep_near: IVec3, radius: i32) {
        self.empty
            .retain(|(_, cp)| (*cp - keep_near).abs().max_element() <= radius * 3);
    }

    /// Day reset: drop ALL storage, edits, damage, and motion, and reseed
    /// worldgen. Chunks restream around the player on the following frames;
    /// the home rock regenerates pristine.
    pub fn reset_for_new_day(&mut self, salt: u64) {
        set_world_salt(salt);
        self.chunks.clear();
        self.empty.clear();
        self.dirty.clear();
        self.damage.clear();
        self.edits.clear();
        self.motion.clear();
        self.nearby.clear();
    }

    /// Remove a block at a WORLD voxel (mined out). Records the gen-space
    /// edit and marks the owning chunk — and face-adjacent neighbors — dirty.
    pub fn set_air(&mut self, v: IVec3) {
        let Some((nb, g)) = self.resolve(v) else { return };
        let key = nb.key;
        let a = nb.a;
        self.edits.insert(g, AIR);
        let cp = chunk_of(g);
        if let Some(c) = self.chunks.get_mut(&(key, cp)) {
            c.blocks[local_index(g)] = AIR;
        }
        self.damage.remove(&g);
        if self.chunks.contains_key(&(key, cp)) {
            self.dirty.insert((key, cp));
        }
        let _ = a;
        for d in [
            IVec3::X,
            IVec3::NEG_X,
            IVec3::Y,
            IVec3::NEG_Y,
            IVec3::Z,
            IVec3::NEG_Z,
        ] {
            let ncp = chunk_of(g + d);
            if ncp != cp && self.chunks.contains_key(&(key, ncp)) {
                self.dirty.insert((key, ncp));
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

/// Cast against every nearby body (each in its own gen frame, shifted by its
/// quantized offset) and keep the nearest hit. Hit voxel/normal are WORLD.
pub fn raycast(world: &VoxelWorld, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RayHit> {
    let mut best: Option<RayHit> = None;
    for nb in world.nearby() {
        // Cheap rejection: ray vs the body's current bounding sphere.
        let center = nb.a.center + nb.off_m;
        let to = center - origin;
        let along = to.dot(dir.normalize_or_zero());
        let closest2 = to.length_squared() - along * along;
        let r = nb.a.reach() + 1.0;
        if closest2 > r * r || (along < -r) || (along - r > max_dist) {
            continue;
        }
        if let Some(mut hit) = raycast_one(world, nb, origin - nb.off_m, dir, max_dist) {
            hit.voxel += nb.off_vox;
            if best.as_ref().is_none_or(|b| hit.t < b.t) {
                best = Some(hit);
            }
        }
    }
    best
}

/// DDA through one body's gen-space grid.
fn raycast_one(
    world: &VoxelWorld,
    nb: &NearBody,
    origin: Vec3,
    dir: Vec3,
    max_dist: f32,
) -> Option<RayHit> {
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
        if cell.cmpge(nb.vmin).all()
            && cell.cmple(nb.vmax).all()
            && world.block_gen(nb.key, &nb.a, cell) != AIR
        {
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

/// Build both meshes for one BODY's chunk in gen space: (lit rock mesh,
/// unlit HDR crystal-glow mesh). Verts are gen-space; the body root's
/// transform carries them to the body's current position.
pub fn mesh_chunk(world: &VoxelWorld, key: IVec3, a: &Asteroid, cp: IVec3) -> (Mesh, Mesh) {
    // Event-driven (not per-frame-hot): a few transient Vecs per remesh is fine.
    let mut solid = MeshScratch::new();
    let mut glow = MeshScratch::new();

    // One coarse field grid for this body; tier/shell per voxel is a trilerp.
    let (min_m, max_m) = chunk_bounds_m(cp);
    let grid = FieldGrid::for_box(a, min_m, max_m);
    let tier_at = |p: Vec3| -> (i32, u8, bool, bool) {
        if p.distance_squared(a.center) <= a.reach() * a.reach() {
            let f = grid.sample(p);
            if f > 0.0 {
                return (a.tier, a.species, a.is_planet, f < SHELL_M);
            }
        }
        (0, 0, false, false)
    };

    let origin = cp * CHUNK;
    for y in 0..CHUNK {
        for z in 0..CHUNK {
            for x in 0..CHUNK {
                let v = origin + IVec3::new(x, y, z);
                let id = world.block_gen(key, a, v);
                if id == AIR {
                    continue;
                }
                let (tier, species, planet, shell) = tier_at(voxel_center_m(v));
                let col = block_color(id, tier, species, planet, v, shell);
                let base = (v.as_vec3()) * VOXEL;
                for face in &FACES {
                    if world.block_gen(key, a, v + face.dir) != AIR {
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

/// Solid + glow mesh entities for each materialized (body, chunk), plus one
/// root entity per body — chunk meshes are its children, and moving the body
/// is just moving the root's transform.
#[derive(Resource, Default)]
pub struct ChunkEntities {
    map: HashMap<(IVec3, IVec3), (Entity, Entity)>,
    roots: HashMap<IVec3, Entity>,
}

impl ChunkEntities {
    fn root(&mut self, key: IVec3, commands: &mut Commands) -> Entity {
        *self.roots.entry(key).or_insert_with(|| {
            commands
                .spawn((Transform::IDENTITY, Visibility::Visible, BodyRoot(key)))
                .id()
        })
    }

    /// Despawn one body's root (children — all its chunk meshes — included).
    pub fn despawn_body(&mut self, key: IVec3, commands: &mut Commands) {
        if let Some(root) = self.roots.remove(&key) {
            commands.entity(root).despawn();
        }
        self.map.retain(|(k, _), _| *k != key);
    }

    /// Day reset: despawn every body root (and with them all chunk meshes).
    pub fn despawn_all(&mut self, commands: &mut Commands) {
        for (_, root) in self.roots.drain() {
            commands.entity(root).despawn();
        }
        self.map.clear();
    }
}

/// Marks a body's mesh root; the wrapped key indexes BodyMotion.
#[derive(Component)]
pub struct BodyRoot(pub IVec3);

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
                (
                    body_motion,
                    refresh_nearby_system,
                    ensure_chunks,
                    remesh_dirty,
                    unload_far_chunks,
                    sync_body_roots,
                )
                    .chain()
                    .before(crate::GameplaySet),
            );
    }
}

// ---------------------------------------------------------------------------
// Body motion — the hole REALLY drags the field in
// ---------------------------------------------------------------------------

/// Tracking radius around the player: every body that can be seen as an
/// impostor gets real motion state (and a backfilled offset on first sight).
const TRACK_RADIUS_M: f32 = 950.0;
/// Kinematic infall: terminal velocity ~ pull * TAU, dead below the
/// threshold so the dawn field is still, capped so collision stays sane.
const INFALL_TAU: f32 = 7.0;
const INFALL_MIN_PULL: f32 = 0.22;
const INFALL_MAX_V: f32 = 90.0;
/// Backfill integration step (s) when a body is first tracked mid-day.
const BACKFILL_STEP: f32 = 2.0;

fn infall_speed(pull: f32) -> f32 {
    ((pull - INFALL_MIN_PULL).max(0.0) * INFALL_TAU).min(INFALL_MAX_V)
}

/// Advance every tracked body toward the hole; consume bodies the horizon
/// reaches (their REAL chunks despawn); register newly-visible bodies with a
/// deterministic backfilled displacement.
#[allow(clippy::too_many_arguments)]
fn body_motion(
    time: Res<Time>,
    day: Res<crate::blackhole::DayState>,
    bh: Res<crate::blackhole::BlackHole>,
    player: Res<crate::player::PlayerState>,
    mut world: ResMut<VoxelWorld>,
    mut chunk_entities: ResMut<ChunkEntities>,
    eat_fx: Option<Res<crate::blackhole::EatFx>>,
    mut sfx: ResMut<crate::audio::SfxQueue>,
    mut commands: Commands,
    mut scan_tick: Local<u32>,
) {
    let dt = time.delta_secs().min(0.05);

    // Periodic discovery sweep: register bodies entering tracking range.
    // Planets and home register on the first pass regardless of distance.
    *scan_tick = scan_tick.wrapping_add(1);
    if *scan_tick % 31 == 1 {
        let register = |key: IVec3, a: &Asteroid, world: &mut VoxelWorld| {
            if world.motion.contains_key(&key) {
                return;
            }
            let mut m = BodyMotion::default();
            if key != IVec3::ZERO {
                // Backfill: integrate the infall this body already suffered
                // since dawn, from the analytic hole history. Deterministic,
                // so a rock looks the same whenever you first see it.
                let mut t = 0.0;
                let mut center = a.center;
                while t < day.elapsed {
                    let step = BACKFILL_STEP.min(day.elapsed - t);
                    let (hc, gm) = crate::blackhole::hole_kinematics(t, day.day_len);
                    let to = hc - center;
                    let d2 = to.length_squared().max(10_000.0);
                    let pull = (gm / d2).min(65.0);
                    let v = infall_speed(pull);
                    center += to.normalize_or_zero() * v * step;
                    t += step;
                }
                m.offset = center - a.center;
                m.off_vox = (m.offset / VOXEL).round().as_ivec3();
            }
            m.backfilled = true;
            world.motion.insert(key, m);
        };
        let pc = cell_of(player.pos);
        let r_cells = (TRACK_RADIUS_M / CELL_M).ceil() as i32;
        for cz in -r_cells..=r_cells {
            for cy in -r_cells..=r_cells {
                for cx in -r_cells..=r_cells {
                    let c = pc + IVec3::new(cx, cy, cz);
                    if world.motion.contains_key(&c) {
                        continue;
                    }
                    let Some(a) = asteroid_in_cell(c) else { continue };
                    if (a.center - player.pos).length() < TRACK_RADIUS_M + a.reach() {
                        register(c, &a, &mut world);
                    }
                }
            }
        }
        for (i, p) in planet_list().iter().enumerate() {
            register(planet_key(i), p, &mut world);
        }
        register(IVec3::ZERO, &asteroid_in_cell(IVec3::ZERO).unwrap(), &mut world);
    }

    // Advance the living; feed the horizon.
    let mut consumed: Vec<(IVec3, Vec3)> = Vec::new();
    for (key, m) in world.motion.iter_mut() {
        if m.eaten || *key == IVec3::ZERO {
            m.delta = Vec3::ZERO;
            continue;
        }
        let Some(a) = body_by_key(*key) else { continue };
        let center = a.center + m.offset;
        let pull = bh.pull(center).length();
        let to = (bh.center - center).normalize_or_zero();
        m.offset += to * infall_speed(pull) * dt;
        let new_q = (m.offset / VOXEL).round().as_ivec3();
        m.delta = (new_q - m.off_vox).as_vec3() * VOXEL;
        m.off_vox = new_q;
        if (a.center + m.offset).distance(bh.center) < bh.horizon_r {
            consumed.push((*key, a.center + m.offset));
        }
    }
    for (key, where_) in consumed {
        // Rare event; this log line is the proof that REAL terrain went in.
        info!("hole consumed body {key} at {where_}");
        world.consume_body(key);
        chunk_entities.despawn_body(key, &mut commands);
        if let Some(fx) = &eat_fx {
            crate::blackhole::spawn_eat_streaks(&mut commands, fx, where_, bh.center);
        }
        // A world dying is loud; a pebble is not.
        if key.x >= PLANET_KEY_BASE {
            sfx.push(crate::audio::SfxEvent::Explosion);
        }
    }
}

fn refresh_nearby_system(
    player: Res<crate::player::PlayerState>,
    mut world: ResMut<VoxelWorld>,
) {
    // Covers collision, mining reach, enclosure probes, drops, and charges.
    world.refresh_nearby(player.pos, 140.0);
}

/// Slide each body's mesh root to its smooth offset.
fn sync_body_roots(world: Res<VoxelWorld>, mut roots: Query<(&BodyRoot, &mut Transform)>) {
    for (root, mut tf) in &mut roots {
        tf.translation = world.offset_of(root.0);
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

/// Materialize chunks around the player, nearest first, within a shared
/// per-frame budget — PER BODY, in each body's gen frame (the player's gen
/// position differs per body by its offset). Pure-vacuum chunks cost a set
/// entry, not storage or entities.
fn ensure_chunks(
    player: Res<crate::player::PlayerState>,
    order: Res<GenOrder>,
    mut world: ResMut<VoxelWorld>,
    mut chunk_entities: ResMut<ChunkEntities>,
    mut meshes: ResMut<Assets<Mesh>>,
    assets: Res<WorldAssets>,
    mut commands: Commands,
) {
    let mut budget = GEN_BUDGET;
    let stream_reach = GEN_RADIUS as f32 * CHUNK_M;
    let bodies: Vec<NearBody> = world
        .nearby()
        .iter()
        .filter(|nb| {
            (nb.a.center + nb.off_m - player.pos).length() < nb.a.reach() + stream_reach
        })
        .copied()
        .collect();
    for nb in bodies {
        if budget == 0 {
            break;
        }
        // The player's position in this body's gen frame.
        let player_gen = player.pos - nb.off_m;
        let pc = chunk_of((player_gen / VOXEL).floor().as_ivec3());
        // Body's chunk-space bounding box for cheap rejection.
        let cmin = chunk_of(nb.vmin);
        let cmax = chunk_of(nb.vmax);
        let root = chunk_entities.root(nb.key, &mut commands);
        for off in &order.0 {
            if budget == 0 {
                break;
            }
            let cp = pc + *off;
            if cp.cmplt(cmin).any() || cp.cmpgt(cmax).any() {
                continue;
            }
            if world.is_loaded(nb.key, cp) {
                continue;
            }
            budget -= 1;
            if !world.generate_chunk(nb.key, &nb.a, cp) {
                continue; // vacuum
            }
            let mut spawn_mesh = |material: &Handle<StandardMaterial>| {
                let handle = meshes.add(Mesh::new(
                    PrimitiveTopology::TriangleList,
                    RenderAssetUsages::default(),
                ));
                let e = commands
                    .spawn((
                        Mesh3d(handle),
                        MeshMaterial3d(material.clone()),
                        Transform::IDENTITY,
                        Visibility::Hidden,
                        ChunkMesh,
                    ))
                    .id();
                commands.entity(root).add_child(e);
                e
            };
            let solid = spawn_mesh(&assets.material);
            let glow = spawn_mesh(&assets.glow_material);
            chunk_entities.map.insert((nb.key, cp), (solid, glow));
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
    let batch: Vec<(IVec3, IVec3)> = world.dirty.iter().copied().take(REMESH_BUDGET).collect();
    for (key, cp) in batch {
        world.dirty.remove(&(key, cp));
        let Some(&(solid_e, glow_e)) = chunk_entities.map.get(&(key, cp)) else {
            continue;
        };
        let Some(a) = body_by_key(key) else { continue };
        let (solid_mesh, glow_mesh) = mesh_chunk(&world, key, &a, cp);
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

/// Drop chunk storage + mesh entities far behind the player (measured at the
/// body's CURRENT position). Edits persist in the gen-space overlay, so
/// flying back regenerates everything exactly as you left it.
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
    let far: Vec<(IVec3, IVec3)> = chunk_entities
        .map
        .keys()
        .filter(|(key, cp)| {
            let off = world.offset_of(*key);
            let world_center =
                (cp.as_vec3() + Vec3::splat(0.5)) * CHUNK_M + off;
            (world_center - player.pos).length()
                > (UNLOAD_RADIUS as f32 + 1.0) * CHUNK_M
        })
        .copied()
        .collect();
    for (key, cp) in far {
        if let Some((a, b)) = chunk_entities.map.remove(&(key, cp)) {
            commands.entity(a).despawn();
            commands.entity(b).despawn();
        }
        world.unload_chunk(key, cp);
    }
    let pc = chunk_of((player.pos / VOXEL).floor().as_ivec3());
    world.forget_empty(pc, UNLOAD_RADIUS + 8);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Materialize every body's chunks within `r_m` meters of a point, and
    /// refresh the nearby list so world-space queries resolve.
    fn gen_sphere(w: &mut VoxelWorld, center: Vec3, r_m: f32) {
        let r_c = (r_m / CHUNK_M).ceil() as i32;
        let cc = chunk_of((center / VOXEL).floor().as_ivec3());
        for z in -r_c..=r_c {
            for y in -r_c..=r_c {
                for x in -r_c..=r_c {
                    let cp = cc + IVec3::new(x, y, z);
                    let (min_m, max_m) = chunk_bounds_m(cp);
                    for (key, a) in bodies_overlapping(min_m, max_m) {
                        w.generate_chunk(key, &a, cp);
                    }
                }
            }
        }
        w.refresh_nearby(center, r_m + 80.0);
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
        // The starter guarantee: an asteroid within 2 cells of home, easy tier.
        let near = nearest_asteroid(Vec3::ZERO, true).expect("field should not be empty");
        assert!(near.center.length() > HOME_R);
        assert!(near.center.length() < 3.0 * CELL_M * 1.8);
        assert!(near.tier <= 1);
        // Ordinary (non-starter) asteroids take their tier from distance.
        let mut checked = false;
        'outer: for cz in -6..=6 {
            for cy in -6..=6 {
                for cx in -6..=6 {
                    let c = IVec3::new(cx, cy, cz);
                    if c == IVec3::ZERO || c == starter_cell() {
                        continue;
                    }
                    if let Some(a) = asteroid_in_cell(c) {
                        assert_eq!(a.tier, (a.center.length() / TIER_M) as i32);
                        checked = true;
                        break 'outer;
                    }
                }
            }
        }
        assert!(checked, "expected at least one ordinary asteroid within 6 cells");
    }

    #[test]
    fn planets_form_a_landable_solar_system() {
        let ps = planet_list();
        assert_eq!(ps.len(), PLANET_COUNT);
        // Deterministic.
        let ps2 = planet_list();
        assert_eq!(ps[0].center, ps2[0].center);
        let axis = SYSTEM_AXIS.normalize();
        let seat = axis * STAR_DIST;
        let side = axis.cross(Vec3::Y).normalize();
        let mut last_orbit = 0.0;
        let mut longitudes = Vec::new();
        for (i, p) in ps.iter().enumerate() {
            // Concentric orbits around the star's seat, tiers falling with
            // orbital distance — the endgame lives closest to the dying star.
            let orbit = p.center.distance(seat);
            assert!(orbit > last_orbit, "planet {i} orbit out of order");
            assert!(orbit > 900.0 && orbit < 4_800.0);
            last_orbit = orbit;
            if i > 0 {
                assert!(p.tier <= ps[i - 1].tier);
            }
            let rel = p.center - seat;
            longitudes.push(f32::atan2(rel.dot(side), rel.dot(axis)));
            assert!(p.radius >= PLANET_R_MIN && p.radius <= PLANET_R_MAX);
            assert!(p.is_planet);
            // Landable: solid voxels just under the surface, vacuum above —
            // same worldgen pipeline as any asteroid.
            let dir = (Vec3::ZERO - p.center).normalize();
            let surf = p.surface_toward(p.center + dir);
            let solid_p = p.center + dir * (surf - 1.5);
            let air_p = p.center + dir * (surf + 3.0);
            let v_solid = (solid_p / VOXEL).floor().as_ivec3();
            let v_air = (air_p / VOXEL).floor().as_ivec3();
            assert_ne!(block_at(v_solid), AIR, "planet {i} has no ground");
            assert_eq!(block_at(v_air), AIR, "planet {i} surface buried");
            // Mining it pays its own tier.
            assert_eq!(tier_of_voxel(v_solid), p.tier);
        }
        assert!(ps.first().unwrap().tier >= 10, "innermost planet must be endgame");
        // Spread around the star, not clumped on one bearing: the largest
        // empty arc between neighboring longitudes stays under half a turn.
        longitudes.sort_by(f32::total_cmp);
        let mut max_gap: f32 = 0.0;
        for w in longitudes.windows(2) {
            max_gap = max_gap.max(w[1] - w[0]);
        }
        max_gap = max_gap.max(
            longitudes[0] + std::f32::consts::TAU - longitudes[longitudes.len() - 1],
        );
        assert!(max_gap < std::f32::consts::PI, "planets clumped: gap {max_gap}");
    }

    #[test]
    fn asteroids_have_shape_and_species_variety() {
        // Scan a swath of cells; the field should not be all one species or
        // one shape.
        let mut species = std::collections::HashSet::new();
        let mut min_amp = f32::MAX;
        let mut max_amp = f32::MIN;
        for cz in -14..=14 {
            for cy in -3..=3 {
                for cx in -14..=14 {
                    if let Some(a) = asteroid_in_cell(IVec3::new(cx, cy, cz)) {
                        species.insert(a.species);
                        min_amp = min_amp.min(a.amp);
                        max_amp = max_amp.max(a.amp);
                        // Shape params stay inside their declared bounds
                        // (reach() and the overlap pad depend on it).
                        assert!(a.stretch.max_element() <= STRETCH_MAX + 1e-4);
                        assert!(a.stretch.min_element() >= STRETCH_MIN - 1e-4);
                        assert!(a.amp <= AMP_MAX + 1e-4 && a.amp >= AMP_MIN - 1e-4);
                    }
                }
            }
        }
        assert!(species.len() >= 4, "want species variety, got {species:?}");
        assert!(max_amp - min_amp > 0.05, "want shape variety");
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
        let home = asteroid_in_cell(IVec3::ZERO).unwrap();
        let cp = chunk_of(v);
        w.unload_chunk(IVec3::ZERO, cp);
        assert!(!w.solid(v), "edit must survive via fallback");
        // Regenerate: the edit must be baked back into chunk storage.
        w.generate_chunk(IVec3::ZERO, &home, cp);
        assert!(!w.solid(v), "edit must survive regeneration");
    }

    #[test]
    fn empty_space_is_air_and_bodies_move_with_offsets() {
        let mut w = VoxelWorld::default();
        // Far empty space: nothing nearby, everything is vacuum.
        w.refresh_nearby(Vec3::new(20_000.0, 9_000.0, -14_000.0), 100.0);
        assert_eq!(w.block(IVec3::new(80_000, 36_000, -56_000)), AIR);

        // A displaced body answers at its NEW location with its own terrain.
        let (cell, a) = nearest_asteroid_keyed(Vec3::ZERO, true).expect("starter exists");
        let shift = IVec3::new(400, 0, 0); // 100m — well clear of the original
        let mut m = BodyMotion::default();
        m.offset = shift.as_vec3() * VOXEL;
        m.off_vox = shift;
        w.motion_mut().insert(cell, m);
        w.refresh_nearby(a.center + m.offset, 90.0);
        let gen_v = (a.center / VOXEL).floor().as_ivec3();
        let world_v = gen_v + shift;
        assert_eq!(w.block(world_v), block_at(gen_v), "terrain rides the offset");
        assert_ne!(w.block(world_v), AIR, "asteroid core should be solid");
        assert_eq!(w.env_of(world_v), (a.tier, a.species));

        // Consumption drops the body for real.
        w.consume_body(cell);
        w.refresh_nearby(a.center + shift.as_vec3() * VOXEL, 90.0);
        assert_eq!(w.block(world_v), AIR, "eaten bodies leave vacuum");
        assert!(w.is_eaten(cell));
    }

    /// Chunk contents must equal pure worldgen everywhere (the block()
    /// fallback path depends on it). Run with --nocapture to also eyeball an
    /// ASCII slice of the home rock.
    #[test]
    fn chunk_storage_matches_pure_gen() {
        let mut w = VoxelWorld::default();
        for &(c, r) in &[(Vec3::ZERO, 22.0), (Vec3::new(33.0, 31.0, -31.0), 18.0)] {
            gen_sphere(&mut w, c, r);
        }
        // One nearby list covering both probe regions (offsets are all zero).
        w.refresh_nearby(Vec3::new(16.0, 15.0, -15.0), 120.0);
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
        let home = asteroid_in_cell(IVec3::ZERO).unwrap();
        let cc = chunk_of((Vec3::ZERO / VOXEL).floor().as_ivec3());
        let r = 4; // 9^3 chunks around home = mix of rock + vacuum
        let mut rock_chunks = Vec::new();

        let t = Instant::now();
        for z in -r..=r {
            for y in -r..=r {
                for x in -r..=r {
                    let cp = cc + IVec3::new(x, y, z);
                    if w.generate_chunk(IVec3::ZERO, &home, cp) {
                        rock_chunks.push(cp);
                    }
                }
            }
        }
        let gen_ms = t.elapsed().as_secs_f64() * 1000.0;
        let total = (2 * r + 1) * (2 * r + 1) * (2 * r + 1);

        let t = Instant::now();
        for cp in &rock_chunks {
            let _ = mesh_chunk(&w, IVec3::ZERO, &home, *cp);
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
        let home = asteroid_in_cell(IVec3::ZERO).unwrap();
        let cp = chunk_of((spawn / VOXEL).floor().as_ivec3());
        let (solid, _glow) = mesh_chunk(&w, IVec3::ZERO, &home, cp);
        assert!(solid.count_vertices() > 0);
    }
}
