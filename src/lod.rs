//! Far-field LOD: impostor meshes for distant asteroids.
//!
//! Voxel chunks only materialize within ~56m, which used to mean rocks popped
//! into existence at that line. Instead, every asteroid out to
//! `IMPOSTOR_RADIUS_M` gets a ~160-vertex ico-sphere displaced by the SAME
//! `surface_toward` noise the voxel generator uses — silhouettes match, so
//! when the real chunks finish streaming in and the impostor hides, the swap
//! is hard to spot. Cost: a few thousand small static meshes, built lazily a
//! few per frame and despawned far behind the player.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::{Indices, PrimitiveTopology, VertexAttributeValues};
use bevy::prelude::*;
use std::collections::{HashMap, HashSet};

use crate::player::PlayerState;
use crate::world::{self, Asteroid, CELL_M};

/// How far out impostors exist. At this range even the biggest rock is a
/// couple dozen pixels, and grow-in hides the birth.
const IMPOSTOR_RADIUS_M: f32 = 850.0;
/// Hide the impostor when the player is this close to the asteroid center.
/// By then the streamed voxel shell has already drawn over it (see INSET_M),
/// so the hide itself is invisible.
const SWAP_M: f32 = 38.0;
/// Impostors sit this far INSIDE the true surface: voxel cubes quantize
/// outward from the noise surface, so an inset impostor gets shrouded by the
/// real chunks as they stream in — the rock "resolves" into voxels instead
/// of flipping representation.
const INSET_M: f32 = 0.30;
/// New impostors scale up over this long so frontier spawns emerge softly.
const GROW_S: f32 = 1.1;
/// Impostor meshes built per frame (each ~640 verts; keep hitches invisible).
const BUILD_BUDGET: usize = 8;
/// Frames between discovery/cleanup sweeps.
const SWEEP_INTERVAL: u32 = 29;

pub struct LodPlugin;

impl Plugin for LodPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Impostors>().add_systems(
            Update,
            (
                ensure_planet_impostors,
                impostor_sweep,
                impostor_build,
                impostor_swap,
                planet_consume,
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// Planets — one big impostor each, alive all day
// ---------------------------------------------------------------------------

#[derive(Component)]
struct PlanetImpostor {
    idx: usize,
    center: Vec3,
    /// Cooldown for shedding matter while inside the hole's tidal zone.
    shed_cd: f32,
}

/// (Re)build planet impostors whenever none exist — at boot and on the frame
/// after a dawn reset (despawn_all empties the list; the world salt has
/// already changed, so this picks up the NEW day's planets).
fn ensure_planet_impostors(
    mut imp: ResMut<Impostors>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut commands: Commands,
) {
    if !imp.planets.is_empty() {
        return;
    }
    let material = imp
        .material
        .get_or_insert_with(|| {
            materials.add(StandardMaterial {
                base_color: Color::WHITE,
                perceptual_roughness: 0.95,
                ..default()
            })
        })
        .clone();
    let planets = world::planet_list();
    for (idx, p) in planets.iter().enumerate() {
        let tint = world::species_tint(p.species);
        let e = commands
            .spawn((
                Mesh3d(meshes.add(impostor_mesh(p, 5))),
                MeshMaterial3d(material.clone()),
                Transform::from_translation(p.center),
                PlanetImpostor {
                    idx,
                    center: p.center,
                    shed_cd: 0.0,
                },
            ))
            .with_children(|world_body| {
                // Atmosphere halo: a species-tinted additive shell. THE thing
                // that says "planet" instead of "large rock" at any distance.
                world_body.spawn((
                    Mesh3d(meshes.add(Sphere::new(p.reach() * 1.045).mesh().ico(4).unwrap())),
                    MeshMaterial3d(materials.add(StandardMaterial {
                        base_color: Color::linear_rgba(
                            0.10 + tint[0] * 0.55,
                            0.10 + tint[1] * 0.55,
                            0.12 + tint[2] * 0.55,
                            0.20,
                        ),
                        unlit: true,
                        alpha_mode: AlphaMode::Add,
                        cull_mode: None,
                        ..default()
                    })),
                    Transform::IDENTITY,
                ));
                // A third of the worlds get rings.
                if p.seed % 3 == 0 {
                    world_body.spawn((
                        Mesh3d(meshes.add(crate::sky::ring_mesh(
                            p.reach() * 1.5,
                            p.reach() * 2.5,
                            96,
                            |t| {
                                let bands = (0.5 + 0.5 * (t * 31.0).sin()).powf(1.4);
                                let edge = (t * (1.0 - t) * 4.0).clamp(0.0, 1.0);
                                let a = 0.04 + 0.22 * bands * edge;
                                [
                                    0.5 + tint[0] * 0.8,
                                    0.5 + tint[1] * 0.8,
                                    0.5 + tint[2] * 0.8,
                                    a,
                                ]
                            },
                        ))),
                        MeshMaterial3d(materials.add(StandardMaterial {
                            base_color: Color::WHITE,
                            unlit: true,
                            alpha_mode: AlphaMode::Add,
                            cull_mode: None,
                            ..default()
                        })),
                        Transform::from_rotation(Quat::from_euler(
                            EulerRot::XYZ,
                            0.3 + (p.seed % 7) as f32 * 0.1,
                            0.0,
                            0.2,
                        )),
                    ));
                }
            })
            .id();
        imp.planets.push(e);
    }
}

/// The hole takes planets in stages: the BODY (voxels and all) is dragged in
/// for real by world::body_motion — here the impostor follows that motion,
/// tidally deforms inside ~2.4 horizon radii (stretching toward the
/// singularity, shedding streams of matter), and throws a shard burst the
/// moment the world's actual chunks are consumed.
fn planet_consume(
    time: Res<Time>,
    bh: Res<crate::blackhole::BlackHole>,
    world_res: Res<crate::world::VoxelWorld>,
    eat_fx: Option<Res<crate::blackhole::EatFx>>,
    mut imp: ResMut<Impostors>,
    mut planets: Query<(&mut PlanetImpostor, &mut Visibility, &mut Transform)>,
    mut commands: Commands,
) {
    let dt = time.delta_secs();
    for (mut p, mut vis, mut tf) in &mut planets {
        if imp.eaten_planets.contains(&p.idx) {
            continue;
        }
        let key = world::planet_key(p.idx);
        let current = p.center + world_res.offset_of(key);
        tf.translation = current;
        if world_res.is_eaten(key) {
            imp.eaten_planets.insert(p.idx);
            *vis = Visibility::Hidden;
            if let Some(fx) = &eat_fx {
                for k in 0..10 {
                    let off = Vec3::new(
                        ((p.idx * 7 + k) % 5) as f32 - 2.0,
                        ((p.idx * 3 + k) % 7) as f32 - 3.0,
                        ((p.idx * 11 + k) % 3) as f32 - 1.0,
                    ) * 60.0;
                    crate::blackhole::spawn_eat_streaks(
                        &mut commands,
                        fx,
                        current + off,
                        bh.center,
                    );
                }
            }
            continue;
        }
        let d = current.distance(bh.center);
        if bh.horizon_r > 1.0 && d < bh.horizon_r * 2.4 {
            // Tidal zone: stretch toward the hole, thin across, shed matter.
            let k = (1.0 - (d - bh.horizon_r) / (bh.horizon_r * 1.4)).clamp(0.0, 1.0);
            let dir = (bh.center - current).normalize_or_zero();
            tf.rotation = Quat::from_rotation_arc(Vec3::Z, dir);
            let stretch = 1.0 + 1.1 * k;
            let thin = 1.0 / stretch.sqrt();
            tf.scale = Vec3::new(thin, thin, stretch);
            p.shed_cd -= dt;
            if p.shed_cd <= 0.0 {
                p.shed_cd = 2.6 - 2.1 * k;
                if let Some(fx) = &eat_fx {
                    crate::blackhole::spawn_eat_streaks(
                        &mut commands,
                        fx,
                        current + dir * d.min(400.0) * 0.3,
                        bh.center,
                    );
                }
            }
        } else if tf.scale != Vec3::ONE {
            tf.scale = Vec3::ONE;
            tf.rotation = Quat::IDENTITY;
        }
    }
}

#[derive(Component)]
struct Impostor {
    /// Gen-space center; the body's live offset is added every frame.
    center: Vec3,
    cell: IVec3,
    age: f32,
}

#[derive(Resource, Default)]
pub struct Impostors {
    /// Cell -> impostor entity (PLACEHOLDER while queued for build).
    map: HashMap<IVec3, Entity>,
    queue: Vec<IVec3>,
    /// Rocks the event horizon has consumed this day — never rebuilt (the
    /// hole advances past them, so a plain distance check would resurrect
    /// them in its wake). Cleared by the dawn reset.
    eaten: HashSet<IVec3>,
    /// Planet impostor entities, by planet index. Unlike belt impostors these
    /// live all day, visible from anywhere in the system (they ARE the
    /// planet beyond chunk range; streamed chunks draw over them up close).
    planets: Vec<Entity>,
    eaten_planets: HashSet<usize>,
    material: Option<Handle<StandardMaterial>>,
}

impl Impostors {
    /// Day reset: the field reseeds, so every cached far-rock is wrong.
    /// Despawn them all; the sweep rediscovers the new field within a frame.
    pub fn despawn_all(&mut self, commands: &mut Commands) {
        for (_, e) in self.map.drain() {
            if e != Entity::PLACEHOLDER {
                commands.entity(e).despawn();
            }
        }
        self.queue.clear();
        self.eaten.clear();
        for e in self.planets.drain(..) {
            commands.entity(e).despawn();
        }
        self.eaten_planets.clear();
    }
}

/// Discover asteroid cells entering range; retire impostors far behind us or
/// consumed by the hole (the BODIES are eaten for real by world::body_motion;
/// here we just free the matching impostor entity).
fn impostor_sweep(
    player: Res<PlayerState>,
    world_res: Res<crate::world::VoxelWorld>,
    mut imp: ResMut<Impostors>,
    mut commands: Commands,
    mut tick: Local<u32>,
) {
    *tick = tick.wrapping_add(1);
    if *tick % SWEEP_INTERVAL != 1 {
        return;
    }
    let pc = (player.pos / CELL_M).floor().as_ivec3();
    let r_cells = (IMPOSTOR_RADIUS_M / CELL_M).ceil() as i32;

    for cz in -r_cells..=r_cells {
        for cy in -r_cells..=r_cells {
            for cx in -r_cells..=r_cells {
                let cell = pc + IVec3::new(cx, cy, cz);
                if imp.map.contains_key(&cell)
                    || imp.eaten.contains(&cell)
                    || world_res.is_eaten(cell)
                {
                    continue;
                }
                let Some(a) = world::asteroid_in_cell(cell) else {
                    continue;
                };
                let current = a.center + world_res.offset_of(cell);
                if current.distance(player.pos) > IMPOSTOR_RADIUS_M {
                    continue;
                }
                imp.map.insert(cell, Entity::PLACEHOLDER);
                imp.queue.push(cell);
            }
        }
    }

    // Free impostors of bodies the hole has actually eaten.
    let consumed: Vec<IVec3> = imp
        .map
        .iter()
        .filter(|(cell, e)| **e != Entity::PLACEHOLDER && world_res.is_eaten(**cell))
        .map(|(c, _)| *c)
        .collect();
    for cell in consumed {
        imp.eaten.insert(cell);
        if let Some(e) = imp.map.remove(&cell) {
            commands.entity(e).despawn();
        }
    }

    // Nearest rocks build first: pop() takes from the end, so sort far-first.
    let ppos = player.pos;
    imp.queue.sort_by(|a, b| {
        let da = ((a.as_vec3() + Vec3::splat(0.5)) * CELL_M).distance_squared(ppos);
        let db = ((b.as_vec3() + Vec3::splat(0.5)) * CELL_M).distance_squared(ppos);
        db.total_cmp(&da)
    });

    // Cleanup pass: drop impostors well outside the radius.
    let limit = IMPOSTOR_RADIUS_M * 1.2;
    let far: Vec<IVec3> = imp
        .map
        .iter()
        .filter(|(cell, e)| {
            **e != Entity::PLACEHOLDER
                && ((cell.as_vec3() + Vec3::splat(0.5)) * CELL_M).distance(player.pos) > limit
        })
        .map(|(c, _)| *c)
        .collect();
    for cell in far {
        if let Some(e) = imp.map.remove(&cell) {
            commands.entity(e).despawn();
        }
    }
}

/// Build queued impostor meshes, a few per frame.
fn impostor_build(
    world_res: Res<crate::world::VoxelWorld>,
    mut imp: ResMut<Impostors>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut commands: Commands,
) {
    if imp.queue.is_empty() {
        return;
    }
    let material = imp
        .material
        .get_or_insert_with(|| {
            materials.add(StandardMaterial {
                base_color: Color::WHITE,
                perceptual_roughness: 0.95,
                ..default()
            })
        })
        .clone();

    for _ in 0..BUILD_BUDGET {
        let Some(cell) = imp.queue.pop() else {
            break;
        };
        let Some(a) = world::asteroid_in_cell(cell) else {
            continue;
        };
        // The hole may have eaten a queued rock before its build slot.
        if world_res.is_eaten(cell) {
            imp.map.remove(&cell);
            imp.eaten.insert(cell);
            continue;
        }
        let entity = commands
            .spawn((
                Mesh3d(meshes.add(impostor_mesh(&a, 3))),
                MeshMaterial3d(material.clone()),
                Transform::from_translation(a.center).with_scale(Vec3::splat(0.01)),
                Impostor {
                    center: a.center,
                    cell,
                    age: 0.0,
                },
            ))
            .id();
        imp.map.insert(cell, entity);
    }
}

/// Displace a low-poly ico-sphere with the asteroid's own surface noise.
/// Subdivision 2 (~160 verts) suits belt rocks; planets get 4 (~2.5k) since
/// one mesh serves a 200m world all day.
fn impostor_mesh(a: &Asteroid, subdiv: u32) -> Mesh {
    let base = Sphere::new(1.0)
        .mesh()
        .ico(subdiv)
        .expect("ico subdivision is valid");
    let Some(VertexAttributeValues::Float32x3(dirs)) =
        base.attribute(Mesh::ATTRIBUTE_POSITION)
    else {
        unreachable!("sphere mesh has positions");
    };
    let mut positions = Vec::with_capacity(dirs.len());
    let mut normals = Vec::with_capacity(dirs.len());
    let mut colors = Vec::with_capacity(dirs.len());
    for (i, d) in dirs.iter().enumerate() {
        let dir = Vec3::from(*d).normalize_or_zero();
        // Quantize to voxel steps (terraced silhouette like the real rock),
        // then tuck inside the true surface so streamed chunks shroud us.
        let r = a.surface_toward(a.center + dir);
        let r = ((r / world::VOXEL).floor() * world::VOXEL - INSET_M).max(1.0);
        positions.push([dir.x * r, dir.y * r, dir.z * r]);
        // Sphere-smooth normals are fine at impostor distances.
        normals.push([dir.x, dir.y, dir.z]);
        let jitter = ((i as u64).wrapping_mul(0x9E37_79B9).wrapping_add(a.seed) % 255) as f32
            / 255.0;
        let c = world::impostor_color(a.species, jitter, a.is_planet);
        colors.push([c[0], c[1], c[2], 1.0]);
    }
    let indices = base
        .indices()
        .expect("sphere mesh is indexed")
        .iter()
        .map(|i| i as u32)
        .collect::<Vec<_>>();
    Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
    .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
    .with_inserted_indices(Indices::U32(indices))
}

/// Per frame: follow each body's REAL displaced position, grow new impostors
/// in, yield to the voxel chunks up close (by which point the streamed shell
/// has already drawn over us), and tidally deform anything inside the hole's
/// kill zone.
fn impostor_swap(
    time: Res<Time>,
    player: Res<PlayerState>,
    bh: Res<crate::blackhole::BlackHole>,
    world: Res<crate::world::VoxelWorld>,
    mut impostors: Query<(&mut Impostor, &mut Visibility, &mut Transform)>,
) {
    let dt = time.delta_secs();
    for (mut imp, mut vis, mut tf) in &mut impostors {
        // The body's true position: gen center + the infall it has suffered.
        let current = imp.center + world.offset_of(imp.cell);
        tf.translation = current;
        if world.is_eaten(imp.cell) {
            *vis = Visibility::Hidden;
            continue;
        }
        let mut s = 1.0;
        if imp.age < GROW_S {
            imp.age += dt;
            let t = (imp.age / GROW_S).clamp(0.0, 1.0);
            // Ease-out growth.
            s = (1.0 - (1.0 - t) * (1.0 - t)).max(0.01);
        }
        // Inside ~2x the horizon, rocks visibly stretch toward the hole on
        // top of their real motion.
        let d = current.distance(bh.center);
        if bh.horizon_r > 1.0 && d < bh.horizon_r * 2.0 {
            let k = (1.0 - (d - bh.horizon_r) / bh.horizon_r).clamp(0.0, 1.0);
            let dir = (bh.center - current).normalize_or_zero();
            tf.rotation = Quat::from_rotation_arc(Vec3::Z, dir);
            let stretch = 1.0 + 1.6 * k;
            tf.scale = Vec3::new(
                s / stretch.sqrt(),
                s / stretch.sqrt(),
                s * stretch,
            );
        } else {
            tf.scale = Vec3::splat(s);
        }
        *vis = if current.distance(player.pos) < SWAP_M {
            Visibility::Hidden
        } else {
            Visibility::Visible
        };
    }
}
