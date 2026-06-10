//! The space dressing: a procedurally generated starfield cubemap (milky way
//! band, nebulae, thousands of stars), an HDR sun, a ringed gas giant, a
//! moon, twinkling foreground stars, shooting stars, and headlamp dust motes
//! inside tunnels. Everything parents to a SkyAnchor that follows the player,
//! so the celestials sit at effective infinity in the open world.
//!
//! Everything is generated at startup from hashes — no texture or model
//! assets. The camera gets Hdr + Bloom + TonyMcMapface here, which is what
//! makes all the >1.0 "hot" colors in the rest of the game actually glow.

use bevy::asset::RenderAssetUsages;
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::core_pipeline::Skybox;
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::post_process::bloom::Bloom;
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, TextureDimension, TextureFormat, TextureViewDescriptor, TextureViewDimension,
};
use bevy::render::view::Hdr;

use crate::player::{Enclosure, PlayerState};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

const CUBE_FACE: u32 = 512;
const SKYBOX_BRIGHTNESS: f32 = 400.0;
/// Where the sun sits (unit direction from the claim).
const SUN_DIR: Vec3 = Vec3::new(0.55, 0.62, 0.30);
const PLANET_DIR: Vec3 = Vec3::new(-0.52, 0.20, -0.70);
const MOON_DIR: Vec3 = Vec3::new(0.05, 0.12, -0.90);
// The new neighborhood: a rust-red rocky world, a deep ice giant, and a
// lighthouse pulsar. Directions chosen to leave the black hole's quadrant
// (-0.65, -0.08, 0.62) uncluttered.
const ROCKY_DIR: Vec3 = Vec3::new(0.80, -0.18, -0.50);
const ICE_DIR: Vec3 = Vec3::new(0.25, -0.62, 0.70);
const PULSAR_DIR: Vec3 = Vec3::new(-0.85, 0.45, -0.25);

const TWINKLE_STARS: usize = 130;
const DUST_MOTES: usize = 70;

pub struct SkyPlugin;

impl Plugin for SkyPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ShootingStarClock {
            next_in: 4.0,
            counter: 0,
        })
        .add_systems(Startup, (setup_celestials, setup_dust))
        .add_systems(PostStartup, setup_camera_sky)
        .add_systems(
            Update,
            (
                anchor_follow,
                twinkle,
                shooting_stars,
                rotate_slow,
                dust_drift,
                comet_drift,
            ),
        );
    }
}

pub fn sun_direction() -> Vec3 {
    SUN_DIR.normalize()
}

/// Sky furniture (sun, planet, moon, twinkle stars) parents to this anchor,
/// which tracks the player —so the celestials never get closer no matter
/// how far you fly. The skybox-at-infinity trick, but for meshes.
#[derive(Component)]
pub struct SkyAnchor;

fn anchor_follow(
    player: Res<PlayerState>,
    mut anchors: Query<&mut Transform, With<SkyAnchor>>,
) {
    for mut tf in &mut anchors {
        tf.translation = player.pos;
    }
}

// ---------------------------------------------------------------------------
// Hash / noise toolbox (deterministic, allocation-free)
// ---------------------------------------------------------------------------

fn hash1(mut x: u64) -> f32 {
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((x >> 40) as f32) / ((1u64 << 24) as f32)
}

fn hash3(x: i32, y: i32, z: i32, salt: u64) -> f32 {
    let p = (x as u64 & 0xFFFFF) | ((y as u64 & 0xFFFFF) << 20) | ((z as u64 & 0xFFFFF) << 40);
    hash1(p ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn smooth(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Trilinear value noise in [0,1].
fn vnoise(p: Vec3, salt: u64) -> f32 {
    let f = p.floor();
    let (ix, iy, iz) = (f.x as i32, f.y as i32, f.z as i32);
    let u = Vec3::new(smooth(p.x - f.x), smooth(p.y - f.y), smooth(p.z - f.z));
    let c = |dx: i32, dy: i32, dz: i32| hash3(ix + dx, iy + dy, iz + dz, salt);
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
// Shared mesh builders
// ---------------------------------------------------------------------------

/// Flat ring (annulus) in the XZ plane, radius `inner..outer`, vertex-colored
/// by `color_fn(t)` where t runs 0 at the inner edge to 1 at the outer.
pub fn ring_mesh(
    inner: f32,
    outer: f32,
    segments: usize,
    color_fn: impl Fn(f32) -> [f32; 4],
) -> Mesh {
    const ROWS: usize = 8;
    let mut positions = Vec::with_capacity((segments + 1) * (ROWS + 1));
    let mut normals = Vec::with_capacity(positions.capacity());
    let mut colors = Vec::with_capacity(positions.capacity());
    let mut indices = Vec::with_capacity(segments * ROWS * 6);
    for r in 0..=ROWS {
        let t = r as f32 / ROWS as f32;
        let radius = inner + (outer - inner) * t;
        let col = color_fn(t);
        for s in 0..=segments {
            let a = s as f32 / segments as f32 * std::f32::consts::TAU;
            positions.push([a.cos() * radius, 0.0, a.sin() * radius]);
            normals.push([0.0, 1.0, 0.0]);
            colors.push(col);
        }
    }
    let stride = (segments + 1) as u32;
    for r in 0..ROWS as u32 {
        for s in 0..segments as u32 {
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

// ---------------------------------------------------------------------------
// Starfield cubemap
// ---------------------------------------------------------------------------

/// Sky radiance (linear, 0..~1.3) for a unit direction.
fn sky_color(dir: Vec3) -> Vec3 {
    // Deep-space floor: not quite black, faintly blue.
    let mut c = Vec3::new(0.004, 0.005, 0.009);

    // Milky way: a great-circle band with fbm structure and a warm core.
    let band_n = Vec3::new(0.31, 0.78, 0.54).normalize();
    let d = dir.dot(band_n);
    let band = (-d * d / 0.025).exp();
    if band > 0.01 {
        let tex = fbm(dir * 5.0 + Vec3::splat(31.7), 4, 0xB17D);
        let lanes = fbm(dir * 11.0 + Vec3::splat(7.3), 3, 0xDA2C);
        // Dust lanes carve darkness out of the bright band.
        let body = (tex * 1.4 - 0.25).max(0.0) * (0.35 + 0.65 * lanes);
        c += Vec3::new(0.32, 0.28, 0.38) * band * body;
        c += Vec3::new(0.50, 0.42, 0.30) * band * (tex - 0.55).max(0.0) * 1.6;
    }

    // Two faint nebulae lobes, teal and magenta, away from the band.
    let neb_a = dir.dot(Vec3::new(-0.62, 0.35, 0.70).normalize()).max(0.0);
    if neb_a > 0.55 {
        let n = fbm(dir * 3.4 + Vec3::splat(91.0), 4, 0x7E8A);
        c += Vec3::new(0.05, 0.22, 0.24) * (neb_a - 0.55).powi(2) * 6.0 * (n * n);
    }
    let neb_b = dir.dot(Vec3::new(0.75, 0.18, -0.63).normalize()).max(0.0);
    if neb_b > 0.6 {
        let n = fbm(dir * 2.9 + Vec3::splat(17.0), 4, 0x44C1);
        c += Vec3::new(0.20, 0.06, 0.22) * (neb_b - 0.6).powi(2) * 7.0 * (n * n);
    }

    // Star layers: dense faint + sparse bright. One cell lookup per layer.
    for (scale, salt, boost) in [(60.0, 0xA001u64, 1.0f32), (18.0, 0xA002, 2.2)] {
        let p = dir * scale;
        let f = p.floor();
        let (ix, iy, iz) = (f.x as i32, f.y as i32, f.z as i32);
        let h = hash3(ix, iy, iz, salt);
        // Star position inside the cell.
        let sp = Vec3::new(
            hash3(ix, iy, iz, salt ^ 0x11),
            hash3(ix, iy, iz, salt ^ 0x22),
            hash3(ix, iy, iz, salt ^ 0x33),
        );
        let local = p - f;
        let dist = (local - sp).length();
        let radius = 0.04 + h * 0.05;
        if dist < radius {
            let falloff = (1.0 - dist / radius).powi(2);
            let bright = h.powi(8) * 14.0 + 0.10;
            // Color temperature: most stars white-blue, a few warm.
            let warm = hash3(ix, iy, iz, salt ^ 0x44);
            let tint = if warm > 0.85 {
                Vec3::new(1.0, 0.75, 0.55)
            } else if warm < 0.2 {
                Vec3::new(0.65, 0.78, 1.0)
            } else {
                Vec3::new(0.92, 0.95, 1.0)
            };
            c += tint * falloff * bright * boost;
        }
    }
    c
}

/// Direction through pixel (u,v) of cubemap face `f`, wgpu face order.
fn face_dir(f: usize, u: f32, v: f32) -> Vec3 {
    let s = u * 2.0 - 1.0;
    let t = v * 2.0 - 1.0;
    match f {
        0 => Vec3::new(1.0, -t, -s),
        1 => Vec3::new(-1.0, -t, s),
        2 => Vec3::new(s, 1.0, t),
        3 => Vec3::new(s, -1.0, -t),
        4 => Vec3::new(s, -t, 1.0),
        _ => Vec3::new(-s, -t, -1.0),
    }
    .normalize()
}

fn build_skybox_image() -> Image {
    let n = CUBE_FACE as usize;
    let mut data = vec![0u8; n * n * 6 * 4];
    for f in 0..6 {
        for y in 0..n {
            for x in 0..n {
                let dir = face_dir(
                    f,
                    (x as f32 + 0.5) / n as f32,
                    (y as f32 + 0.5) / n as f32,
                );
                let c = sky_color(dir);
                let i = ((f * n + y) * n + x) * 4;
                // Linear -> sRGB-ish encode for the Rgba8UnormSrgb format.
                data[i] = (c.x.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
                data[i + 1] = (c.y.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
                data[i + 2] = (c.z.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
                data[i + 3] = 255;
            }
        }
    }
    let mut image = Image::new(
        Extent3d {
            width: CUBE_FACE,
            height: CUBE_FACE * 6,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    image
        .reinterpret_stacked_2d_as_array(6)
        .expect("cubemap stack: face count divides height");
    image.texture_view_descriptor = Some(TextureViewDescriptor {
        dimension: Some(TextureViewDimension::Cube),
        ..default()
    });
    image
}

/// PostStartup: the player camera exists now —bolt the space look onto it.
fn setup_camera_sky(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    cameras: Query<Entity, With<Camera3d>>,
) {
    let skybox = images.add(build_skybox_image());
    for cam in &cameras {
        commands.entity(cam).insert((
            Hdr,
            // NATURAL preset but thinner: enough to halo lasers and crystals
            // without hazing the sunlit surface to white.
            Bloom {
                intensity: 0.09,
                ..Bloom::NATURAL
            },
            Tonemapping::TonyMcMapface,
            Skybox {
                image: skybox.clone(),
                brightness: SKYBOX_BRIGHTNESS,
                rotation: Quat::IDENTITY,
            },
        ));
    }
}

// ---------------------------------------------------------------------------
// Celestial bodies + foreground stars
// ---------------------------------------------------------------------------

#[derive(Component)]
struct Twinkle {
    base: f32,
    phase: f32,
    speed: f32,
}

#[derive(Component)]
struct RotateSlow {
    axis: Vec3,
    rate: f32,
}

fn setup_celestials(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
) {
    let anchor = commands
        .spawn((Transform::IDENTITY, Visibility::Visible, SkyAnchor))
        .id();

    // --- The sun: an HDR ball the bloom pass turns into a glare ------------
    let sun = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(48.0))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::linear_rgb(40.0, 32.0, 22.0),
                unlit: true,
                ..default()
            })),
            Transform::from_translation(sun_direction() * 1600.0),
        ))
        .id();

    // --- Gas giant with rings ----------------------------------------------
    let planet_pos = PLANET_DIR.normalize() * 1800.0;
    let planet = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(260.0).mesh().uv(64, 32))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color_texture: Some(images.add(gas_giant_texture())),
                base_color: Color::WHITE,
                perceptual_roughness: 1.0,
                // Faint self-light so the night side reads against the void.
                emissive: LinearRgba::rgb(0.015, 0.018, 0.035),
                ..default()
            })),
            Transform::from_translation(planet_pos)
                .with_rotation(Quat::from_rotation_z(0.18)),
            RotateSlow {
                axis: Vec3::new(0.18, 1.0, 0.0).normalize(),
                rate: 0.008,
            },
        ))
        .id();
    let rings = commands
        .spawn((
            Mesh3d(meshes.add(ring_mesh(360.0, 620.0, 128, |t| {
                // Banded: alpha pulses with radius, fading at both edges.
                let bands = (0.5 + 0.5 * (t * 43.0).sin()).powf(1.5);
                let edge = (t * (1.0 - t) * 4.0).clamp(0.0, 1.0);
                let a = 0.05 + 0.30 * bands * edge;
                [0.75 + 0.2 * t, 0.72, 0.68 - 0.25 * t, a]
            }))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::WHITE,
                unlit: true,
                alpha_mode: AlphaMode::Add,
                cull_mode: None,
                ..default()
            })),
            Transform::from_translation(planet_pos).with_rotation(Quat::from_euler(
                EulerRot::XYZ,
                0.45,
                0.05,
                0.18,
            )),
        ))
        .id();

    // --- A dead grey moon ----------------------------------------------------
    let moon = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(46.0))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::srgb(0.45, 0.44, 0.43),
                perceptual_roughness: 1.0,
                ..default()
            })),
            Transform::from_translation(MOON_DIR.normalize() * 1500.0),
        ))
        .id();

    // --- A rust-red rocky world with polar caps ------------------------------
    let rocky = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(95.0).mesh().uv(48, 24))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color_texture: Some(images.add(rocky_planet_texture())),
                base_color: Color::WHITE,
                perceptual_roughness: 1.0,
                emissive: LinearRgba::rgb(0.010, 0.006, 0.004),
                ..default()
            })),
            Transform::from_translation(ROCKY_DIR.normalize() * 1900.0)
                .with_rotation(Quat::from_rotation_z(-0.12)),
            RotateSlow {
                axis: Vec3::new(-0.12, 1.0, 0.05).normalize(),
                rate: 0.012,
            },
        ))
        .id();

    // --- An ice giant, deep azure, no rings ---------------------------------
    let ice = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(150.0).mesh().uv(48, 24))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color_texture: Some(images.add(ice_giant_texture())),
                base_color: Color::WHITE,
                perceptual_roughness: 1.0,
                emissive: LinearRgba::rgb(0.008, 0.014, 0.030),
                ..default()
            })),
            Transform::from_translation(ICE_DIR.normalize() * 2100.0)
                .with_rotation(Quat::from_rotation_z(0.35)),
            RotateSlow {
                axis: Vec3::new(0.3, 1.0, -0.1).normalize(),
                rate: 0.006,
            },
        ))
        .id();

    // --- A lighthouse pulsar: HDR core + two sweeping beams ------------------
    let pulsar = commands
        .spawn((
            Transform::from_translation(PULSAR_DIR.normalize() * 1300.0),
            Visibility::Visible,
            RotateSlow {
                axis: Vec3::new(0.2, 1.0, 0.3).normalize(),
                rate: 1.4,
            },
        ))
        .with_children(|p| {
            p.spawn((
                Mesh3d(meshes.add(Sphere::new(3.0))),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::linear_rgb(9.0, 10.0, 14.0),
                    unlit: true,
                    ..default()
                })),
                Transform::IDENTITY,
            ));
            let beam_mat = materials.add(StandardMaterial {
                base_color: Color::linear_rgba(2.2, 2.8, 4.5, 0.30),
                unlit: true,
                alpha_mode: AlphaMode::Add,
                cull_mode: None,
                ..default()
            });
            let beam = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
            // Beams tilted off the spin axis so they sweep like a lighthouse.
            for side in [-1.0f32, 1.0] {
                p.spawn((
                    Mesh3d(beam.clone()),
                    MeshMaterial3d(beam_mat.clone()),
                    Transform::from_translation(Vec3::new(side * 8.0, 0.0, side * 190.0))
                        .looking_to(Vec3::new(side * 0.08, 0.0, side * 1.0), Vec3::Y)
                        .with_scale(Vec3::new(2.0, 2.0, 380.0)),
                ));
            }
        })
        .id();

    // --- A comet on a slow tilted orbit, tail blown anti-sunward -------------
    let comet = commands
        .spawn((
            Mesh3d(meshes.add(Sphere::new(2.2))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: Color::linear_rgb(7.0, 8.5, 9.5),
                unlit: true,
                ..default()
            })),
            Transform::IDENTITY,
            Comet { angle: 1.3 },
        ))
        .with_children(|c| {
            c.spawn((
                Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::linear_rgba(1.6, 2.2, 2.8, 0.30),
                    unlit: true,
                    alpha_mode: AlphaMode::Add,
                    cull_mode: None,
                    ..default()
                })),
                // Local -Z is "away from the sun" (comet_drift orients us).
                Transform::from_translation(Vec3::new(0.0, 0.0, -110.0))
                    .with_scale(Vec3::new(3.5, 3.5, 220.0)),
            ));
        })
        .id();

    commands
        .entity(anchor)
        .add_children(&[sun, planet, rings, moon, rocky, ice, pulsar, comet]);

    // --- Foreground twinkle stars -------------------------------------------
    let star_mesh = meshes.add(Sphere::new(1.0));
    let star_mats = [
        materials.add(StandardMaterial {
            base_color: Color::linear_rgb(4.0, 4.2, 4.8),
            unlit: true,
            ..default()
        }),
        materials.add(StandardMaterial {
            base_color: Color::linear_rgb(2.6, 3.2, 5.0),
            unlit: true,
            ..default()
        }),
        materials.add(StandardMaterial {
            base_color: Color::linear_rgb(5.0, 3.4, 2.0),
            unlit: true,
            ..default()
        }),
    ];
    for i in 0..TWINKLE_STARS {
        let h = |salt: u64| hash1(i as u64 * 7919 ^ salt);
        // All around the sphere now —there's no "below the horizon" in space.
        let z = h(0x1) * 1.9 - 0.9;
        let a = h(0x2) * std::f32::consts::TAU;
        let r = (1.0 - z * z).max(0.0).sqrt();
        let dir = Vec3::new(r * a.cos(), z, r * a.sin());
        let base = 0.9 + h(0x3) * 2.2;
        let star = commands
            .spawn((
                Mesh3d(star_mesh.clone()),
                MeshMaterial3d(star_mats[(h(0x4) * 3.0) as usize % 3].clone()),
                Transform::from_translation(dir * 1200.0).with_scale(Vec3::splat(base)),
                Twinkle {
                    base,
                    phase: h(0x5) * std::f32::consts::TAU,
                    speed: 1.5 + h(0x6) * 4.0,
                },
            ))
            .id();
        commands.entity(anchor).add_child(star);
    }
}

/// Equirect gas-giant bands: indigo/violet/cream with fbm turbulence.
fn gas_giant_texture() -> Image {
    let (w, h) = (512usize, 256usize);
    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        let lat = y as f32 / h as f32;
        for x in 0..w {
            let lon = x as f32 / w as f32;
            // Turbulent latitude: bands wobble with longitude.
            let wob = fbm(
                Vec3::new(lon * 8.0, lat * 22.0, 4.5),
                4,
                0x6A57,
            );
            let band = lat * 26.0 + wob * 2.6;
            let s = (band * std::f32::consts::TAU / 4.0).sin() * 0.5 + 0.5;
            let deep = Vec3::new(0.13, 0.10, 0.28);
            let mid = Vec3::new(0.30, 0.22, 0.48);
            let cream = Vec3::new(0.75, 0.68, 0.58);
            let c = if s < 0.5 {
                deep.lerp(mid, s * 2.0)
            } else {
                mid.lerp(cream, (s - 0.5) * 2.0)
            };
            // A couple of storm ovals.
            let storm = ((lon - 0.3).powi(2) * 40.0 + (lat - 0.62).powi(2) * 90.0)
                .min((lon - 0.74).powi(2) * 60.0 + (lat - 0.35).powi(2) * 120.0);
            let c = if storm < 1.0 {
                c.lerp(Vec3::new(0.85, 0.55, 0.40), (1.0 - storm) * 0.8)
            } else {
                c
            };
            let i = (y * w + x) * 4;
            data[i] = (c.x.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 1] = (c.y.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 2] = (c.z.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 3] = 255;
        }
    }
    Image::new(
        Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// Equirect rocky-world texture: rust plains, dark maria, polar ice.
fn rocky_planet_texture() -> Image {
    let (w, h) = (512usize, 256usize);
    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        let lat = y as f32 / h as f32; // 0 = north pole
        for x in 0..w {
            let lon = x as f32 / w as f32;
            let p = Vec3::new(lon * 9.0, lat * 5.0, 2.0);
            let n = fbm(p, 4, 0x5EA5);
            let m = fbm(p * 2.3 + Vec3::splat(13.0), 3, 0x77AA);
            let rust = Vec3::new(0.55, 0.30, 0.18);
            let dark = Vec3::new(0.26, 0.15, 0.11);
            let sand = Vec3::new(0.68, 0.50, 0.32);
            let mut c = if n < 0.45 {
                dark.lerp(rust, n / 0.45)
            } else {
                rust.lerp(sand, ((n - 0.45) / 0.55).powf(1.3))
            };
            // Mottling so the surface isn't airbrushed.
            c *= 0.85 + 0.3 * m;
            // Polar ice caps with a noisy edge.
            let polar = (lat.min(1.0 - lat) * 2.0) + (m - 0.5) * 0.12;
            if polar < 0.22 {
                let k = (1.0 - polar / 0.22).clamp(0.0, 1.0);
                c = c.lerp(Vec3::new(0.92, 0.94, 0.97), k * k);
            }
            let i = (y * w + x) * 4;
            data[i] = (c.x.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 1] = (c.y.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 2] = (c.z.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 3] = 255;
        }
    }
    Image::new(
        Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// Equirect ice-giant texture: smooth azure bands, one pale storm streak.
fn ice_giant_texture() -> Image {
    let (w, h) = (512usize, 256usize);
    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        let lat = y as f32 / h as f32;
        for x in 0..w {
            let lon = x as f32 / w as f32;
            let wob = fbm(Vec3::new(lon * 6.0, lat * 16.0, 8.5), 3, 0x1CE9);
            let band = lat * 14.0 + wob * 1.2;
            let s = (band * std::f32::consts::TAU / 4.0).sin() * 0.5 + 0.5;
            let deep = Vec3::new(0.06, 0.16, 0.38);
            let pale = Vec3::new(0.30, 0.55, 0.78);
            let mut c = deep.lerp(pale, s * 0.7);
            // A single bright methane streak.
            let streak = ((lat - 0.38).abs() * 30.0 + (wob - 0.5).abs() * 4.0).min(4.0);
            if streak < 1.0 {
                c = c.lerp(Vec3::new(0.80, 0.92, 0.98), (1.0 - streak) * 0.5);
            }
            let i = (y * w + x) * 4;
            data[i] = (c.x.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 1] = (c.y.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 2] = (c.z.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8;
            data[i + 3] = 255;
        }
    }
    Image::new(
        Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        data,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    )
}

/// A comet crawling along a tilted circle (anchor-relative, so effectively a
/// fixture of the sky), head always oriented so the tail streams anti-sun.
#[derive(Component)]
struct Comet {
    angle: f32,
}

fn comet_drift(time: Res<Time>, mut comets: Query<(&mut Comet, &mut Transform)>) {
    let dt = time.delta_secs();
    let tilt = Quat::from_euler(EulerRot::XYZ, 0.45, 0.0, 0.30);
    for (mut comet, mut tf) in &mut comets {
        comet.angle += dt * 0.005;
        let (sin, cos) = comet.angle.sin_cos();
        tf.translation = tilt * Vec3::new(cos * 1050.0, 180.0, sin * 1050.0);
        // Tail child sits along local -Z: face local -Z away from the sun.
        tf.rotation = Quat::from_rotation_arc(Vec3::NEG_Z, -sun_direction());
    }
}

fn twinkle(time: Res<Time>, mut stars: Query<(&Twinkle, &mut Transform)>) {
    let t = time.elapsed_secs();
    for (tw, mut tf) in &mut stars {
        let s = tw.base * (0.72 + 0.38 * (t * tw.speed + tw.phase).sin());
        tf.scale = Vec3::splat(s);
    }
}

fn rotate_slow(time: Res<Time>, mut q: Query<(&RotateSlow, &mut Transform)>) {
    let dt = time.delta_secs();
    for (r, mut tf) in &mut q {
        tf.rotation = Quat::from_axis_angle(r.axis, r.rate * dt) * tf.rotation;
    }
}

// ---------------------------------------------------------------------------
// Shooting stars
// ---------------------------------------------------------------------------

#[derive(Resource)]
struct ShootingStarClock {
    next_in: f32,
    counter: u64,
}

#[derive(Component)]
struct ShootingStar {
    vel: Vec3,
    ttl: f32,
}

fn shooting_stars(
    time: Res<Time>,
    player: Res<PlayerState>,
    mut clock: ResMut<ShootingStarClock>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut stars: Query<(Entity, &mut ShootingStar, &mut Transform)>,
    mut commands: Commands,
    mut local_assets: Local<Option<(Handle<Mesh>, Handle<StandardMaterial>)>>,
) {
    let dt = time.delta_secs();

    // Animate + retire live streaks.
    for (e, mut s, mut tf) in &mut stars {
        s.ttl -= dt;
        if s.ttl <= 0.0 {
            commands.entity(e).despawn();
            continue;
        }
        let v = s.vel;
        tf.translation += v * dt;
        // Tail stretches early, shrinks as it dies.
        let life = (s.ttl / 2.0).clamp(0.0, 1.0);
        tf.scale = Vec3::new(1.0, 1.0, 0.4 + life * 1.2);
    }

    clock.next_in -= dt;
    if clock.next_in > 0.0 {
        return;
    }
    clock.counter += 1;
    let counter = clock.counter;
    let h = |salt: u64| hash1(counter.wrapping_mul(2654435761) ^ salt);
    clock.next_in = 2.5 + h(0xAA) * 6.0;

    let (mesh, material) = local_assets
        .get_or_insert_with(|| {
            (
                meshes.add(Cuboid::new(0.7, 0.7, 34.0)),
                materials.add(StandardMaterial {
                    base_color: Color::linear_rgb(6.0, 7.0, 8.5),
                    unlit: true,
                    ..default()
                }),
            )
        })
        .clone();

    // Spawn anywhere on the sky sphere, streaking tangentially.
    let z = h(0x1) * 1.6 - 0.8;
    let a = h(0x2) * std::f32::consts::TAU;
    let r = (1.0 - z * z).max(0.0).sqrt();
    let dir = Vec3::new(r * a.cos(), z, r * a.sin());
    let pos = player.pos + dir * 1100.0;
    let tangent = dir.cross(Vec3::new(h(0x3) - 0.5, h(0x4) - 0.5, h(0x5) - 0.5).normalize())
        .normalize_or_zero();
    let vel = tangent * (350.0 + h(0x6) * 250.0);
    commands.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_translation(pos).looking_to(vel.normalize_or_zero(), Vec3::Y),
        ShootingStar { vel, ttl: 2.0 },
    ));
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Dust motes —drifting specks in the headlamp beam underground
// ---------------------------------------------------------------------------

#[derive(Component)]
struct DustMote {
    vel: Vec3,
}

fn setup_dust(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mesh = meshes.add(Cuboid::new(0.012, 0.012, 0.012));
    let mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgba(1.5, 1.5, 1.6, 0.35),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        cull_mode: None,
        ..default()
    });
    for i in 0..DUST_MOTES as u64 {
        let h = |salt: u64| hash1(i.wrapping_mul(40503) ^ salt);
        commands.spawn((
            Mesh3d(mesh.clone()),
            MeshMaterial3d(mat.clone()),
            Transform::from_translation(Vec3::new(
                h(1) * 8.0 - 4.0,
                h(2) * 8.0 - 4.0,
                h(3) * 8.0 - 4.0,
            )),
            Visibility::Hidden,
            DustMote {
                vel: Vec3::new(h(4) - 0.5, h(5) - 0.6, h(6) - 0.5) * 0.16,
            },
        ));
    }
}

/// Drift around the player, wrapping inside a 8m box; only visible when
/// you're buried enough that the headlamp is doing the lighting.
fn dust_drift(
    time: Res<Time>,
    player: Res<PlayerState>,
    enclosure: Res<Enclosure>,
    mut motes: Query<(&DustMote, &mut Transform, &mut Visibility)>,
) {
    let dt = time.delta_secs();
    let show = enclosure.0 > 0.35;
    let center = player.eye();
    for (mote, mut tf, mut vis) in &mut motes {
        *vis = if show {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        if !show {
            continue;
        }
        tf.translation += mote.vel * dt;
        // Wrap each axis into a box around the player.
        for axis in 0..3 {
            let d = tf.translation[axis] - center[axis];
            if d > 4.0 {
                tf.translation[axis] -= 8.0;
            } else if d < -4.0 {
                tf.translation[axis] += 8.0;
            }
        }
    }
}
