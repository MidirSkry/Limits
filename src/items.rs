//! Physical items in the world: loot drops you tractor in by proximity, debris
//! chips and laser sparks (pure visual juice), and plasma charges.
//!
//! Perf note: explosions DO NOT spawn one drop per voxel (a 4m blast carves
//! ~17k voxels — that was 17k entities and single-digit FPS). Loot from a
//! blast is aggregated per (block, band) into a handful of stacked drops,
//! each carrying a `count`.

use bevy::prelude::*;
use std::collections::HashMap;

use crate::audio::{SfxEvent, SfxQueue};
use crate::game::{Inventory, StatusMsg, Upgrades};
use crate::player::{PlayerState, Shake};
use crate::world::{loot_color, tier_of_voxel, VoxelWorld, AIR, BARRIER, VOXEL};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Zero-G: loose items drift, they don't fall. Damping settles a blast's
/// debris cloud into slow tumble instead of letting it disperse forever.
const DRIFT_DAMP: f32 = 0.55;
const DROP_SIZE: f32 = 0.1;
/// Drops vanish after this long so an unlooted crater doesn't leak entities.
const DROP_TTL: f32 = 90.0;
const COLLECT_RANGE: f32 = 0.45;

const PLASMA_FUSE: f32 = 2.0;
const PLASMA_RADIUS_M: f32 = 4.0;
/// Visual chips per explosion (stacked drops carry the "stuff flew out" read;
/// this is just extra spark).
const BLAST_DEBRIS: usize = 160;
const BLAST_SPARKS: usize = 70;
/// Max stacked drops per loot group in one blast.
const BLAST_DROPS_PER_GROUP: u32 = 14;

const DEBRIS_TTL: f32 = 0.9;

// ---------------------------------------------------------------------------
// Components + assets
// ---------------------------------------------------------------------------

#[derive(Component)]
pub struct DropItem {
    id: u8,
    band: i32,
    /// How many units of loot this drop is worth (blast loot is stacked).
    count: u32,
    vel: Vec3,
    ttl: f32,
}

#[derive(Component)]
pub struct PlasmaCharge {
    vel: Vec3,
    fuse: f32,
    material: Handle<StandardMaterial>,
}

#[derive(Component)]
pub struct Debris {
    vel: Vec3,
    ttl: f32,
    ttl_max: f32,
    /// Sparks float (low gravity multiplier); rock chips drop.
    grav: f32,
}

/// Short-lived explosion light.
#[derive(Component)]
struct Flash {
    age: f32,
    ttl: f32,
    peak: f32,
}

/// Expanding ground ring on detonation.
#[derive(Component)]
struct Shockwave {
    age: f32,
    radius: f32,
    material: Handle<StandardMaterial>,
}

/// Recent-pickup streak — pitches the pickup blip up while you hoover loot.
#[derive(Resource, Default)]
pub struct PickupCombo(f32);

#[derive(Resource)]
pub struct ItemAssets {
    drop_mesh: Handle<Mesh>,
    debris_mesh: Handle<Mesh>,
    debris_material: Handle<StandardMaterial>,
    spark_mesh: Handle<Mesh>,
    spark_hot: Handle<StandardMaterial>,
    spark_ember: Handle<StandardMaterial>,
    charge_mesh: Handle<Mesh>,
    ring_mesh: Handle<Mesh>,
    /// One material per loot kind+band, created on first drop of that kind.
    loot_materials: HashMap<(u8, i32), Handle<StandardMaterial>>,
}

impl ItemAssets {
    fn loot_material(
        &mut self,
        materials: &mut Assets<StandardMaterial>,
        id: u8,
        band: i32,
    ) -> Handle<StandardMaterial> {
        self.loot_materials
            .entry((id, band))
            .or_insert_with(|| {
                let c = loot_color(id, band);
                materials.add(StandardMaterial {
                    base_color: Color::linear_rgb(c[0], c[1], c[2]),
                    // Slight glow so loot reads in a headlamp-lit mine.
                    emissive: LinearRgba::rgb(c[0] * 0.6, c[1] * 0.6, c[2] * 0.6),
                    perceptual_roughness: 0.8,
                    ..default()
                })
            })
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct ItemsPlugin;

impl Plugin for ItemsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PickupCombo>()
            .add_systems(Startup, setup_item_assets)
            .add_systems(
                Update,
                (
                    charge_place.in_set(crate::GameplaySet),
                    drop_physics,
                    drop_pickup,
                    charge_tick,
                    update_debris,
                    update_flash,
                    update_shockwave,
                ),
            );
    }
}

fn setup_item_assets(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.insert_resource(ItemAssets {
        drop_mesh: meshes.add(Cuboid::new(DROP_SIZE, DROP_SIZE, DROP_SIZE)),
        debris_mesh: meshes.add(Cuboid::new(0.07, 0.07, 0.07)),
        debris_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.45, 0.42, 0.38),
            perceptual_roughness: 1.0,
            ..default()
        }),
        spark_mesh: meshes.add(Cuboid::new(0.035, 0.035, 0.035)),
        spark_hot: materials.add(StandardMaterial {
            base_color: Color::linear_rgb(2.0, 7.0, 8.0),
            unlit: true,
            ..default()
        }),
        spark_ember: materials.add(StandardMaterial {
            base_color: Color::linear_rgb(8.0, 3.0, 0.6),
            unlit: true,
            ..default()
        }),
        charge_mesh: meshes.add(Sphere::new(0.16)),
        ring_mesh: meshes.add(crate::sky::ring_mesh(0.82, 1.0, 64, |_| {
            [1.0, 1.0, 1.0, 1.0]
        })),
        loot_materials: HashMap::new(),
    });
}

// ---------------------------------------------------------------------------
// Spawning (called from mining, the laser, and explosions)
// ---------------------------------------------------------------------------

/// Deterministic [0,1) scatter from a voxel coordinate — no RNG state.
fn scatter(v: IVec3, salt: i32) -> f32 {
    let h = v.x.wrapping_mul(374_761_393)
        ^ v.y.wrapping_mul(668_265_263)
        ^ v.z.wrapping_mul(2_147_483_647 / 31)
        ^ salt.wrapping_mul(97_429);
    ((h as u32) >> 8) as f32 / (1u32 << 24) as f32
}

/// One stacked drop at a world position. Bigger stacks render bigger.
#[allow(clippy::too_many_arguments)]
fn spawn_drop_at(
    commands: &mut Commands,
    assets: &mut ItemAssets,
    materials: &mut Assets<StandardMaterial>,
    pos: Vec3,
    id: u8,
    band: i32,
    count: u32,
    vel: Vec3,
) {
    let material = assets.loot_material(materials, id, band);
    let scale = 1.0 + (count as f32).ln().max(0.0) * 0.35;
    commands.spawn((
        Mesh3d(assets.drop_mesh.clone()),
        MeshMaterial3d(material),
        Transform::from_translation(pos).with_scale(Vec3::splat(scale.min(3.0))),
        DropItem {
            id,
            band,
            count,
            vel,
            ttl: DROP_TTL,
        },
    ));
}

/// The mining-path drop: one unit pops out of a destroyed voxel.
pub fn spawn_drop(
    commands: &mut Commands,
    assets: &mut ItemAssets,
    materials: &mut Assets<StandardMaterial>,
    v: IVec3,
    id: u8,
) {
    let tier = tier_of_voxel(v);
    let center = (v.as_vec3() + Vec3::splat(0.5)) * VOXEL;
    let a = scatter(v, 1) * std::f32::consts::TAU;
    // Gentle pop: in zero-G a fast drop coasts straight out of tractor range
    // and is simply gone.
    let vel = Vec3::new(a.cos() * 0.3, 0.25 + scatter(v, 2) * 0.3, a.sin() * 0.3);
    spawn_drop_at(commands, assets, materials, center, id, tier, 1, vel);
}

pub fn spawn_block_debris(commands: &mut Commands, assets: &ItemAssets, v: IVec3) {
    let center = (v.as_vec3() + Vec3::splat(0.5)) * VOXEL;
    for i in 0..3 {
        let a = scatter(v, 10 + i) * std::f32::consts::TAU;
        let vel = Vec3::new(a.cos() * 1.2, 1.6 + scatter(v, 20 + i), a.sin() * 1.2);
        commands.spawn((
            Mesh3d(assets.debris_mesh.clone()),
            MeshMaterial3d(assets.debris_material.clone()),
            Transform::from_translation(center),
            Debris {
                vel,
                ttl: DEBRIS_TTL,
                ttl_max: DEBRIS_TTL,
                grav: 1.0,
            },
        ));
    }
}

/// Low-gravity dust kicked up by a hard landing.
pub fn spawn_land_dust(commands: &mut Commands, assets: &ItemAssets, feet: Vec3) {
    let sv = (feet / VOXEL).as_ivec3();
    for i in 0..6 {
        let a = scatter(sv, 500 + i) * std::f32::consts::TAU;
        let speed = 0.8 + scatter(sv, 600 + i) * 1.2;
        commands.spawn((
            Mesh3d(assets.debris_mesh.clone()),
            MeshMaterial3d(assets.debris_material.clone()),
            Transform::from_translation(feet + Vec3::Y * 0.05)
                .with_scale(Vec3::splat(0.7)),
            Debris {
                vel: Vec3::new(a.cos() * speed, 0.6, a.sin() * speed),
                ttl: 0.7,
                ttl_max: 0.7,
                grav: 0.25,
            },
        ));
    }
}

/// Glowing sparks — used at the laser impact point and in explosions.
pub fn spawn_sparks(
    commands: &mut Commands,
    assets: &ItemAssets,
    pos: Vec3,
    n: usize,
    hot: bool,
    seed: i32,
) {
    let sv = (pos / VOXEL).as_ivec3();
    let mat = if hot { &assets.spark_hot } else { &assets.spark_ember };
    for i in 0..n {
        let s = seed.wrapping_add(i as i32 * 31);
        let a = scatter(sv, s) * std::f32::consts::TAU;
        let up = scatter(sv, s + 1) * 2.2;
        let speed = 1.0 + scatter(sv, s + 2) * 2.5;
        let ttl = 0.25 + scatter(sv, s + 3) * 0.3;
        commands.spawn((
            Mesh3d(assets.spark_mesh.clone()),
            MeshMaterial3d(mat.clone()),
            Transform::from_translation(pos),
            Debris {
                vel: Vec3::new(a.cos() * speed, up, a.sin() * speed),
                ttl,
                ttl_max: ttl,
                grav: 0.35,
            },
        ));
    }
}

// ---------------------------------------------------------------------------
// Drops: ballistic fall with per-axis voxel stop, then tractor + collect.
// ---------------------------------------------------------------------------

fn drop_physics(
    time: Res<Time>,
    world: Res<VoxelWorld>,
    bh: Res<crate::blackhole::BlackHole>,
    mut drops: Query<(Entity, &mut DropItem, &mut Transform)>,
    mut commands: Commands,
) {
    let dt = time.delta_secs().min(0.05);
    for (entity, mut drop, mut tf) in &mut drops {
        drop.ttl -= dt;
        if drop.ttl <= 0.0 {
            commands.entity(entity).despawn();
            continue;
        }
        // Zero-G drift with damping, so loot hangs near where it popped.
        let v = drop.vel;
        drop.vel = v * (-DRIFT_DAMP * 1.8 * dt).exp();
        // The singularity claims unattended loot. A floor on the pull keeps a
        // faint, ominous drift toward it even early in the day.
        let pull = bh.pull(tf.translation);
        let pull = pull.normalize_or_zero() * pull.length().max(0.25) * 0.6;
        drop.vel += pull * dt;
        // Per-axis point collision: stop the axis instead of entering a wall.
        let mut p = tf.translation;
        for axis in 0..3 {
            let mut np = p;
            np[axis] += drop.vel[axis] * dt;
            let probe = (np / VOXEL).floor().as_ivec3();
            if world.solid(probe) {
                drop.vel[axis] = if axis == 1 { 0.0 } else { -drop.vel[axis] * 0.3 };
                // Ground friction so drops settle instead of sliding forever.
                if axis == 1 {
                    drop.vel.x *= 0.7;
                    drop.vel.z *= 0.7;
                }
            } else {
                p = np;
            }
        }
        tf.translation = p;
        tf.rotation = Quat::from_rotation_y(drop.ttl * 2.0);
    }
}

fn drop_pickup(
    time: Res<Time>,
    player: Res<PlayerState>,
    upgrades: Res<Upgrades>,
    mut combo: ResMut<PickupCombo>,
    mut inventory: ResMut<Inventory>,
    mut sfx: ResMut<SfxQueue>,
    mut drops: Query<(Entity, &mut DropItem, &mut Transform)>,
    mut commands: Commands,
) {
    let dt = time.delta_secs().min(0.05);
    combo.0 = (combo.0 - dt * 2.0).max(0.0);
    let target = player.pos + Vec3::Y * 0.7;
    let magnet_range = upgrades.magnet_range();
    let magnet_accel = upgrades.magnet_accel();
    for (entity, mut drop, mut tf) in &mut drops {
        let to_player = target - tf.translation;
        let dist = to_player.length();
        if dist > magnet_range {
            continue;
        }
        if dist <= COLLECT_RANGE {
            inventory.add_n(drop.id, drop.band, drop.count);
            combo.0 = (combo.0 + 1.0).min(16.0);
            sfx.push(SfxEvent::Pickup {
                pitch: 1.0 + combo.0 * 0.035,
            });
            commands.entity(entity).despawn();
            continue;
        }
        // Tractor glide, strongest up close.
        let pull = to_player / dist * magnet_accel * (1.0 - dist / magnet_range + 0.3);
        drop.vel += pull * dt;
        let v = drop.vel;
        tf.translation += v * dt;
    }
}

// ---------------------------------------------------------------------------
// Plasma charges
// ---------------------------------------------------------------------------

fn charge_place(
    keys: Res<ButtonInput<KeyCode>>,
    focused: Res<crate::player::Focused>,
    player: Res<PlayerState>,
    assets: Res<ItemAssets>,
    mut upgrades: ResMut<Upgrades>,
    mut status: ResMut<StatusMsg>,
    mut sfx: ResMut<SfxQueue>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut commands: Commands,
) {
    if !focused.0 || !keys.just_pressed(KeyCode::KeyQ) {
        return;
    }
    if upgrades.charges == 0 {
        status.set("No plasma charges — buy more at the depot [5]");
        sfx.push(SfxEvent::Deny);
        return;
    }
    upgrades.charges -= 1;
    sfx.push(SfxEvent::Plant);
    let dir = player.look_dir();
    // Per-charge material so the fuse can ramp its glow without touching
    // other charges. Charges are rare spawns; a material each is fine.
    let material = materials.add(StandardMaterial {
        base_color: Color::linear_rgb(1.5, 0.4, 3.0),
        unlit: true,
        ..default()
    });
    commands.spawn((
        Mesh3d(assets.charge_mesh.clone()),
        MeshMaterial3d(material.clone()),
        Transform::from_translation(player.eye() + dir * 0.6),
        PlasmaCharge {
            vel: dir * 5.0 + Vec3::Y * 2.0,
            fuse: PLASMA_FUSE,
            material,
        },
    ));
}

#[allow(clippy::too_many_arguments)]
fn charge_tick(
    time: Res<Time>,
    bh: Res<crate::blackhole::BlackHole>,
    mut world: ResMut<VoxelWorld>,
    mut charges: Query<(Entity, &mut PlasmaCharge, &mut Transform)>,
    mut assets: ResMut<ItemAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut status: ResMut<StatusMsg>,
    mut sfx: ResMut<SfxQueue>,
    mut shake: ResMut<Shake>,
    player: Res<PlayerState>,
    mut commands: Commands,
) {
    let dt = time.delta_secs().min(0.05);
    for (entity, mut charge, mut tf) in &mut charges {
        charge.fuse -= dt;
        if charge.fuse <= 0.0 {
            let center = tf.translation;
            commands.entity(entity).despawn();
            explode(
                &mut world,
                &mut assets,
                &mut materials,
                &mut status,
                &mut sfx,
                &mut shake,
                &player,
                &mut commands,
                center,
            );
            continue;
        }
        // Plasma torpedo: sails in zero-G until it sticks — its arc bends
        // toward the singularity like everything else.
        charge.vel += bh.pull(tf.translation) * dt;
        let mut p = tf.translation;
        for axis in 0..3 {
            let mut np = p;
            np[axis] += charge.vel[axis] * dt;
            if world.solid((np / VOXEL).floor().as_ivec3()) {
                charge.vel[axis] = 0.0;
                if axis == 1 {
                    charge.vel.x *= 0.6;
                    charge.vel.z *= 0.6;
                }
            } else {
                p = np;
            }
        }
        tf.translation = p;
        // Fuse tell: pulse faster and burn whiter as detonation approaches.
        let urgency = 1.0 - charge.fuse / PLASMA_FUSE;
        let pulse = 1.0 + 0.18 * (charge.fuse * (10.0 + urgency * 26.0)).sin();
        tf.scale = Vec3::splat(pulse);
        if let Some(mat) = materials.get_mut(&charge.material) {
            let w = 1.0 + urgency * 6.0;
            mat.base_color = Color::linear_rgb(1.5 * w, 0.4 + urgency * 3.0, 3.0 * w);
        }
    }
}

/// Carve a sphere, aggregate the loot into stacked drops, and dress the blast:
/// flash light, ground shockwave ring, debris, sparks, screen shake.
#[allow(clippy::too_many_arguments)]
fn explode(
    world: &mut VoxelWorld,
    assets: &mut ItemAssets,
    materials: &mut Assets<StandardMaterial>,
    status: &mut StatusMsg,
    sfx: &mut SfxQueue,
    shake: &mut Shake,
    player: &PlayerState,
    commands: &mut Commands,
    center: Vec3,
) {
    let r_vox = (PLASMA_RADIUS_M / VOXEL).ceil() as i32;
    let c_vox = (center / VOXEL).floor().as_ivec3();
    // One blast = one asteroid (in practice): a single tier lookup covers
    // every carved voxel instead of 12k field queries.
    let blast_tier = tier_of_voxel(c_vox);
    let mut carved = 0usize;
    // Loot aggregation: (id, tier) -> voxels destroyed.
    let mut loot: HashMap<(u8, i32), u32> = HashMap::new();
    for dy in -r_vox..=r_vox {
        for dz in -r_vox..=r_vox {
            for dx in -r_vox..=r_vox {
                let v = c_vox + IVec3::new(dx, dy, dz);
                let p = (v.as_vec3() + Vec3::splat(0.5)) * VOXEL;
                if p.distance(center) > PLASMA_RADIUS_M {
                    continue;
                }
                let id = world.block(v);
                if id == AIR || id == BARRIER {
                    continue;
                }
                world.set_air(v);
                *loot.entry((id, blast_tier)).or_insert(0) += 1;
                carved += 1;
            }
        }
    }

    // Stacked drops: a handful per loot group, fanned out over the crater.
    for (gi, (&(id, band), &total)) in loot.iter().enumerate() {
        let stacks = total.min(BLAST_DROPS_PER_GROUP);
        let per = total / stacks.max(1);
        let mut rem = total - per * stacks;
        for s in 0..stacks {
            let mut count = per;
            if rem > 0 {
                count += 1;
                rem -= 1;
            }
            let salt = (gi as i32) * 1009 + s as i32;
            let a = scatter(c_vox, salt) * std::f32::consts::TAU;
            let r = scatter(c_vox, salt + 7) * PLASMA_RADIUS_M * 0.55;
            let pos = center + Vec3::new(a.cos() * r, 0.4, a.sin() * r);
            let vel = Vec3::new(
                a.cos() * (1.0 + r),
                2.5 + scatter(c_vox, salt + 13) * 2.0,
                a.sin() * (1.0 + r),
            );
            spawn_drop_at(commands, assets, materials, pos, id, band, count, vel);
        }
    }

    // Radial chip burst + hot sparks for the shockwave read.
    for i in 0..BLAST_DEBRIS {
        let u = scatter(c_vox, i as i32) * std::f32::consts::TAU;
        let w = scatter(c_vox, 1000 + i as i32) * 2.0 - 1.0;
        let r = (1.0 - w * w).max(0.0).sqrt();
        let dir = Vec3::new(r * u.cos(), w.abs() * 0.8 + 0.2, r * u.sin());
        let speed = 5.0 + scatter(c_vox, 2000 + i as i32) * 9.0;
        let ttl = 0.8 + scatter(c_vox, 3000 + i as i32) * 0.5;
        commands.spawn((
            Mesh3d(assets.debris_mesh.clone()),
            MeshMaterial3d(assets.debris_material.clone()),
            Transform::from_translation(center),
            Debris {
                vel: dir * speed,
                ttl,
                ttl_max: ttl,
                grav: 1.0,
            },
        ));
    }
    spawn_sparks(commands, assets, center, BLAST_SPARKS, true, 4242);

    // Flash light.
    commands.spawn((
        PointLight {
            color: Color::srgb(0.7, 0.9, 1.0),
            intensity: 0.0,
            range: 30.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_translation(center + Vec3::Y * 0.5),
        Flash {
            age: 0.0,
            ttl: 0.45,
            peak: 12_000_000.0,
        },
    ));

    // Ground shockwave ring (unique material so alpha can fade per-instance).
    let ring_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgba(2.0, 5.0, 6.0, 0.8),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        cull_mode: None,
        ..default()
    });
    commands.spawn((
        Mesh3d(assets.ring_mesh.clone()),
        MeshMaterial3d(ring_mat.clone()),
        Transform::from_translation(center).with_scale(Vec3::splat(0.5)),
        Shockwave {
            age: 0.0,
            radius: PLASMA_RADIUS_M * 2.6,
            material: ring_mat,
        },
    ));

    // Feel: shake scaled by proximity, capped so a point-blank blast is loud
    // but not nauseating.
    let d = player.pos.distance(center);
    shake.add(0.65 * (1.0 - d / 24.0).clamp(0.0, 1.0));
    sfx.push(SfxEvent::Explosion);
    status.set(format!("BOOM — {carved} blocks"));
}

fn update_flash(
    time: Res<Time>,
    mut flashes: Query<(Entity, &mut Flash, &mut PointLight)>,
    mut commands: Commands,
) {
    let dt = time.delta_secs();
    for (entity, mut flash, mut light) in &mut flashes {
        flash.age += dt;
        let t = flash.age / flash.ttl;
        if t >= 1.0 {
            commands.entity(entity).despawn();
            continue;
        }
        // Sharp attack, quadratic falloff.
        let env = if t < 0.08 { t / 0.08 } else { (1.0 - t).powi(2) };
        light.intensity = flash.peak * env;
    }
}

fn update_shockwave(
    time: Res<Time>,
    mut waves: Query<(Entity, &mut Shockwave, &mut Transform)>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut commands: Commands,
) {
    let dt = time.delta_secs();
    for (entity, mut wave, mut tf) in &mut waves {
        wave.age += dt;
        let t = wave.age / 0.55;
        if t >= 1.0 {
            commands.entity(entity).despawn();
            continue;
        }
        let eased = 1.0 - (1.0 - t) * (1.0 - t);
        tf.scale = Vec3::splat(0.5 + eased * wave.radius);
        if let Some(mat) = materials.get_mut(&wave.material) {
            mat.base_color = Color::linear_rgba(2.0, 5.0, 6.0, 0.8 * (1.0 - t));
        }
    }
}

// ---------------------------------------------------------------------------
// Debris chips + sparks — fall, tumble, vanish. Pure visual.
// ---------------------------------------------------------------------------

fn update_debris(
    time: Res<Time>,
    mut debris: Query<(Entity, &mut Debris, &mut Transform)>,
    mut commands: Commands,
) {
    let dt = time.delta_secs();
    for (entity, mut d, mut tf) in &mut debris {
        d.ttl -= dt;
        if d.ttl <= 0.0 {
            commands.entity(entity).despawn();
            continue;
        }
        // Drift + damp; `grav` doubles as how quickly this debris settles.
        let damp = DRIFT_DAMP * (0.5 + d.grav);
        let v = d.vel;
        d.vel = v * (-damp * dt).exp();
        tf.translation += d.vel * dt;
        tf.rotation *= Quat::from_rotation_x(6.0 * dt) * Quat::from_rotation_y(4.0 * dt);
        // Sparks shrink away instead of popping out.
        let frac = (d.ttl / d.ttl_max).clamp(0.0, 1.0);
        tf.scale = Vec3::splat(0.4 + 0.6 * frac);
    }
}
