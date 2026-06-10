//! First-person player: mouse look, WASD movement with voxel AABB collision,
//! and the mining beam (hold LMB, RPG-style damage-per-swing vs block HP).

use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions};

use crate::game::Upgrades;
use crate::items::{self, ItemAssets};
use crate::world::{
    self, block_display_name, block_hp, band_of_depth, depth_of, raycast, VoxelWorld, AIR,
    BARRIER, ORE, VOXEL, WORLD_VOXELS_XZ,
};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Player collision half-extents (so 0.6m wide, 1.4m tall — ~6 voxels).
const PLAYER_HALF: Vec3 = Vec3::new(0.30, 0.70, 0.30);
/// Eye height above the feet.
const EYE: f32 = 1.25;
const GRAVITY: f32 = -22.0;
/// Falling faster than this risks crossing a whole voxel in one substep.
const TERMINAL_FALL: f32 = 24.0;
/// Jump impulse: apex ≈ 1.1m, enough to hop a 4-voxel (1m) step, so you can
/// always staircase back out of your own mine before buying the recall device.
const JUMP_VEL: f32 = 7.0;
const WALK_SPEED: f32 = 4.5;
const MOUSE_SENS: f32 = 0.0023;
/// Mining reach in world units (14 voxels).
const REACH: f32 = 3.5;

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
    /// Deepest depth reached (m) and where we were standing when we reached it.
    pub max_depth: f32,
    pub deepest_pos: Vec3,
}

impl PlayerState {
    pub fn spawn_point() -> Vec3 {
        let c = WORLD_VOXELS_XZ as f32 * VOXEL * 0.5;
        Vec3::new(c, 0.0, c)
    }

    pub fn eye(&self) -> Vec3 {
        self.pos + Vec3::Y * EYE
    }

    pub fn look_dir(&self) -> Vec3 {
        Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, 0.0) * Vec3::NEG_Z
    }

    pub fn depth_m(&self) -> f32 {
        (-self.pos.y).max(0.0)
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
            max_depth: 0.0,
            deepest_pos: Self::spawn_point(),
        }
    }
}

/// True while the cursor is grabbed and gameplay input is live.
#[derive(Resource, Default)]
pub struct Focused(pub bool);

/// What the crosshair is pointing at, for the HUD.
#[derive(Resource, Default)]
pub struct TargetInfo(pub Option<TargetBlock>);

pub struct TargetBlock {
    pub name: String,
    /// Remaining HP fraction in [0,1].
    pub hp_frac: f32,
    pub indestructible: bool,
    /// Sale value if this is an ore, else 0.
    pub value: u64,
}

#[derive(Component)]
struct PlayerCamera;

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct PlayerPlugin;

impl Plugin for PlayerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PlayerState>()
            .init_resource::<Focused>()
            .init_resource::<TargetInfo>()
            .add_systems(Startup, setup_player)
            .add_systems(
                Update,
                (grab_cursor, mouse_look, player_move, sync_camera, mining).chain(),
            );
    }
}

fn setup_player(mut commands: Commands) {
    commands
        .spawn((
            Camera3d::default(),
            Projection::Perspective(PerspectiveProjection {
                fov: 1.22, // ~70°
                ..default()
            }),
            Transform::from_translation(PlayerState::spawn_point() + Vec3::Y * EYE),
            // Per-camera ambient light (0.18: AmbientLight is a component, not
            // a resource). depth_lighting in main.rs retunes it every frame.
            AmbientLight {
                color: Color::WHITE,
                brightness: 220.0,
                ..default()
            },
            PlayerCamera,
        ))
        .with_children(|parent| {
            // Headlamp: the primary light source once you're below the sunlit
            // surface. No shadows — it's a feel light, not a correctness light.
            parent.spawn((
                PointLight {
                    color: Color::srgb(1.0, 0.93, 0.75),
                    intensity: 1_500_000.0,
                    range: 22.0,
                    shadows_enabled: false,
                    ..default()
                },
                Transform::IDENTITY,
            ));
        });
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

fn player_move(
    keys: Res<ButtonInput<KeyCode>>,
    focused: Res<Focused>,
    time: Res<Time>,
    world: Res<VoxelWorld>,
    mut player: ResMut<PlayerState>,
) {
    let dt = time.delta_secs().min(0.05); // clamp tunneling on hitches
    let mut wish = Vec3::ZERO;
    if focused.0 {
        let fwd = Quat::from_rotation_y(player.yaw) * Vec3::NEG_Z;
        let right = Quat::from_rotation_y(player.yaw) * Vec3::X;
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
    }
    let wish = wish.normalize_or_zero() * WALK_SPEED;
    player.vel.x = wish.x;
    player.vel.z = wish.z;
    player.vel.y += GRAVITY * dt;
    if focused.0 && player.grounded && keys.pressed(KeyCode::Space) {
        player.vel.y = JUMP_VEL;
    }

    player.vel.y = player.vel.y.max(-TERMINAL_FALL);

    // Per-axis swept move, substepped so no axis crosses more than ~half a
    // voxel per step — otherwise a long fall can tunnel through a floor and
    // the boundary snap embeds the player inside solid ground.
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

    // Track deepest standable point for the [G] return teleport.
    if player.grounded {
        let depth = player.depth_m();
        if depth > player.max_depth + 0.01 {
            player.max_depth = depth;
            player.deepest_pos = player.pos;
        }
    }
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

fn sync_camera(
    player: Res<PlayerState>,
    mut cameras: Query<&mut Transform, With<PlayerCamera>>,
) {
    if let Ok(mut tf) = cameras.single_mut() {
        tf.translation = player.eye();
        tf.rotation = Quat::from_euler(EulerRot::YXZ, player.yaw, player.pitch, 0.0);
    }
}

// ---------------------------------------------------------------------------
// Mining
// ---------------------------------------------------------------------------

fn mining(
    mouse: Res<ButtonInput<MouseButton>>,
    focused: Res<Focused>,
    time: Res<Time>,
    player: Res<PlayerState>,
    upgrades: Res<Upgrades>,
    mut swing_accum: Local<f32>,
    mut world: ResMut<VoxelWorld>,
    mut target: ResMut<TargetInfo>,
    mut item_assets: ResMut<ItemAssets>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut commands: Commands,
) {
    target.0 = None;
    if !focused.0 {
        *swing_accum = 0.0;
        return;
    }

    let Some(hit) = raycast(&world, player.eye(), player.look_dir(), REACH) else {
        *swing_accum = 0.0;
        return;
    };
    let v = hit.voxel;
    let id = world.block(v);
    if id == AIR {
        *swing_accum = 0.0;
        return;
    }

    let depth = depth_of(v);
    let band = band_of_depth(depth);
    let hp_max = block_hp(id, depth);
    let is_ore = id == ORE;
    let value = if is_ore { world::ore_value(band) } else { 0 };

    // Accumulate swings while LMB is held; whole swings deal tool damage.
    let mining_held = mouse.pressed(MouseButton::Left);
    let mut damage = 0.0;
    if mining_held && id != BARRIER {
        *swing_accum += time.delta_secs() * upgrades.swings_per_sec();
        let swings = swing_accum.floor();
        *swing_accum -= swings;
        damage = swings * upgrades.damage();
    } else {
        *swing_accum = 0.0;
    }

    let mut remaining = *world.damage.get(&v).unwrap_or(&hp_max);
    if damage > 0.0 {
        remaining -= damage;
        if remaining <= 0.0 {
            world.set_air(v);
            // Loot is physical now: the block pops out as a drop you walk over
            // to collect (a full pack just leaves it lying there).
            items::spawn_drop(&mut commands, &mut item_assets, &mut materials, v, id);
            items::spawn_block_debris(&mut commands, &item_assets, v);
            target.0 = None;
            return;
        }
        world.damage.insert(v, remaining);
    }

    target.0 = Some(TargetBlock {
        name: block_display_name(id, depth),
        hp_frac: if hp_max.is_finite() {
            (remaining / hp_max).clamp(0.0, 1.0)
        } else {
            1.0
        },
        indestructible: id == BARRIER,
        value,
    });
}

