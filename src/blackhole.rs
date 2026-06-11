//! THE BLACK HOLE — the game's clock, antagonist, and centerpiece.
//!
//! A real world-space singularity (not sky furniture) that starts ~6.5km out
//! and closes in over one "day" (~10 minutes, `LIMITS_DAY_S` to override).
//! Its pull is felt by the player, loot, debris, and plasma charges, ramping
//! from a barely-there drift to inescapable drag. When the day expires — or
//! you stray past the photon ring — the dive begins: control is lost, the
//! camera locks onto the horizon, speed ramps to relativistic silliness, the
//! screen whites out... and a new day dawns. Upgrades survive; credits, the
//! hold, and the asteroid field itself do not (the field reseeds per day).
//!
//! Visuals, all procedural HDR-fed-into-bloom (no shaders, no assets):
//! an unlit-black event horizon sphere, a billboarded photon ring + outer
//! glow halo (so the lensing ring reads from any approach angle), a
//! doppler-beamed accretion disc with log-spaced rings, and a swarm of
//! infalling debris streaks spiraling down the disc plane.

use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::asset::RenderAssetUsages;

use crate::audio::{SfxEvent, SfxQueue};
use crate::game::{Inventory, StatusMsg, Wallet};
use crate::items::{Debris, DropItem, PlasmaCharge};
use crate::lod::Impostors;
use crate::player::{LaserState, PlayerState, Shake};
use crate::sky::ring_mesh;
use crate::world::{ChunkEntities, VoxelWorld};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// The system axis and the star's seat — shared with worldgen, which strings
/// the planets along the same ray (closer to the star = higher tier).
const BH_DIR: Vec3 = crate::world::SYSTEM_AXIS;
const DIST_START: f32 = crate::world::STAR_DIST;
const DIST_END: f32 = 1_350.0;
/// Event horizon radius (m) at FULL size. The hole is born at half this and
/// grows all day as it feeds; visuals scale with it via the root transform.
const HORIZON_R: f32 = 650.0;
/// Accretion disc annulus (m), at full size — the disc alone spans ~60° of
/// sky from the home rock at dawn and only grows from there.
const DISC_IN: f32 = HORIZON_R * 1.40;
const DISC_OUT: f32 = HORIZON_R * 5.2;
/// Real-time session length (s). `LIMITS_DAY_S` overrides for testing.
const DAY_S_DEFAULT: f32 = 600.0;

/// The day OPENS with the system's star collapsing: it hangs in the sky
/// through the dawn fade, swells, detonates (supernova flash + a shockwave
/// shell that rolls over the player), and falls into itself — the hole
/// assembly then grows out of the wreckage. Timed against Active `elapsed`.
const STAR_R: f32 = 800.0;
const STAR_SWELL_S: f32 = 1.6;
const STAR_DETONATE_S: f32 = 4.0;
const STAR_COLLAPSE_S: f32 = 1.3;
const HOLE_GROW_S: f32 = 2.6;
const SHOCK_SPEED: f32 = 1_000.0;

/// Pull at the home rock at day start / nominal day end (m/s²). Thrust is 16
/// — past the crossover there is no escape. There is NO timer: past the
/// nominal day the hole simply keeps coming (advancing along its ray, sweeping
/// the field) and keeps growing (gm doubles every OVERTIME_DOUBLE_S), so both
/// hiding behind rock and fleeing into deep space only buy minutes.
const PULL_HOME_START: f32 = 0.02;
const PULL_HOME_END: f32 = 26.0;
/// Acceleration cap so the math stays integrable at point-blank range.
const PULL_CAP: f32 = 65.0;
/// Grounded players get ripped off the surface above this pull (m/s²).
const RIP_ACCEL: f32 = 7.0;
/// Overtime mass growth: gm doubles this often (s) once the nominal day is
/// spent — guarantees pull eventually beats thrust at ANY distance.
const OVERTIME_DOUBLE_S: f32 = 45.0;

/// Begin the death dive inside this multiple of the horizon radius...
const FALL_TRIGGER: f32 = 1.2;
/// ...or when pull is hopeless (>2.5x thrust) AND you're already being
/// dragged inward this fast (m/s). A player pinned against rock doesn't
/// trip this — the advancing horizon comes for them instead.
const DIVE_PULL: f32 = 40.0;
const DIVE_INWARD_V: f32 = 12.0;
/// The dive "lands" (reset fires) inside this multiple.
const FALL_IMPACT: f32 = 0.5;
/// Dawn fade-in length (s).
const DAWN_S: f32 = 3.5;

const N_STREAKS: usize = 64;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum DayPhase {
    /// Normal play; the clock runs.
    Active,
    /// The dive: control surrendered, camera locked, speed ramping.
    Falling,
    /// Post-reset white fade with the day splash.
    Dawn,
}

#[derive(Resource)]
pub struct DayState {
    pub day: u32,
    /// Seconds into the Active phase.
    pub elapsed: f32,
    pub day_len: f32,
    pub phase: DayPhase,
    /// Seconds into the current Falling/Dawn phase.
    pub phase_t: f32,
    /// 0..1 white overlay the HUD renders (dive impact + dawn fade).
    pub whiteout: f32,
    /// Set on Falling→Dawn; consumed by the reset system the same frame.
    pending_reset: bool,
}

impl DayState {
    /// Nominal-day fraction spent, 0..1 (the approach schedule, not a cutoff).
    pub fn frac(&self) -> f32 {
        (self.elapsed / self.day_len).clamp(0.0, 1.0)
    }
    pub fn remaining(&self) -> f32 {
        (self.day_len - self.elapsed).max(0.0)
    }
    /// Seconds past the nominal day — the hole's runaway phase.
    pub fn overtime(&self) -> f32 {
        (self.elapsed - self.day_len).max(0.0)
    }
}

impl Default for DayState {
    fn default() -> Self {
        let day_len = std::env::var("LIMITS_DAY_S")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(DAY_S_DEFAULT);
        Self {
            day: 1,
            elapsed: 0.0,
            day_len,
            // Boot into the tail of a Dawn so day 1 opens with the same white
            // fade + splash every later day gets.
            phase: DayPhase::Dawn,
            phase_t: 1.2,
            whiteout: 1.0,
            pending_reset: false,
        }
    }
}

#[derive(Resource)]
pub struct BlackHole {
    pub center: Vec3,
    pub horizon_r: f32,
    /// Gravitational parameter (m³/s²-ish); pull = gm / d².
    gm: f32,
    /// 0..1 "how doomed are we" — drives the rumble loop and HUD vignette.
    pub dread: f32,
}

impl BlackHole {
    fn at(frac: f32) -> Self {
        let dist = DIST_START + (DIST_END - DIST_START) * frac.powf(1.6);
        let pull_home =
            PULL_HOME_START * (PULL_HOME_END / PULL_HOME_START).powf(frac);
        Self {
            center: BH_DIR.normalize() * dist,
            // Starts at zero: the hole doesn't exist until the star falls in.
            // day_cycle drives the real value (birth + all-day growth).
            horizon_r: 0.0,
            gm: pull_home * dist * dist,
            dread: 0.0,
        }
    }

    /// Gravitational acceleration toward the singularity at `p`.
    pub fn pull(&self, p: Vec3) -> Vec3 {
        let to = self.center - p;
        let d2 = to.length_squared().max(10_000.0);
        to.normalize_or_zero() * (self.gm / d2).min(PULL_CAP)
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct BlackHolePlugin;

impl Plugin for BlackHolePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<DayState>()
            .insert_resource(BlackHole::at(0.0))
            .add_systems(Startup, setup_blackhole)
            .add_systems(
                Update,
                (
                    (day_cycle, bh_gravity, day_reset).chain().after(crate::GameplaySet),
                    (place_blackhole, billboard_rings, spin_disc, animate_streaks),
                    (star_cycle, animate_eat_streaks, bh_glow_throb),
                ),
            );
    }
}

// ---------------------------------------------------------------------------
// Visuals
// ---------------------------------------------------------------------------

/// Root entity — moved to BlackHole.center each frame; everything parents here.
#[derive(Component)]
struct BhRoot;

/// Photon ring / halo — rotated to face the player every frame, so the
/// lensing ring wraps the shadow from any approach angle.
#[derive(Component)]
struct BhBillboard;

#[derive(Component)]
struct BhDisc {
    /// Base orientation (disc plane); spin is applied around its local normal.
    base: Quat,
    angle: f32,
    /// Spin multiplier — the two disc layers counter-rotate at different
    /// speeds, so the structure churns instead of turning like a plate.
    rate_mul: f32,
}

/// Relativistic polar jet — scales/pulses with how hard the hole is feeding.
#[derive(Component)]
struct BhJet {
    sign: f32,
}

/// Material handles mutated per frame for the living-glow throb.
#[derive(Resource)]
struct BhGlowMats {
    photon: Handle<StandardMaterial>,
    jet: Handle<StandardMaterial>,
}

/// Infalling debris streak, animated in disc-local space.
#[derive(Component)]
struct BhStreak {
    angle: f32,
    r: f32,
    /// Inward drift rate (m/s) and size scale.
    rate: f32,
    size: f32,
}

/// Shared mesh/material for asteroid-consumption shards (lod.rs spawns them
/// when the advancing horizon swallows a far rock).
#[derive(Resource)]
pub struct EatFx {
    mesh: Handle<Mesh>,
    mat: Handle<StandardMaterial>,
}

/// A doomed rock's last light: a hot shard streaking from where the asteroid
/// was into the horizon, in world space.
#[derive(Component)]
struct EatStreak {
    vel: Vec3,
    ttl: f32,
    ttl_max: f32,
}

/// Spawn the shards for one consumed asteroid.
pub fn spawn_eat_streaks(commands: &mut Commands, fx: &EatFx, from: Vec3, hole: Vec3) {
    let to = hole - from;
    let ttl = 1.6;
    let dir = to / ttl; // arrive at the center as the shard dies
    for k in 0..3u64 {
        let side = Vec3::new(
            hash01(k, from.x.to_bits() as u64) - 0.5,
            hash01(k, from.y.to_bits() as u64) - 0.5,
            hash01(k, from.z.to_bits() as u64) - 0.5,
        ) * 30.0;
        commands.spawn((
            Mesh3d(fx.mesh.clone()),
            MeshMaterial3d(fx.mat.clone()),
            Transform::from_translation(from + side)
                .looking_to(dir.normalize_or_zero(), Vec3::Y)
                .with_scale(Vec3::new(5.0, 5.0, 70.0)),
            EatStreak {
                vel: dir,
                ttl,
                ttl_max: ttl,
            },
        ));
    }
}

fn animate_eat_streaks(
    time: Res<Time>,
    mut streaks: Query<(Entity, &mut EatStreak, &mut Transform)>,
    mut commands: Commands,
) {
    let dt = time.delta_secs();
    for (e, mut s, mut tf) in &mut streaks {
        s.ttl -= dt;
        if s.ttl <= 0.0 {
            commands.entity(e).despawn();
            continue;
        }
        let v = s.vel;
        tf.translation += v * dt;
        let frac = (s.ttl / s.ttl_max).clamp(0.0, 1.0);
        tf.scale = Vec3::new(5.0 * frac, 5.0 * frac, 70.0 + 80.0 * (1.0 - frac));
    }
}

fn hash01(i: u64, salt: u64) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((x >> 40) as f32) / ((1u64 << 24) as f32)
}

/// One accretion-disc layer: log-spaced rings (detail bunched at the hot
/// inner edge), vertex colors carrying temperature (white-hot → ember →
/// violet), doppler beaming (the approaching side burns brighter), and
/// streaky azimuthal structure so the spin reads. `phase`/`streak_f` vary
/// per layer so the counter-rotating pair interferes instead of doubling.
fn disc_mesh(phase: f32, streak_f: f32) -> Mesh {
    const SEG: usize = 180;
    const ROWS: usize = 14;
    let mut positions = Vec::with_capacity((SEG + 1) * (ROWS + 1));
    let mut normals = Vec::with_capacity(positions.capacity());
    let mut colors = Vec::with_capacity(positions.capacity());
    let mut indices = Vec::with_capacity(SEG * ROWS * 6);
    for r in 0..=ROWS {
        let t = r as f32 / ROWS as f32;
        let radius = DISC_IN * (DISC_OUT / DISC_IN).powf(t);
        let hot = (1.0 - t).powf(2.2);
        let base = Vec3::new(
            2.6 + 19.0 * hot,
            0.85 + 11.0 * hot.powf(1.25),
            0.95 + 5.5 * hot.powf(1.9),
        );
        // Gentle outer falloff: the dim violet rim has to survive
        // tonemapping at 5km or the disc reads half its true size.
        let alpha = (1.0 - t).powf(0.8) * 0.8;
        for s in 0..=SEG {
            let a = s as f32 / SEG as f32 * std::f32::consts::TAU;
            positions.push([a.cos() * radius, 0.0, a.sin() * radius]);
            normals.push([0.0, 1.0, 0.0]);
            let doppler = 0.30 + 1.45 * (0.5 + 0.5 * (a + phase).cos()).powf(1.6);
            let streak = 0.55
                + 0.45
                    * ((a * streak_f + phase + t * 25.0).sin() * (a * 3.0 - t * 11.0).sin())
                        .abs();
            let k = doppler * streak;
            colors.push([base.x * k, base.y * k, base.z * k, alpha]);
        }
    }
    let stride = (SEG + 1) as u32;
    for r in 0..ROWS as u32 {
        for s in 0..SEG as u32 {
            let i = r * stride + s;
            indices.extend_from_slice(&[i, i + 1, i + stride, i + 1, i + stride + 1, i + stride]);
        }
    }
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
    .with_inserted_indices(Indices::U32(indices))
}

fn setup_blackhole(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    bh: Res<BlackHole>,
) {
    // Unlit additive no-cull: the recipe for every glowing part of the hole.
    let add_mat = || StandardMaterial {
        base_color: Color::WHITE,
        unlit: true,
        alpha_mode: AlphaMode::Add,
        cull_mode: None,
        ..default()
    };

    let root = commands
        .spawn((Transform::from_translation(bh.center), Visibility::Visible, BhRoot))
        .id();

    // The shadow itself: an unlit pure-black sphere. Opaque, so it occludes
    // the disc, impostors, and starfield behind it — the one thing in this
    // game bloom can't touch.
    let horizon = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(HORIZON_R).mesh().ico(4).unwrap())),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::BLACK,
                unlit: true,
                ..default()
            })),
            Transform::IDENTITY,
        ))
        .id();

    // Photon ring: thin, searing, white-gold — the blazing edge of the
    // shadow. Billboarded; its material throbs (bh_glow_throb).
    let photon_mat = materials.add(add_mat());
    let photon = commands
        .spawn((
            Mesh3d(meshes.add(ring_mesh(HORIZON_R * 1.02, HORIZON_R * 1.20, 160, |t| {
                let b = (std::f32::consts::PI * t).sin().powf(1.2);
                [42.0 * b, 33.0 * b, 21.0 * b, b]
            }))),
            MeshMaterial3d(photon_mat.clone()),
            Transform::IDENTITY,
            BhBillboard,
        ))
        .id();

    // Lensing arc: a second hot annulus just outside the photon ring — the
    // disc's light bent over the poles. With the tilted disc threading
    // through it, the composite is unmistakably *that* black hole image.
    let arc = commands
        .spawn((
            Mesh3d(meshes.add(ring_mesh(HORIZON_R * 1.22, HORIZON_R * 1.85, 128, |t| {
                let b = (1.0 - t).powf(1.6);
                [14.0 * b, 7.5 * b, 3.2 * b, 0.85 * b]
            }))),
            MeshMaterial3d(materials.add(add_mat())),
            Transform::IDENTITY,
            BhBillboard,
        ))
        .id();

    // Outer glow halo: broad, faint, warm. Billboarded.
    let halo = commands
        .spawn((
            Mesh3d(meshes.add(ring_mesh(HORIZON_R * 1.25, HORIZON_R * 3.4, 96, |t| {
                let b = (1.0 - t).powf(2.2);
                [5.0 * b, 2.9 * b, 1.6 * b, 0.55 * b]
            }))),
            MeshMaterial3d(materials.add(add_mat())),
            Transform::IDENTITY,
            BhBillboard,
        ))
        .id();

    // TWO accretion-disc layers, counter-rotating at different speeds with
    // offset streak patterns — the structure churns instead of spinning like
    // a painted plate. Tilted near-edge-on from the home rock's vantage.
    let disc_normal = Vec3::new(0.30, 0.90, 0.20).normalize();
    let disc_base = Quat::from_rotation_arc(Vec3::Y, disc_normal);
    let disc = commands
        .spawn((
            Mesh3d(meshes.add(disc_mesh(0.0, 7.0))),
            MeshMaterial3d(materials.add(add_mat())),
            Transform::from_rotation(disc_base),
            BhDisc {
                base: disc_base,
                angle: 0.0,
                rate_mul: 1.0,
            },
        ))
        .id();
    let disc_b_base = Quat::from_rotation_arc(
        Vec3::Y,
        (disc_normal + Vec3::new(0.05, 0.0, -0.04)).normalize(),
    );
    let disc_b = commands
        .spawn((
            Mesh3d(meshes.add(disc_mesh(2.4, 11.0))),
            MeshMaterial3d(materials.add(add_mat())),
            Transform::from_rotation(disc_b_base).with_scale(Vec3::splat(0.985)),
            BhDisc {
                base: disc_b_base,
                angle: 0.0,
                rate_mul: -0.6,
            },
        ))
        .id();

    // Relativistic polar jets along the disc axis — twin lances of blue-white
    // that lengthen and pulse as the hole feeds harder through the day.
    let jet_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgba(2.6, 4.2, 7.0, 0.40),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        cull_mode: None,
        ..default()
    });
    let jet_mesh = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    let mut jets = Vec::new();
    for sign in [-1.0f32, 1.0] {
        jets.push(
            commands
                .spawn((
                    Mesh3d(jet_mesh.clone()),
                    MeshMaterial3d(jet_mat.clone()),
                    Transform::from_translation(disc_normal * sign * HORIZON_R * 2.8)
                        .looking_to(disc_normal * sign, Vec3::X)
                        .with_scale(Vec3::new(
                            HORIZON_R * 0.30,
                            HORIZON_R * 0.30,
                            HORIZON_R * 5.0,
                        )),
                    BhJet { sign },
                ))
                .id(),
        );
    }
    commands.insert_resource(BhGlowMats {
        photon: photon_mat,
        jet: jet_mat,
    });

    // Infalling debris: streaks spiraling down the disc plane into the
    // horizon. Mix of white-hot plasma and dim rocky chunks — the visible
    // proof that this thing EATS.
    let streak_mesh = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    let hot_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgba(9.0, 5.5, 2.5, 0.8),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        cull_mode: None,
        ..default()
    });
    let rock_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgba(0.9, 0.75, 0.65, 0.65),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        cull_mode: None,
        ..default()
    });
    let mut streaks = Vec::with_capacity(N_STREAKS);
    for i in 0..N_STREAKS as u64 {
        let hot = hash01(i, 0x11) < 0.65;
        let streak = commands
            .spawn((
                Mesh3d(streak_mesh.clone()),
                MeshMaterial3d(if hot { hot_mat.clone() } else { rock_mat.clone() }),
                Transform::IDENTITY,
                BhStreak {
                    angle: hash01(i, 0x22) * std::f32::consts::TAU,
                    r: DISC_IN + hash01(i, 0x33) * (DISC_OUT * 0.85 - DISC_IN),
                    rate: 14.0 + hash01(i, 0x44) * 30.0,
                    size: if hot {
                        2.0 + hash01(i, 0x55) * 2.5
                    } else {
                        4.0 + hash01(i, 0x55) * 7.0
                    },
                },
            ))
            .id();
        streaks.push(streak);
    }

    commands.entity(disc).add_children(&streaks);
    commands
        .entity(root)
        .add_children(&[horizon, photon, arc, halo, disc, disc_b]);
    commands.entity(root).add_children(&jets);

    // Consumption-shard assets for lod.rs.
    commands.insert_resource(EatFx {
        mesh: meshes.add(Cuboid::new(1.0, 1.0, 1.0)),
        mat: materials.add(StandardMaterial {
            base_color: Color::linear_rgba(7.0, 4.0, 1.8, 0.8),
            unlit: true,
            alpha_mode: AlphaMode::Add,
            cull_mode: None,
            ..default()
        }),
    });

    // The doomed star — whole through the dawn, gone by breakfast. Separate
    // from the root so the hole's scale animation doesn't touch it.
    let star_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgb(13.0, 10.0, 5.5),
        unlit: true,
        ..default()
    });
    commands.insert_resource(StarAssets {
        star_mat: star_mat.clone(),
        shock_mesh: meshes.add(Sphere::new(1.0).mesh().ico(3).unwrap()),
        shock_mat: materials.add(StandardMaterial {
            base_color: Color::linear_rgba(3.0, 2.2, 1.5, 0.16),
            unlit: true,
            alpha_mode: AlphaMode::Add,
            cull_mode: None,
            ..default()
        }),
    });
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(STAR_R).mesh().ico(3).unwrap())),
        MeshMaterial3d(star_mat),
        Transform::from_translation(bh.center),
        BhStar,
    ));
}

#[derive(Component)]
struct BhStar;

#[derive(Resource)]
struct StarAssets {
    star_mat: Handle<StandardMaterial>,
    shock_mesh: Handle<Mesh>,
    shock_mat: Handle<StandardMaterial>,
}

/// The supernova's expanding blast shell; `hit` flips when it rolls over the
/// player (boom + shake arrive at lightspeed-ish delay — sound in space is a
/// lie we tell for drama).
#[derive(Component)]
struct ShockShell {
    age: f32,
    origin: Vec3,
    hit: bool,
}

/// Drive the opening spectacle: steady star → swell → detonation (shockwave
/// + flash + SFX) → collapse to nothing as the hole assembly grows in.
#[allow(clippy::too_many_arguments)]
fn star_cycle(
    time: Res<Time>,
    day: Res<DayState>,
    bh: Res<BlackHole>,
    player: Res<PlayerState>,
    assets: Res<StarAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut stars: Query<(&mut Transform, &mut Visibility), With<BhStar>>,
    mut shells: Query<(Entity, &mut ShockShell, &mut Transform), Without<BhStar>>,
    mut sfx: ResMut<SfxQueue>,
    mut shake: ResMut<Shake>,
    mut prev_elapsed: Local<f32>,
    mut commands: Commands,
) {
    let dt = time.delta_secs();

    // Animate + retire shock shells regardless of phase.
    for (e, mut shell, mut tf) in &mut shells {
        shell.age += dt;
        if shell.age > 7.0 {
            commands.entity(e).despawn();
            continue;
        }
        let r = SHOCK_SPEED * shell.age;
        tf.scale = Vec3::splat(r);
        if !shell.hit && r >= shell.origin.distance(player.pos) {
            shell.hit = true;
            sfx.push(SfxEvent::Explosion);
            shake.add(0.55);
        }
    }

    let Ok((mut tf, mut vis)) = stars.single_mut() else {
        return;
    };
    tf.translation = bh.center;

    let t = match day.phase {
        // Through the dawn fade the star just hangs there, pristine.
        DayPhase::Dawn => -1.0,
        _ => day.elapsed,
    };
    let detonated_at = STAR_DETONATE_S;
    let swell_from = detonated_at - STAR_SWELL_S;

    if t >= detonated_at + STAR_COLLAPSE_S {
        *vis = Visibility::Hidden;
        *prev_elapsed = t;
        return;
    }
    *vis = Visibility::Visible;

    let pulse = 1.0 + 0.015 * (time.elapsed_secs() * 2.3).sin();
    let (scale, glow) = if t < swell_from {
        (pulse, 1.0)
    } else if t < detonated_at {
        // Swelling: the last seconds of a star's life.
        let k = ((t - swell_from) / STAR_SWELL_S).clamp(0.0, 1.0);
        (1.0 + 0.5 * k * k, 1.0 + 4.0 * k * k)
    } else {
        // Collapse: everything it was, gone inward in a heartbeat.
        let k = ((t - detonated_at) / STAR_COLLAPSE_S).clamp(0.0, 1.0);
        ((1.5 * (1.0 - k * k)).max(0.01), 6.0 * (1.0 - k) + 1.0)
    };
    tf.scale = Vec3::splat(scale);
    if let Some(mat) = materials.get_mut(&assets.star_mat) {
        mat.base_color = Color::linear_rgb(13.0 * glow, 10.0 * glow, 5.5 * glow);
    }

    // Detonation crossing: one shockwave, one supernova roar, per day.
    if *prev_elapsed < detonated_at && t >= detonated_at {
        sfx.push(SfxEvent::Supernova);
        shake.add(0.2);
        commands.spawn((
            Mesh3d(assets.shock_mesh.clone()),
            MeshMaterial3d(assets.shock_mat.clone()),
            Transform::from_translation(bh.center).with_scale(Vec3::splat(1.0)),
            ShockShell {
                age: 0.0,
                origin: bh.center,
                hit: false,
            },
        ));
    }
    *prev_elapsed = t;
}


/// Track the singularity's position AND size: every visual is a child of the
/// root, meshed at full-size dimensions, so one uniform scale grows the whole
/// assembly from newborn to end-of-day monster.
fn place_blackhole(bh: Res<BlackHole>, mut roots: Query<&mut Transform, With<BhRoot>>) {
    let s = (bh.horizon_r / HORIZON_R).max(0.0001);
    for mut tf in &mut roots {
        tf.translation = bh.center;
        tf.scale = Vec3::splat(s);
    }
}

/// Face the photon ring + halo at the player so the lensing read survives
/// any approach angle (a real ring of bent light is view-independent).
fn billboard_rings(
    player: Res<PlayerState>,
    bh: Res<BlackHole>,
    mut rings: Query<&mut Transform, With<BhBillboard>>,
) {
    let dir = (player.eye() - bh.center).normalize_or_zero();
    if dir == Vec3::ZERO {
        return;
    }
    let rot = Quat::from_rotation_arc(Vec3::Y, dir);
    for mut tf in &mut rings {
        tf.rotation = rot;
    }
}

fn spin_disc(time: Res<Time>, day: Res<DayState>, mut discs: Query<(&mut BhDisc, &mut Transform)>) {
    // The disc churns faster as the day runs out — feeding frenzy.
    let rate = 0.045 + 0.09 * day.frac() * day.frac()
        + if day.phase == DayPhase::Falling { 0.35 } else { 0.0 };
    for (mut disc, mut tf) in &mut discs {
        disc.angle += rate * disc.rate_mul * time.delta_secs();
        tf.rotation = disc.base * Quat::from_rotation_y(disc.angle);
    }
}

/// Jets lengthen and pulse with feeding intensity; the photon ring breathes.
fn bh_glow_throb(
    time: Res<Time>,
    day: Res<DayState>,
    bh: Res<BlackHole>,
    mats: Res<BhGlowMats>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut jets: Query<(&BhJet, &mut Transform)>,
) {
    let t = time.elapsed_secs();
    let feed = 0.45 + 0.85 * day.frac() + bh.dread * 0.4;
    for (jet, mut tf) in &mut jets {
        let pulse = 1.0 + 0.16 * (t * 2.6 + jet.sign).sin();
        let len = HORIZON_R * (3.2 + 3.4 * feed) * pulse;
        tf.scale = Vec3::new(
            HORIZON_R * (0.22 + 0.14 * feed),
            HORIZON_R * (0.22 + 0.14 * feed),
            len,
        );
        tf.translation = tf.rotation * Vec3::NEG_Z * (len * 0.5 + HORIZON_R * 0.6);
    }
    if let Some(m) = materials.get_mut(&mats.photon) {
        let b = 1.0 + 0.16 * (t * 2.1).sin() + 0.25 * bh.dread;
        m.base_color = Color::linear_rgb(b, b, b);
    }
    if let Some(m) = materials.get_mut(&mats.jet) {
        let b = (0.55 + 0.75 * feed) * (1.0 + 0.2 * (t * 3.4).sin());
        m.base_color = Color::linear_rgba(2.6 * b, 4.2 * b, 7.0 * b, 0.40);
    }
}

/// Spiral the debris streaks down the disc (in disc-local space — they're
/// children of the disc entity, so the disc's tilt and spin come free).
fn animate_streaks(
    time: Res<Time>,
    day: Res<DayState>,
    mut streaks: Query<(&mut BhStreak, &mut Transform)>,
) {
    let dt = time.delta_secs();
    let frenzy = 1.0 + 3.0 * day.frac() * day.frac()
        + if day.phase == DayPhase::Falling { 6.0 } else { 0.0 };
    for (mut s, mut tf) in &mut streaks {
        // Kepler-ish: angular speed blows up toward the center.
        let w = 900.0 / s.r.max(60.0).powf(1.20);
        s.angle += w * frenzy * dt;
        s.r -= s.rate * frenzy * dt;
        if s.r < HORIZON_R * 1.05 {
            // Consumed — respawn at the rim with a new lane.
            let i = (s.angle.to_bits() as u64).wrapping_add(s.r.to_bits() as u64);
            s.r = DISC_OUT * (0.78 + 0.2 * hash01(i, 0x77));
            s.angle = hash01(i, 0x88) * std::f32::consts::TAU;
        }
        let (sin, cos) = s.angle.sin_cos();
        let pos = Vec3::new(cos * s.r, 0.0, sin * s.r);
        // Velocity direction: mostly tangential, a little inward.
        let tangent = Vec3::new(-sin, 0.0, cos);
        let inward = -Vec3::new(cos, 0.0, sin);
        let vel_dir = (tangent + inward * 0.22).normalize();
        let len = (s.r * w * frenzy * 0.45).clamp(18.0, 220.0);
        *tf = Transform::from_translation(pos)
            .looking_to(vel_dir, Vec3::Y)
            .with_scale(Vec3::new(s.size, s.size, len));
    }
}

// ---------------------------------------------------------------------------
// The day cycle
// ---------------------------------------------------------------------------

/// Shortest-arc angle ease (same trick as the demo driver — a naive lerp
/// averages the ±π wrap to 180° wrong).
fn ease_angle(current: f32, target: f32, k: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut delta = (target - current) % TAU;
    if delta > PI {
        delta -= TAU;
    } else if delta < -PI {
        delta += TAU;
    }
    current + delta * k.min(1.0)
}

#[allow(clippy::too_many_arguments)]
fn day_cycle(
    time: Res<Time>,
    mut day: ResMut<DayState>,
    mut bh: ResMut<BlackHole>,
    mut player: ResMut<PlayerState>,
    mut shake: ResMut<Shake>,
    mut sfx: ResMut<SfxQueue>,
    mut status: ResMut<StatusMsg>,
    mut projections: Query<&mut Projection, With<Camera3d>>,
    mut warn_stage: Local<u8>,
) {
    let dt = time.delta_secs().min(0.05);
    match day.phase {
        DayPhase::Active => {
            day.elapsed += dt;
            let frac = day.frac();
            let ot = day.overtime();
            if ot <= 0.0 {
                let next = BlackHole::at(frac);
                bh.center = next.center;
                bh.gm = next.gm;
            } else {
                // OVERTIME — no timer, just physics with the gloves off. The
                // hole keeps advancing along its ray at its end-of-day speed
                // (accelerating), sweeping the field and engulfing anything
                // braced against rock; its mass doubles every 45s so a
                // flee-er's pull deficit always closes.
                let v_end = 1.6 * (DIST_START - DIST_END) / day.day_len;
                let dist = DIST_END - v_end * (ot + ot * ot / 80.0);
                bh.center = BH_DIR.normalize() * dist;
                let gm_end = PULL_HOME_END * DIST_END * DIST_END;
                bh.gm = gm_end * 2.0f32.powf(ot / OVERTIME_DOUBLE_S);
            }

            // Threshold warnings — the collapse must never be a surprise.
            let mins = (day.remaining() / 60.0).floor() as i32;
            let secs = (day.remaining() % 60.0) as i32;
            let warn = |s: &mut StatusMsg, q: &mut SfxQueue, msg: String| {
                s.set(msg);
                q.push(SfxEvent::Overheat);
            };
            if ot > 0.0 && *warn_stage < 4 {
                *warn_stage = 4;
                warn(&mut status, &mut sfx, "BORROWED TIME — IT IS COMING FOR THE FIELD".into());
            } else if frac >= 0.92 && *warn_stage < 3 {
                *warn_stage = 3;
                warn(&mut status, &mut sfx, "EVENT HORIZON IMMINENT — GET CLEAR OR GET CONSUMED".into());
            } else if frac >= 0.75 && *warn_stage < 2 {
                *warn_stage = 2;
                warn(&mut status, &mut sfx, format!("The singularity accelerates — {mins}:{secs:02} left"));
            } else if frac >= 0.5 && *warn_stage < 1 {
                *warn_stage = 1;
                warn(&mut status, &mut sfx, format!("Half the day gone — {mins}:{secs:02} until collapse"));
            }

            // The hole is born from the collapsing star at ~half size and
            // grows all day as it feeds (overtime: keeps creeping). Visuals
            // scale with horizon_r via the root transform.
            let birth = ((day.elapsed - STAR_DETONATE_S) / HOLE_GROW_S).clamp(0.0, 1.0);
            let birth = birth * birth * (3.0 - 2.0 * birth);
            let size = (0.5 + 0.7 * frac + ot * 0.0015).min(1.5);
            bh.horizon_r = HORIZON_R * size * birth;

            // Dread: time pressure or proximity, whichever screams louder.
            let pull = bh.pull(player.pos).length();
            bh.dread = (frac * frac * 0.85 + ot / 60.0)
                .max((pull / 22.0).clamp(0.0, 1.0))
                .min(1.0);

            // Supernova flash washes the screen as the star goes.
            let since_boom = day.elapsed - STAR_DETONATE_S;
            day.whiteout = if (0.0..0.8).contains(&since_boom) {
                0.45 * (1.0 - since_boom / 0.8)
            } else {
                0.0
            };

            // The dive triggers PHYSICALLY: grazing the horizon, or being
            // dragged inward with no mathematical way back. A player pinned
            // behind rock triggers neither — the horizon advances onto them.
            let to_hole = bh.center - player.pos;
            let dist = to_hole.length();
            let inward_v = player.vel.dot(to_hole / dist.max(1.0));
            if dist < bh.horizon_r * FALL_TRIGGER
                || (pull > DIVE_PULL && inward_v > DIVE_INWARD_V)
            {
                day.phase = DayPhase::Falling;
                day.phase_t = 0.0;
                sfx.push(SfxEvent::Collapse);
                status.set("CAUGHT — the horizon takes you");
            }
        }
        DayPhase::Falling => {
            day.phase_t += dt;
            bh.dread = 1.0;
            let t = day.phase_t;

            // The dive: pos driven directly (no thrust, no collision — past
            // the point of no return, rock is no obstacle).
            let to = bh.center - player.pos;
            let dist = to.length();
            let dir = to / dist.max(1.0);
            let speed = 60.0 * (1.0 + 0.9 * t) * (1.0 + 0.9 * t);
            player.pos += dir * speed * dt;
            player.vel = dir * speed;

            // Lock the gaze onto what's coming.
            let target_yaw = f32::atan2(-dir.x, -dir.z);
            let target_pitch = dir.y.clamp(-1.0, 1.0).asin();
            let k = (dt * 3.0).min(1.0);
            player.yaw = ease_angle(player.yaw, target_yaw, k);
            player.pitch = ease_angle(player.pitch, target_pitch, k);

            shake.add((0.04 + 0.10 * (t / 3.0).min(1.0)) * dt * 60.0 * 0.08);

            // Tidal FOV stretch.
            for mut proj in &mut projections {
                if let Projection::Perspective(p) = &mut *proj {
                    p.fov = 1.22 + 0.45 * (t / 2.5).min(1.0);
                }
            }

            // White-out by proximity, then hand over to the reset.
            let h = bh.horizon_r;
            day.whiteout = ((2.0 * h - dist) / (1.3 * h)).clamp(0.0, 1.0);
            if dist < h * FALL_IMPACT {
                day.phase = DayPhase::Dawn;
                day.phase_t = 0.0;
                day.whiteout = 1.0;
                day.day += 1;
                day.elapsed = 0.0;
                day.pending_reset = true;
                *warn_stage = 0;
                sfx.push(SfxEvent::Consumed);
            }
        }
        DayPhase::Dawn => {
            day.phase_t += dt;
            let t = day.phase_t;
            day.whiteout = (1.0 - t / DAWN_S).clamp(0.0, 1.0).powf(1.4);
            bh.dread = 0.0;
            // No hole yet — the star hangs whole in the sky until it goes.
            bh.horizon_r = 0.0;
            for mut proj in &mut projections {
                if let Projection::Perspective(p) = &mut *proj {
                    p.fov = 1.22;
                }
            }
            if t >= DAWN_S {
                day.phase = DayPhase::Active;
                day.phase_t = 0.0;
                sfx.push(SfxEvent::Dawn);
            }
        }
    }
}

/// The singularity's pull on the player during normal play. Drops, debris,
/// and charges take theirs in items.rs.
fn bh_gravity(
    time: Res<Time>,
    day: Res<DayState>,
    bh: Res<BlackHole>,
    mut player: ResMut<PlayerState>,
) {
    if day.phase != DayPhase::Active {
        return;
    }
    let dt = time.delta_secs().min(0.05);
    let a = bh.pull(player.pos);
    // Strong enough pull rips you straight off the rock you're standing on.
    if player.grounded && a.length() > RIP_ACCEL {
        player.grounded = false;
        player.vel += Vec3::Y * 0.5;
    }
    if !player.grounded {
        player.vel += a * dt;
    }
}

/// One-frame teardown + rebirth: fresh field, empty pockets, kept upgrades.
#[allow(clippy::too_many_arguments)]
fn day_reset(
    mut day: ResMut<DayState>,
    mut bh: ResMut<BlackHole>,
    mut world: ResMut<VoxelWorld>,
    mut chunk_entities: ResMut<ChunkEntities>,
    mut impostors: ResMut<Impostors>,
    mut player: ResMut<PlayerState>,
    mut wallet: ResMut<Wallet>,
    mut inventory: ResMut<Inventory>,
    mut laser: ResMut<LaserState>,
    mut shake: ResMut<Shake>,
    mut status: ResMut<StatusMsg>,
    loose: Query<Entity, Or<(With<DropItem>, With<PlasmaCharge>, With<Debris>)>>,
    mut commands: Commands,
) {
    if !day.pending_reset {
        return;
    }
    day.pending_reset = false;

    // The hole retreats to its dawn position immediately — it must already
    // be far away when the white fade thins, not snap there afterward.
    *bh = BlackHole::at(0.0);

    // A new field for a new day (home rock is salt-independent).
    world.reset_for_new_day((day.day - 1) as u64);
    chunk_entities.despawn_all(&mut commands);
    impostors.despawn_all(&mut commands);
    for e in &loose {
        commands.entity(e).despawn();
    }

    // What the hole keeps: your credits, your cargo, your map of the field.
    wallet.credits = 0;
    inventory.clear();

    let spawn = PlayerState::spawn_point();
    player.pos = spawn;
    player.vel = Vec3::ZERO;
    player.yaw = 0.0;
    player.pitch = -0.1;
    player.grounded = false;
    player.max_range = 0.0;
    player.far_pos = spawn;

    laser.heat = 0.0;
    laser.locked = false;
    laser.firing = false;
    shake.trauma = 0.0;

    status.set(format!("DAY {} — the field is reborn. Upgrades retained.", day.day));
}
