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
use std::collections::HashMap;

use crate::player::PlayerState;
use crate::world::{self, Asteroid, CELL_M};

/// How far out impostors exist. Beyond this, rocks are smaller than a pixel
/// or close to it — the starfield carries the rest.
const IMPOSTOR_RADIUS_M: f32 = 520.0;
/// Hide the impostor when the player is this close to the asteroid center —
/// by then the voxel chunks are streamed in and take over.
const SWAP_M: f32 = 40.0;
/// Impostor meshes built per frame (each ~160 verts; keep hitches invisible).
const BUILD_BUDGET: usize = 12;
/// Frames between discovery/cleanup sweeps.
const SWEEP_INTERVAL: u32 = 29;

pub struct LodPlugin;

impl Plugin for LodPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Impostors>()
            .add_systems(Update, (impostor_sweep, impostor_build, impostor_swap));
    }
}

#[derive(Component)]
struct Impostor {
    center: Vec3,
}

#[derive(Resource, Default)]
struct Impostors {
    /// Cell -> impostor entity (PLACEHOLDER while queued for build).
    map: HashMap<IVec3, Entity>,
    queue: Vec<IVec3>,
    material: Option<Handle<StandardMaterial>>,
}

/// Discover asteroid cells entering range; retire impostors far behind us.
fn impostor_sweep(
    player: Res<PlayerState>,
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
                if imp.map.contains_key(&cell) {
                    continue;
                }
                let Some(a) = world::asteroid_in_cell(cell) else {
                    continue;
                };
                if a.center.distance(player.pos) > IMPOSTOR_RADIUS_M {
                    continue;
                }
                imp.map.insert(cell, Entity::PLACEHOLDER);
                imp.queue.push(cell);
            }
        }
    }

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
        let entity = commands
            .spawn((
                Mesh3d(meshes.add(impostor_mesh(&a))),
                MeshMaterial3d(material.clone()),
                Transform::from_translation(a.center),
                Impostor { center: a.center },
            ))
            .id();
        imp.map.insert(cell, entity);
    }
}

/// Displace a low-poly ico-sphere with the asteroid's own surface noise.
fn impostor_mesh(a: &Asteroid) -> Mesh {
    let base = Sphere::new(1.0)
        .mesh()
        .ico(2)
        .expect("ico(2) subdivision is valid");
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
        let r = a.surface_toward(a.center + dir);
        positions.push([dir.x * r, dir.y * r, dir.z * r]);
        // Sphere-smooth normals are fine at impostor distances.
        normals.push([dir.x, dir.y, dir.z]);
        let jitter = ((i as u64).wrapping_mul(0x9E37_79B9).wrapping_add(a.seed) % 255) as f32
            / 255.0;
        let c = world::impostor_color(a.tier, jitter);
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

/// Per frame: impostors near the player yield to the real voxel chunks.
fn impostor_swap(
    player: Res<PlayerState>,
    mut impostors: Query<(&Impostor, &mut Visibility)>,
) {
    for (imp, mut vis) in &mut impostors {
        *vis = if imp.center.distance(player.pos) < SWAP_M {
            Visibility::Hidden
        } else {
            Visibility::Visible
        };
    }
}
