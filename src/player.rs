//! First-person player: mouse look, zero-G 6DOF thruster flight with voxel
//! AABB collision, surface walking on asteroid tops, and the mining laser
//! (hold LMB — continuous damage-per-second vs block HP, with a heat loop).

use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions};

use crate::audio::{SfxEvent, SfxQueue};
use crate::game::Upgrades;
use crate::items::{self, ItemAssets};
use crate::world::{
    self, block_display_name, block_hp, raycast, tier_of_voxel, VoxelWorld, AIR, BARRIER, ORE,
    REGOLITH, VOXEL,
};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Player collision half-extents (so 0.6m wide, 1.4m tall — ~6 voxels).
const PLAYER_HALF: Vec3 = Vec3::new(0.30, 0.70, 0.30);
/// Eye height above the feet.
const EYE: f32 = 1.25;
/// Suit thrusters: zero-G 6DOF flight. W/S along the look ray, A/D strafe,
/// Space up, C down, Shift brakes hard. Idle drift bleeds off slowly so
/// space feels floaty but the ship... er, miner, stays controllable.
const THRUST: f32 = 16.0;
const MAX_SPEED: f32 = 18.0;
const IDLE_DAMP: f32 = 1.1;
const BRAKE_DAMP: f32 = 6.0;
/// Walking on a surface uses direct velocity for precise mining footwork.
const WALK_SPEED: f32 = 4.5;
/// Hop impulse when grounded (Space tap); hold to keep thrusting up.
const JUMP_VEL: f32 = 4.0;
/// Don't cross more than ~half a voxel per collision substep.
const MAX_STEP_SPEED: f32 = 28.0;
const MOUSE_SENS: f32 = 0.0023;
/// Mining reach in world units (~17 voxels).
const REACH: f32 = 4.2;

/// Heat fraction below which an overheat lockout clears.
const HEAT_UNLOCK: f32 = 0.35;
/// Locked venting cools faster than ordinary idle cooling.
const VENT_BONUS: f32 = 1.6;

/// Viewmodel rest offset in camera space.
const VM_BASE: Vec3 = Vec3::new(0.30, -0.24, -0.42);
/// Muzzle tip in camera space — the beam starts here.
const VM_MUZZLE: Vec3 = Vec3::new(0.30, -0.225, -0.84);

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Resource)]
pub struct PlayerState {
    /// Feet position (bottom-center of the collision box).
    pub pos: Vec3,
    pub vel: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub grounded: bool,
    /// Farthest range from home reached (m), and where that was.
    pub max_range: f32,
    pub far_pos: Vec3,
}

impl PlayerState {
    pub fn spawn_point() -> Vec3 {
        world::home_spawn()
    }

    pub fn eye(&self) -> Vec3 {
        self.pos + Vec3::Y * EYE
    }

    pub fn look_rot(&self) -> Quat {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0)
    }

    pub fn look_dir(&self) -> Vec3 {
        self.look_rot() * Vec3::NEG_Z
    }

    /// Distance out from the home rock's surface — the progression axis.
    /// (Standing at the depot reads 0, not the rock's radius.)
    pub fn range_m(&self) -> f32 {
        (self.pos.length() - world::HOME_R).max(0.0)
    }
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            pos: Self::spawn_point(),
            vel: Vec3::ZERO,
            yaw: 0.0,
            pitch: -0.2,
            grounded: false,
            max_range: 0.0,
            far_pos: Self::spawn_point(),
        }
    }
}

/// How buried the player is, 0 (open sky) to 1 (rock overhead). Drives the
/// sun/ambient fade and the dust motes. Probed with an upward ray and
/// smoothed so light doesn't pop when crossing a tunnel mouth.
#[derive(Resource, Default)]
pub struct Enclosure(pub f32);

/// True while the cursor is grabbed and gameplay input is live.
#[derive(Resource, Default)]
pub struct Focused(pub bool);

/// Mining-laser state, published for the HUD, audio, and FX systems.
#[derive(Resource, Default)]
pub struct LaserState {
    pub firing: bool,
    /// 0..1; at 1.0 the beam locks out until it vents below HEAT_UNLOCK.
    pub heat: f32,
    pub locked: bool,
    pub beam_start: Vec3,
    pub beam_end: Vec3,
    pub has_hit: bool,
}

/// True while the jetpack is thrusting (audio hook).
#[derive(Resource, Default)]
pub struct JetState(pub bool);

/// Screen-shake trauma. Effects add to it; the camera turns trauma² into a
/// rotational wobble and it decays fast.
#[derive(Resource, Default)]
pub struct Shake {
    pub trauma: f32,
}

impl Shake {
    pub fn add(&mut self, amount: f32) {
        self.trauma = (self.trauma + amount).min(1.0);
    }
}

/// What the crosshair is pointing at, for the HUD.
#[derive(Resource, Default)]
pub struct TargetInfo(pub Option<TargetBlock>);

pub struct TargetBlock {
    pub name: String,
    /// Remaining HP fraction in [0,1].
    pub hp_frac: f32,
    pub indestructible: bool,
    /// Sale value if this is a crystal, else 0.
    pub value: u64,
}

#[derive(Component)]
struct PlayerCamera;

/// Laser FX entities, telled apart by kind. They all live at the world root.
#[derive(Component, PartialEq, Eq, Clone, Copy)]
enum LaserFx {
    Core,
    Halo,
    Glow,
    Light,
}

#[derive(Component)]
struct ImpactLightMark;

/// Helmet lamp parts — intensity follows Enclosure (sun does the work
/// outside; the lamp takes over in tunnels, instead of stacking on daylight).
#[derive(Component)]
struct HeadLamp {
    max: f32,
    min: f32,
}

/// Root of the first-person tool model (child of the camera).
#[derive(Component)]
struct ViewmodelRoot;

#[derive(Resource)]
struct ViewmodelMats {
    emitter: Handle<StandardMaterial>,
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlayerState>()
            .init_resource::<Focused>()
            .init_resource::<TargetInfo>()
            .init_resource::<LaserState>()
            .init_resource::<JetState>()
            .init_resource::<Shake>()
            .init_resource::<Enclosure>()
            .add_systems(Startup, setup_player)
            .add_systems(
                Update,
                (
                    grab_cursor,
                    mouse_look,
                    player_move,
                    update_enclosure,
                    headlamp_adapt,
                    mining,
                    sync_camera,
                    laser_fx,
                    viewmodel_update,
                )
                    .chain()
                    .in_set(crate::GameplaySet),
            );
    }
}

fn setup_player(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let metal = materials.add(StandardMaterial {
        base_color: Color::srgb(0.13, 0.14, 0.17),
        metallic: 0.85,
        perceptual_roughness: 0.35,
        ..default()
    });
    let accent = materials.add(StandardMaterial {
        base_color: Color::srgb(0.20, 0.23, 0.28),
        metallic: 0.6,
        perceptual_roughness: 0.5,
        ..default()
    });
    let emitter = materials.add(StandardMaterial {
        base_color: Color::srgb(0.05, 0.08, 0.10),
        emissive: LinearRgba::rgb(0.2, 1.2, 1.4),
        ..default()
    });
    commands.insert_resource(ViewmodelMats {
        emitter: emitter.clone(),
    });

    commands
        .spawn((
            Camera3d::default(),
            Projection::Perspective(PerspectiveProjection {
                fov: 1.22, // ~70°
                far: 2500.0, // the sky furniture lives way out there
                ..default()
            }),
            Transform::from_translation(PlayerState::spawn_point() + Vec3::Y * EYE),
            // Per-camera ambient light (0.18: AmbientLight is a component, not
            // a resource). depth_lighting in main.rs retunes it every frame.
            AmbientLight {
                color: Color::srgb(0.85, 0.90, 1.0),
                brightness: 130.0,
                ..default()
            },
            PlayerCamera,
        ))
        .with_children(|parent| {
            // Helmet lamp: a tight spot doing the "headlamp cone" read. A spot
            // concentrates its lumens ~10x vs a point light — keep it modest
            // or every close-up wall is a white disc. Intensity is driven by
            // the Enclosure probe each frame (headlamp_adapt).
            parent.spawn((
                SpotLight {
                    color: Color::srgb(0.95, 0.98, 1.0),
                    intensity: 60_000.0,
                    range: 34.0,
                    inner_angle: 0.30,
                    outer_angle: 0.62,
                    shadows_enabled: false,
                    ..default()
                },
                Transform::IDENTITY,
                HeadLamp {
                    max: 480_000.0,
                    min: 40_000.0,
                },
            ));
            // ...plus a faint fill so the tunnel right at your feet isn't void.
            parent.spawn((
                PointLight {
                    color: Color::srgb(0.8, 0.88, 1.0),
                    intensity: 8_000.0,
                    range: 6.0,
                    shadows_enabled: false,
                    ..default()
                },
                Transform::IDENTITY,
                HeadLamp {
                    max: 40_000.0,
                    min: 6_000.0,
                },
            ));

            // First-person mining laser, built from primitives. All children
            // of one root so sway/recoil moves the whole tool.
            parent
                .spawn((
                    Transform::from_translation(VM_BASE),
                    Visibility::Visible,
                    ViewmodelRoot,
                ))
                .with_children(|tool| {
                    // Receiver body.
                    tool.spawn((
                        Mesh3d(meshes.add(Cuboid::new(0.09, 0.10, 0.34))),
                        MeshMaterial3d(metal.clone()),
                        Transform::IDENTITY,
                    ));
                    // Grip.
                    tool.spawn((
                        Mesh3d(meshes.add(Cuboid::new(0.035, 0.10, 0.05))),
                        MeshMaterial3d(metal.clone()),
                        Transform::from_xyz(0.0, -0.09, 0.10),
                    ));
                    // Barrel.
                    tool.spawn((
                        Mesh3d(meshes.add(Cuboid::new(0.05, 0.05, 0.24))),
                        MeshMaterial3d(accent.clone()),
                        Transform::from_xyz(0.0, 0.015, -0.27),
                    ));
                    // Cooling fins.
                    for side in [-1.0f32, 1.0] {
                        tool.spawn((
                            Mesh3d(meshes.add(Cuboid::new(0.012, 0.07, 0.16))),
                            MeshMaterial3d(accent.clone()),
                            Transform::from_xyz(side * 0.056, 0.04, -0.10),
                        ));
                    }
                    // Emitter tip — glows with heat.
                    tool.spawn((
                        Mesh3d(meshes.add(Cuboid::new(0.055, 0.055, 0.05))),
                        MeshMaterial3d(emitter),
                        Transform::from_xyz(0.0, 0.015, -0.42),
                    ));
                });
        });

    // Laser FX pool (world space, hidden until the trigger is held).
    let beam_core_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgb(3.0, 9.0, 10.0),
        unlit: true,
        ..default()
    });
    let beam_halo_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgba(0.4, 1.6, 2.0, 0.25),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        cull_mode: None,
        ..default()
    });
    let glow_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgb(2.5, 8.0, 9.0),
        unlit: true,
        ..default()
    });
    let unit = meshes.add(Cuboid::new(1.0, 1.0, 1.0));
    commands.spawn((
        Mesh3d(unit.clone()),
        MeshMaterial3d(beam_core_mat),
        Transform::IDENTITY,
        Visibility::Hidden,
        LaserFx::Core,
    ));
    commands.spawn((
        Mesh3d(unit),
        MeshMaterial3d(beam_halo_mat),
        Transform::IDENTITY,
        Visibility::Hidden,
        LaserFx::Halo,
    ));
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(0.07))),
        MeshMaterial3d(glow_mat),
        Transform::IDENTITY,
        Visibility::Hidden,
        LaserFx::Glow,
    ));
    commands.spawn((
        PointLight {
            color: Color::srgb(0.35, 0.9, 1.0),
            intensity: 0.0,
            range: 12.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::IDENTITY,
        Visibility::Hidden,
        LaserFx::Light,
        ImpactLightMark,
    ));
}

// ---------------------------------------------------------------------------
// Cursor + look
// ---------------------------------------------------------------------------

fn grab_cursor(
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut focused: ResMut<Focused>,
    mut cursors: Query<&mut CursorOptions>,
) {
    let Ok(mut cursor) = cursors.single_mut() else {
        return;
    };
    if !focused.0 && mouse.just_pressed(MouseButton::Left) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
        focused.0 = true;
    }
    if focused.0 && keys.just_pressed(KeyCode::Escape) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
        focused.0 = false;
    }
}

fn mouse_look(
    motion: Res<AccumulatedMouseMotion>,
    focused: Res<Focused>,
    mut player: ResMut<PlayerState>,
) {
    if !focused.0 || motion.delta == Vec2::ZERO {
        return;
    }
    player.yaw -= motion.delta.x * MOUSE_SENS;
    player.pitch =
        (player.pitch - motion.delta.y * MOUSE_SENS).clamp(-1.54, 1.54);
}

// ---------------------------------------------------------------------------
// Movement + collision
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn player_move(
    keys: Res<ButtonInput<KeyCode>>,
    focused: Res<Focused>,
    time: Res<Time>,
    world: Res<VoxelWorld>,
    mut player: ResMut<PlayerState>,
    mut jet: ResMut<JetState>,
    mut shake: ResMut<Shake>,
    mut sfx: ResMut<SfxQueue>,
    assets: Option<Res<ItemAssets>>,
    mut commands: Commands,
) {
    let dt = time.delta_secs().min(0.05); // clamp tunneling on hitches

    // Thrust wish vector. Grounded: flat walk axes for precise footwork.
    // Airborne: full 6DOF — W follows the look ray (pitch included).
    let mut wish = Vec3::ZERO;
    let mut vertical = 0.0f32;
    if focused.0 {
        let (fwd, right) = if player.grounded {
            let f = Quat::from_rotation_y(player.yaw) * Vec3::NEG_Z;
            let r = Quat::from_rotation_y(player.yaw) * Vec3::X;
            (f, r)
        } else {
            let f = player.look_dir();
            let r = Quat::from_rotation_y(player.yaw) * Vec3::X;
            (f, r)
        };
        if keys.pressed(KeyCode::KeyW) {
            wish += fwd;
        }
        if keys.pressed(KeyCode::KeyS) {
            wish -= fwd;
        }
        if keys.pressed(KeyCode::KeyD) {
            wish += right;
        }
        if keys.pressed(KeyCode::KeyA) {
            wish -= right;
        }
        if keys.pressed(KeyCode::Space) {
            vertical += 1.0;
        }
        if keys.pressed(KeyCode::KeyC) {
            vertical -= 1.0;
        }
    }
    let wish = wish.normalize_or_zero();
    let thrusting = wish != Vec3::ZERO || vertical != 0.0;
    let braking = focused.0 && keys.pressed(KeyCode::ShiftLeft);

    if player.grounded {
        // Planted: direct velocity control. No stick force — with vertical
        // velocity exactly zero, the 1mm ground probe keeps `grounded` set;
        // any constant downward push would leave us micro-hovering above the
        // snap plane and flicker the flag at high FPS.
        let walk = wish * WALK_SPEED;
        player.vel.x = walk.x;
        player.vel.z = walk.z;
        player.vel.y = if vertical > 0.0 { JUMP_VEL } else { 0.0 };
        jet.0 = false;
    } else {
        // Free flight: accelerate, brake, or drift.
        let accel = (wish + Vec3::Y * vertical).normalize_or_zero() * THRUST;
        player.vel += accel * dt;
        let damp = if braking { BRAKE_DAMP } else { IDLE_DAMP };
        if !thrusting || braking {
            let v = player.vel;
            player.vel = v * (-damp * dt).exp();
        }
        let speed = player.vel.length();
        if speed > MAX_SPEED {
            let v = player.vel;
            player.vel = v * (MAX_SPEED / speed);
        }
        jet.0 = thrusting;
    }
    let speed = player.vel.length();
    if speed > MAX_STEP_SPEED {
        let v = player.vel;
        player.vel = v * (MAX_STEP_SPEED / speed);
    }

    // Per-axis swept move, substepped so no axis crosses more than ~half a
    // voxel per step — otherwise a fast pass can tunnel through a wall and
    // the boundary snap embeds the player inside solid rock.
    let was_grounded = player.grounded;
    let impact_speed = player.vel.length();
    let mut pos = player.pos;
    let mut vel = player.vel;
    let max_disp = (vel * dt).abs().max_element();
    let steps = (max_disp / (VOXEL * 0.45)).ceil().max(1.0) as u32;
    let sub_dt = dt / steps as f32;
    let mut grounded = false;
    for _ in 0..steps {
        move_axis(&world, &mut pos, &mut vel, 0, sub_dt);
        move_axis(&world, &mut pos, &mut vel, 2, sub_dt);
        grounded |= move_axis(&world, &mut pos, &mut vel, 1, sub_dt);
    }
    player.grounded = grounded;
    player.pos = pos;
    player.vel = vel;

    // Touchdown feel: thud + dust + a touch of shake, scaled by impact.
    if grounded && !was_grounded && impact_speed > 3.0 {
        let k = ((impact_speed - 3.0) / 10.0).clamp(0.0, 1.0);
        shake.add(0.06 + 0.12 * k);
        sfx.push(SfxEvent::Land);
        if let Some(assets) = &assets {
            items::spawn_land_dust(&mut commands, assets, player.pos);
        }
    }

    // Track the farthest site for the [G] long-jump teleport.
    let range = player.range_m();
    if range > player.max_range + 0.5 {
        player.max_range = range;
        player.far_pos = player.pos;
    }
}

/// Brighten the helmet lamp as the sky disappears.
fn headlamp_adapt(
    enclosure: Res<Enclosure>,
    mut spots: Query<(&HeadLamp, Option<&mut SpotLight>, Option<&mut PointLight>)>,
) {
    for (lamp, spot, point) in &mut spots {
        let i = lamp.min + (lamp.max - lamp.min) * enclosure.0;
        if let Some(mut s) = spot {
            s.intensity = i;
        }
        if let Some(mut p) = point {
            p.intensity = i;
        }
    }
}

/// Probe how buried we are: an upward ray that hits rock soon means we're in
/// a tunnel and the sun shouldn't reach us. Smoothed to avoid light pops.
fn update_enclosure(
    time: Res<Time>,
    world: Res<VoxelWorld>,
    player: Res<PlayerState>,
    mut enclosure: ResMut<Enclosure>,
) {
    const PROBE_M: f32 = 12.0;
    let target = match raycast(&world, player.eye(), Vec3::Y, PROBE_M) {
        Some(hit) => (1.0 - hit.t / PROBE_M).clamp(0.0, 1.0).powf(0.5),
        None => 0.0,
    };
    let k = (time.delta_secs() * 4.0).min(1.0);
    enclosure.0 += (target - enclosure.0) * k;
}

/// Move along one axis and clamp against solid voxels. Returns true if we hit
/// a floor (axis 1, moving down) — used for the grounded check.
fn move_axis(world: &VoxelWorld, pos: &mut Vec3, vel: &mut Vec3, axis: usize, dt: f32) -> bool {
    let delta = vel[axis] * dt;
    if delta == 0.0 {
        // Still allow "standing on ground" detection by probing 1mm down.
        if axis == 1 {
            let mut probe = *pos;
            probe.y -= 0.001;
            return collides(world, probe);
        }
        return false;
    }
    pos[axis] += delta;
    if !collides(world, *pos) {
        return false;
    }
    // Collided: snap to the voxel boundary we crossed.
    let half = PLAYER_HALF[axis];
    let center_off = if axis == 1 { PLAYER_HALF.y } else { 0.0 };
    let center = pos[axis] + center_off;
    if delta > 0.0 {
        let face = ((center + half) / VOXEL).floor() * VOXEL;
        pos[axis] = face - half - center_off - 1e-4;
    } else {
        let face = ((center - half) / VOXEL).ceil() * VOXEL;
        pos[axis] = face + half - center_off + 1e-4;
    }
    let hit_floor = axis == 1 && delta < 0.0;
    vel[axis] = 0.0;
    hit_floor
}

/// Does the player AABB (feet at `pos`) overlap any solid voxel?
fn collides(world: &VoxelWorld, pos: Vec3) -> bool {
    let center = pos + Vec3::Y * PLAYER_HALF.y;
    let min = center - PLAYER_HALF;
    let max = center + PLAYER_HALF;
    let v_min = (min / VOXEL).floor().as_ivec3();
    let v_max = (max / VOXEL).floor().as_ivec3();
    for y in v_min.y..=v_max.y {
        for z in v_min.z..=v_max.z {
            for x in v_min.x..=v_max.x {
                if world.solid(IVec3::new(x, y, z)) {
                    return true;
                }
            }
        }
    }
    false
}

/// World point on the struck face, jittered slightly so rapid damage numbers
/// fan out instead of stacking on one pixel.
fn pop_point(v: IVec3, normal: IVec3) -> Vec3 {
    let center = (v.as_vec3() + Vec3::splat(0.5)) * VOXEL;
    let face = center + normal.as_vec3() * (VOXEL * 0.5);
    let j = ((v.x.wrapping_mul(7) ^ v.y.wrapping_mul(13) ^ v.z.wrapping_mul(17)) & 7) as f32;
    face + Vec3::new((j - 3.5) * 0.03, 0.05, (3.5 - j) * 0.03)
}

/// Continuous noise in [-1,1] for shake — cheap sin mix, no allocation.
fn wobble(t: f32, seed: f32) -> f32 {
    ((t * 37.7 + seed).sin() + 0.7 * (t * 23.3 + seed * 2.3).sin()) / 1.7
}

fn sync_camera(
    time: Res<Time>,
    player: Res<PlayerState>,
    mut shake: ResMut<Shake>,
    mut cameras: Query<&mut Transform, With<PlayerCamera>>,
) {
    shake.trauma = (shake.trauma - time.delta_secs() * 1.4).max(0.0);
    if let Ok(mut tf) = cameras.single_mut() {
        tf.translation = player.eye();
        let t = time.elapsed_secs();
        let s = shake.trauma * shake.trauma * 0.045;
        let shake_rot = Quat::from_euler(
            EulerRot::YXZ,
            wobble(t, 1.0) * s,
            wobble(t, 7.0) * s,
            wobble(t, 13.0) * s * 0.6,
        );
        tf.rotation = player.look_rot() * shake_rot;
    }
}

// ---------------------------------------------------------------------------
// Mining laser
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn mining(
    mouse: Res<ButtonInput<MouseButton>>,
    focused: Res<Focused>,
    time: Res<Time>,
    player: Res<PlayerState>,
    upgrades: Res<Upgrades>,
    mut world: ResMut<VoxelWorld>,
    mut target: ResMut<TargetInfo>,
    mut laser: ResMut<LaserState>,
    mut item_assets: ResMut<ItemAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut sfx: ResMut<SfxQueue>,
    mut shake: ResMut<Shake>,
    mut dmg_accum: Local<f32>,
    mut pop_timer: Local<f32>,
    mut spark_timer: Local<f32>,
    mut commands: Commands,
) {
    target.0 = None;
    let dt = time.delta_secs().min(0.05);

    // Heat machine: trigger → heat up; idle → cool; saturated → locked vent.
    let trigger = focused.0 && mouse.pressed(MouseButton::Left);
    if laser.locked {
        laser.firing = false;
        laser.heat -= upgrades.cool_rate() * VENT_BONUS * dt;
        if laser.heat <= HEAT_UNLOCK {
            laser.heat = laser.heat.max(0.0);
            laser.locked = false;
        }
    } else if trigger {
        laser.firing = true;
        laser.heat += upgrades.heat_rate() * dt;
        if laser.heat >= 1.0 {
            laser.heat = 1.0;
            laser.locked = true;
            laser.firing = false;
            sfx.push(SfxEvent::Overheat);
            sfx.push(SfxEvent::Vent);
        }
    } else {
        laser.firing = false;
        laser.heat = (laser.heat - upgrades.cool_rate() * dt).max(0.0);
    }

    let eye = player.eye();
    let dir = player.look_dir();
    // VM_MUZZLE is camera-relative; rotate it into the world.
    laser.beam_start = eye + player.look_rot() * VM_MUZZLE;

    let hit = raycast(&world, eye, dir, REACH);
    let Some(hit) = hit else {
        // Firing into space: the beam still draws out to max reach.
        laser.has_hit = false;
        laser.beam_end = eye + dir * REACH;
        *dmg_accum = 0.0;
        *pop_timer = 0.0;
        return;
    };

    let v = hit.voxel;
    let id = world.block(v);
    if id == AIR {
        laser.has_hit = false;
        laser.beam_end = eye + dir * REACH;
        return;
    }

    laser.has_hit = true;
    laser.beam_end = eye + dir * (hit.t - 0.01).max(0.1);

    let tier = tier_of_voxel(v);
    let hp_max = block_hp(id, tier);
    let is_ore = id == ORE;
    let value = if is_ore { world::ore_value(tier) } else { 0 };

    let mut remaining = *world.damage.get(&v).unwrap_or(&hp_max);
    if laser.firing && id != BARRIER {
        let damage = upgrades.dps() * dt;
        remaining -= damage;
        *dmg_accum += damage;

        // Impact sparks while the beam chews.
        *spark_timer -= dt;
        if *spark_timer <= 0.0 {
            *spark_timer = 0.07;
            items::spawn_sparks(
                &mut commands,
                &item_assets,
                laser.beam_end,
                2,
                true,
                (time.elapsed_secs() * 997.0) as i32,
            );
        }

        // Damage numbers tick out in batches so they're readable.
        *pop_timer += dt;
        let killed = remaining <= 0.0;
        if killed {
            crate::hud::spawn_damage_number(
                &mut commands,
                pop_point(v, hit.normal),
                hp_max,
                true,
            );
            world.set_air(v);
            items::spawn_drop(&mut commands, &mut item_assets, &mut materials, v, id);
            items::spawn_block_debris(&mut commands, &item_assets, v);
            items::spawn_sparks(&mut commands, &item_assets, laser.beam_end, 6, !is_ore, v.x);
            // Crystals ring, rock crunches — deeper rock lands heavier.
            sfx.push(SfxEvent::Break {
                crystal: is_ore,
                pitch: if id == REGOLITH {
                    1.15
                } else {
                    (1.0 - tier as f32 * 0.05).max(0.7)
                },
            });
            shake.add(0.05);
            *dmg_accum = 0.0;
            *pop_timer = 0.0;
            target.0 = None;
            return;
        }
        if *pop_timer >= 0.22 && *dmg_accum >= 1.0 {
            crate::hud::spawn_damage_number(
                &mut commands,
                pop_point(v, hit.normal),
                *dmg_accum,
                false,
            );
            *dmg_accum = 0.0;
            *pop_timer = 0.0;
        }
        world.damage.insert(v, remaining);
    } else {
        *dmg_accum = 0.0;
        *pop_timer = 0.0;
    }

    target.0 = Some(TargetBlock {
        name: block_display_name(id, tier),
        hp_frac: if hp_max.is_finite() {
            (remaining / hp_max).clamp(0.0, 1.0)
        } else {
            1.0
        },
        indestructible: id == BARRIER,
        value,
    });
}

/// Place the beam, halo, impact glow, and impact light to match LaserState.
fn laser_fx(
    laser: Res<LaserState>,
    time: Res<Time>,
    mut parts: Query<(&LaserFx, &mut Transform, &mut Visibility)>,
    mut lights: Query<&mut PointLight, With<ImpactLightMark>>,
) {
    let t = time.elapsed_secs();
    let visible = laser.firing;
    let start = laser.beam_start;
    let end = laser.beam_end;
    let len = (end - start).length().max(0.05);
    let mid = (start + end) * 0.5;
    // Subtle per-frame thickness flicker sells "energy", not "solid rod".
    let flicker = 1.0 + 0.18 * (t * 47.0).sin();

    for (kind, mut tf, mut vis) in &mut parts {
        *vis = if visible && (laser.has_hit || !matches!(kind, LaserFx::Glow | LaserFx::Light))
        {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
        match kind {
            LaserFx::Core => {
                *tf = Transform::from_translation(mid)
                    .looking_at(end, Vec3::Y)
                    .with_scale(Vec3::new(0.022 * flicker, 0.022 * flicker, len));
            }
            LaserFx::Halo => {
                *tf = Transform::from_translation(mid)
                    .looking_at(end, Vec3::Y)
                    .with_scale(Vec3::new(0.075 * flicker, 0.075 * flicker, len));
            }
            LaserFx::Glow => {
                *tf = Transform::from_translation(end)
                    .with_scale(Vec3::splat(1.0 + 0.3 * (t * 31.0).sin()));
            }
            LaserFx::Light => {
                tf.translation = end + (start - end).normalize_or_zero() * 0.2;
            }
        }
    }
    if let Ok(mut light) = lights.single_mut() {
        light.intensity = if visible && laser.has_hit {
            120_000.0 * (1.0 + 0.25 * (t * 53.0).sin())
        } else {
            0.0
        };
    }
}

/// Sway, recoil, and heat glow for the first-person tool.
fn viewmodel_update(
    time: Res<Time>,
    player: Res<PlayerState>,
    laser: Res<LaserState>,
    mats: Res<ViewmodelMats>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut roots: Query<&mut Transform, With<ViewmodelRoot>>,
    mut bob_phase: Local<f32>,
) {
    let dt = time.delta_secs();
    let t = time.elapsed_secs();
    let speed = Vec2::new(player.vel.x, player.vel.z).length();
    let moving = (speed / WALK_SPEED).clamp(0.0, 1.0);
    if player.grounded {
        *bob_phase += dt * 7.0 * moving.max(0.05);
    }

    if let Ok(mut tf) = roots.single_mut() {
        let bob = Vec3::new(
            (*bob_phase).sin() * 0.012 * moving,
            -((*bob_phase * 2.0).sin().abs()) * 0.009 * moving,
            0.0,
        );
        let recoil = if laser.firing {
            Vec3::new(
                wobble(t * 1.7, 3.0) * 0.0035,
                wobble(t * 1.9, 9.0) * 0.0035,
                0.012 + wobble(t * 2.3, 5.0) * 0.004,
            )
        } else {
            Vec3::ZERO
        };
        tf.translation = VM_BASE + bob + recoil;
        tf.rotation = Quat::from_rotation_z((*bob_phase).cos() * 0.01 * moving)
            * Quat::from_rotation_x(if laser.firing { 0.01 } else { 0.0 });
    }

    // Emitter glow: cyan at idle, searing white-hot at full heat, angry red
    // while locked.
    if let Some(mat) = materials.get_mut(&mats.emitter) {
        let h = laser.heat;
        mat.emissive = if laser.locked {
            let blink = 0.5 + 0.5 * (t * 12.0).sin();
            LinearRgba::rgb(6.0 * blink, 0.6, 0.4)
        } else {
            LinearRgba::rgb(0.2 + h * 5.0, 1.2 + h * 4.0, 1.4 + h * 3.0)
        };
    }
}
