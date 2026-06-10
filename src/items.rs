//! Physical items in the world: loot drops you pick up by touch, debris
//! chips (pure visual juice), and TNT charges.
//!
//! Test-feature notes: every destroyed voxel drops loot (cheap stuff is $1),
//! and TNT is free to place while we evaluate the feel. Both are deliberately
//! unclamped so playtesting can find the real limits.

use bevy::prelude::*;
use std::collections::HashMap;

use crate::game::{Inventory, StatusMsg, Upgrades};
use crate::player::PlayerState;
use crate::world::{
    band_of_depth, depth_of, loot_color, VoxelWorld, AIR, BARRIER, VOXEL,
};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

const GRAVITY: f32 = -22.0;
const DROP_SIZE: f32 = 0.1;
/// Drops vanish after this long so a TNT crater doesn't leak entities forever.
const DROP_TTL: f32 = 120.0;
/// Within this range (and with pack space) drops magnet toward the player.
const MAGNET_RANGE: f32 = 1.6;
const MAGNET_ACCEL: f32 = 40.0;
const COLLECT_RANGE: f32 = 0.45;

const TNT_FUSE: f32 = 2.0;
const TNT_RADIUS_M: f32 = 4.0;
/// Visual chips per explosion (drops carry the "stuff flew out" read; this is
/// just extra spark).
const TNT_DEBRIS: usize = 250;

const DEBRIS_TTL: f32 = 0.7;

// ---------------------------------------------------------------------------
// Components + assets
// ---------------------------------------------------------------------------

#[derive(Component)]
pub struct DropItem {
    id: u8,
    band: i32,
    vel: Vec3,
    ttl: f32,
}

#[derive(Component)]
pub struct Tnt {
    vel: Vec3,
    fuse: f32,
}

#[derive(Component)]
pub struct Debris {
    vel: Vec3,
    ttl: f32,
}

#[derive(Resource)]
pub struct ItemAssets {
    drop_mesh: Handle<Mesh>,
    debris_mesh: Handle<Mesh>,
    debris_material: Handle<StandardMaterial>,
    tnt_mesh: Handle<Mesh>,
    tnt_material: Handle<StandardMaterial>,
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
                    emissive: LinearRgba::rgb(c[0] * 0.4, c[1] * 0.4, c[2] * 0.4),
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
        app.add_systems(Startup, setup_item_assets).add_systems(
            Update,
            (drop_physics, drop_pickup, tnt_place, tnt_tick, update_debris),
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
        debris_mesh: meshes.add(Cuboid::new(0.08, 0.08, 0.08)),
        debris_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.5, 0.45, 0.4),
            perceptual_roughness: 1.0,
            ..default()
        }),
        tnt_mesh: meshes.add(Cuboid::new(0.3, 0.3, 0.3)),
        tnt_material: materials.add(StandardMaterial {
            base_color: Color::srgb(0.85, 0.1, 0.05),
            emissive: LinearRgba::rgb(1.2, 0.1, 0.05),
            ..default()
        }),
        loot_materials: HashMap::new(),
    });
}

// ---------------------------------------------------------------------------
// Spawning (called from mining and TNT)
// ---------------------------------------------------------------------------

/// Deterministic [0,1) scatter from a voxel coordinate — no RNG state.
fn scatter(v: IVec3, salt: i32) -> f32 {
    let h = v.x.wrapping_mul(374_761_393)
        ^ v.y.wrapping_mul(668_265_263)
        ^ v.z.wrapping_mul(2_147_483_647 / 31)
        ^ salt.wrapping_mul(97_429);
    ((h as u32) >> 8) as f32 / (1u32 << 24) as f32
}

pub fn spawn_drop(
    commands: &mut Commands,
    assets: &mut ItemAssets,
    materials: &mut Assets<StandardMaterial>,
    v: IVec3,
    id: u8,
) {
    let band = band_of_depth(depth_of(v));
    let material = assets.loot_material(materials, id, band);
    let center = (v.as_vec3() + Vec3::splat(0.5)) * VOXEL;
    let a = scatter(v, 1) * std::f32::consts::TAU;
    let vel = Vec3::new(a.cos() * 0.8, 1.5 + scatter(v, 2), a.sin() * 0.8);
    commands.spawn((
        Mesh3d(assets.drop_mesh.clone()),
        MeshMaterial3d(material),
        Transform::from_translation(center),
        DropItem {
            id,
            band,
            vel,
            ttl: DROP_TTL,
        },
    ));
}

pub fn spawn_block_debris(commands: &mut Commands, assets: &ItemAssets, v: IVec3) {
    let center = (v.as_vec3() + Vec3::splat(0.5)) * VOXEL;
    for i in 0..3 {
        let a = scatter(v, 10 + i) * std::f32::consts::TAU;
        let vel = Vec3::new(a.cos() * 1.5, 2.0 + scatter(v, 20 + i), a.sin() * 1.5);
        commands.spawn((
            Mesh3d(assets.debris_mesh.clone()),
            MeshMaterial3d(assets.debris_material.clone()),
            Transform::from_translation(center),
            Debris {
                vel,
                ttl: DEBRIS_TTL,
            },
        ));
    }
}

// ---------------------------------------------------------------------------
// Drops: ballistic fall with per-axis voxel stop, then magnet + collect.
// ---------------------------------------------------------------------------

fn drop_physics(
    time: Res<Time>,
    world: Res<VoxelWorld>,
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
        drop.vel.y += GRAVITY * dt;
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
    mut inventory: ResMut<Inventory>,
    mut status: ResMut<StatusMsg>,
    mut drops: Query<(Entity, &mut DropItem, &mut Transform)>,
    mut commands: Commands,
) {
    let dt = time.delta_secs().min(0.05);
    let target = player.pos + Vec3::Y * 0.7;
    let full = inventory.units >= upgrades.capacity();
    for (entity, mut drop, mut tf) in &mut drops {
        let to_player = target - tf.translation;
        let dist = to_player.length();
        if dist > MAGNET_RANGE {
            continue;
        }
        if full {
            status.set("Pack full! Sell at the surface shop [E]");
            continue;
        }
        if dist <= COLLECT_RANGE {
            inventory.add(drop.id, drop.band);
            commands.entity(entity).despawn();
            continue;
        }
        // Magnet glide, strongest up close.
        let pull = to_player / dist * MAGNET_ACCEL * (1.0 - dist / MAGNET_RANGE + 0.3);
        drop.vel += pull * dt;
        let v = drop.vel;
        tf.translation += v * dt;
    }
}

// ---------------------------------------------------------------------------
// TNT
// ---------------------------------------------------------------------------

fn tnt_place(
    keys: Res<ButtonInput<KeyCode>>,
    focused: Res<crate::player::Focused>,
    player: Res<PlayerState>,
    assets: Res<ItemAssets>,
    mut commands: Commands,
) {
    if !focused.0 || !keys.just_pressed(KeyCode::KeyQ) {
        return;
    }
    let dir = player.look_dir();
    commands.spawn((
        Mesh3d(assets.tnt_mesh.clone()),
        MeshMaterial3d(assets.tnt_material.clone()),
        Transform::from_translation(player.eye() + dir * 0.6),
        Tnt {
            vel: dir * 5.0 + Vec3::Y * 2.0,
            fuse: TNT_FUSE,
        },
    ));
}

fn tnt_tick(
    time: Res<Time>,
    mut world: ResMut<VoxelWorld>,
    mut tnts: Query<(Entity, &mut Tnt, &mut Transform)>,
    mut assets: ResMut<ItemAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut status: ResMut<StatusMsg>,
    mut commands: Commands,
) {
    let dt = time.delta_secs().min(0.05);
    for (entity, mut tnt, mut tf) in &mut tnts {
        tnt.fuse -= dt;
        if tnt.fuse <= 0.0 {
            let center = tf.translation;
            commands.entity(entity).despawn();
            explode(
                &mut world,
                &mut assets,
                &mut materials,
                &mut status,
                &mut commands,
                center,
            );
            continue;
        }
        // Ballistic fall with the same per-axis stop the drops use.
        tnt.vel.y += GRAVITY * dt;
        let mut p = tf.translation;
        for axis in 0..3 {
            let mut np = p;
            np[axis] += tnt.vel[axis] * dt;
            if world.solid((np / VOXEL).floor().as_ivec3()) {
                tnt.vel[axis] = 0.0;
                if axis == 1 {
                    tnt.vel.x *= 0.6;
                    tnt.vel.z *= 0.6;
                }
            } else {
                p = np;
            }
        }
        tf.translation = p;
        // Fuse tell: pulse faster as detonation approaches.
        let pulse = 1.0 + 0.15 * (tnt.fuse * (8.0 + (TNT_FUSE - tnt.fuse) * 12.0)).sin();
        tf.scale = Vec3::splat(pulse);
    }
}

/// Carve a sphere, drop loot for every voxel destroyed, and throw chips.
/// Flat "destroys anything but bedrock" for now — depth-scaled TNT power is a
/// balance decision for later.
fn explode(
    world: &mut VoxelWorld,
    assets: &mut ItemAssets,
    materials: &mut Assets<StandardMaterial>,
    status: &mut StatusMsg,
    commands: &mut Commands,
    center: Vec3,
) {
    let r_vox = (TNT_RADIUS_M / VOXEL).ceil() as i32;
    let c_vox = (center / VOXEL).floor().as_ivec3();
    let mut carved = 0usize;
    for dy in -r_vox..=r_vox {
        for dz in -r_vox..=r_vox {
            for dx in -r_vox..=r_vox {
                let v = c_vox + IVec3::new(dx, dy, dz);
                let p = (v.as_vec3() + Vec3::splat(0.5)) * VOXEL;
                if p.distance(center) > TNT_RADIUS_M {
                    continue;
                }
                let id = world.block(v);
                if id == AIR || id == BARRIER {
                    continue;
                }
                world.set_air(v);
                spawn_drop(commands, assets, materials, v, id);
                carved += 1;
            }
        }
    }
    // Radial chip burst for the shockwave read.
    for i in 0..TNT_DEBRIS {
        let u = scatter(c_vox, i as i32) * std::f32::consts::TAU;
        let w = scatter(c_vox, 1000 + i as i32) * 2.0 - 1.0;
        let r = (1.0 - w * w).max(0.0).sqrt();
        let dir = Vec3::new(r * u.cos(), w.abs() * 0.8 + 0.2, r * u.sin());
        let speed = 6.0 + scatter(c_vox, 2000 + i as i32) * 10.0;
        commands.spawn((
            Mesh3d(assets.debris_mesh.clone()),
            MeshMaterial3d(assets.debris_material.clone()),
            Transform::from_translation(center),
            Debris {
                vel: dir * speed,
                ttl: 1.2,
            },
        ));
    }
    status.set(format!("BOOM — {carved} blocks"));
}

// ---------------------------------------------------------------------------
// Debris chips — fall, tumble, vanish. Pure visual.
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
        d.vel.y += GRAVITY * 0.5 * dt;
        tf.translation += d.vel * dt;
        tf.rotation *= Quat::from_rotation_x(6.0 * dt) * Quat::from_rotation_y(4.0 * dt);
    }
}
