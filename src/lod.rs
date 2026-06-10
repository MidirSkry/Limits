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
/// Impostor meshes built per frame (each ~160 verts; keep hitches invisible).
const BUILD_BUDGET: usize = 12;
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
        let e = commands
            .spawn((
                Mesh3d(meshes.add(impostor_mesh(p, 4))),
                MeshMaterial3d(material.clone()),
                Transform::from_translation(p.center),
                PlanetImpostor {
                    idx,
                    center: p.center,
                },
            ))
            .id();
        imp.planets.push(e);
    }
}

/// The advancing horizon swallows planets whole. Hide the impostor and fire a
/// burst of consumption shards — from across the system you see a world die.
fn planet_consume(
    bh: Res<crate::blackhole::BlackHole>,
    eat_fx: Option<Res<crate::blackhole::EatFx>>,
    mut imp: ResMut<Impostors>,
    mut planets: Query<(&PlanetImpostor, &mut Visibility)>,
    mut commands: Commands,
) {
    for (p, mut vis) in &mut planets {
        if imp.eaten_planets.contains(&p.idx) {
            continue;
        }
        if p.center.distance(bh.center) < bh.horizon_r {
            imp.eaten_planets.insert(p.idx);
            *vis = Visibility::Hidden;
            if let Some(fx) = &eat_fx {
                for k in 0..4 {
                    let off = Vec3::new(
                        ((p.idx * 7 + k) % 5) as f32 - 2.0,
                        ((p.idx * 3 + k) % 7) as f32 - 3.0,
                        ((p.idx * 11 + k) % 3) as f32 - 1.0,
                    ) * 40.0;
                    crate::blackhole::spawn_eat_streaks(
                        &mut commands,
                        fx,
                        p.center + off,
                        bh.center,
                    );
                }
            }
        }
    }
}

#[derive(Component)]
struct Impostor {
    center: Vec3,
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

/// Discover asteroid cells entering range; retire impostors far behind us;
/// feed rocks the advancing event horizon has reached to the hole.
fn impostor_sweep(
    player: Res<PlayerState>,
    bh: Res<crate::blackhole::BlackHole>,
    eat_fx: Option<Res<crate::blackhole::EatFx>>,
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
                if imp.map.contains_key(&cell) || imp.eaten.contains(&cell) {
                    continue;
                }
                let Some(a) = world::asteroid_in_cell(cell) else {
                    continue;
                };
                if a.center.distance(player.pos) > IMPOSTOR_RADIUS_M {
                    continue;
                }
                // Already inside the hole: consumed before we ever saw it.
                if a.center.distance(bh.center) < bh.horizon_r {
                    imp.eaten.insert(cell);
                    continue;
                }
                imp.map.insert(cell, Entity::PLACEHOLDER);
                imp.queue.push(cell);
            }
        }
    }

    // Consumption pass: any impostor the horizon has reached is despawned
    // with a shard of light streaking into the hole, and marked eaten so it
    // never pops back in the hole's wake. The dawn reset clears the set.
    let consumed: Vec<IVec3> = imp
        .map
        .iter()
        .filter(|(cell, e)| {
            **e != Entity::PLACEHOLDER
                && ((cell.as_vec3() + Vec3::splat(0.5)) * CELL_M).distance(bh.center)
                    < bh.horizon_r + CELL_M * 0.5
        })
        .map(|(c, _)| *c)
        .collect();
    for cell in consumed {
        imp.eaten.insert(cell);
        if let Some(e) = imp.map.remove(&cell) {
            commands.entity(e).despawn();
            if let Some(fx) = &eat_fx {
                if let Some(a) = world::asteroid_in_cell(cell) {
                    crate::blackhole::spawn_eat_streaks(&mut commands, fx, a.center, bh.center);
                }
            }
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
    bh: Res<crate::blackhole::BlackHole>,
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
        // The horizon may have reached a queued rock before its build slot.
        if a.center.distance(bh.center) < bh.horizon_r {
            imp.map.remove(&cell);
            imp.eaten.insert(cell);
            continue;
        }
        let entity = commands
            .spawn((
                Mesh3d(meshes.add(impostor_mesh(&a, 2))),
                MeshMaterial3d(material.clone()),
                Transform::from_translation(a.center).with_scale(Vec3::splat(0.01)),
                Impostor {
                    center: a.center,
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
        let c = world::impostor_color(a.species, jitter);
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

/// Per frame: grow new impostors in, and yield to the voxel chunks up close
/// (by which point the streamed shell has already drawn over us).
fn impostor_swap(
    time: Res<Time>,
    player: Res<PlayerState>,
    mut impostors: Query<(&mut Impostor, &mut Visibility, &mut Transform)>,
) {
    let dt = time.delta_secs();
    for (mut imp, mut vis, mut tf) in &mut impostors {
        if imp.age < GROW_S {
            imp.age += dt;
            let t = (imp.age / GROW_S).clamp(0.0, 1.0);
            // Ease-out growth.
            let s = 1.0 - (1.0 - t) * (1.0 - t);
            tf.scale = Vec3::splat(s.max(0.01));
        }
        *vis = if imp.center.distance(player.pos) < SWAP_M {
            Visibility::Hidden
        } else {
            Visibility::Visible
        };
    }
}
