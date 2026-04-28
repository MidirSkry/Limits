use bevy::diagnostic::{Diagnostic, DiagnosticPath, Diagnostics, RegisterDiagnostic};
use bevy::prelude::*;
use std::time::Instant;

// Custom per-system timing diagnostics. LogDiagnosticsPlugin auto-prints these.
const DIAG_MOTION_MS: DiagnosticPath = DiagnosticPath::const_new("limits/motion_ms");
const DIAG_SYNC_MS: DiagnosticPath = DiagnosticPath::const_new("limits/sync_ms");

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

const ENTITY_COUNT_DEFAULT: u32 = 100_000;
const ENTITY_COUNT_MIN: u32 = 1;
const ENTITY_COUNT_MAX: u32 = 4_000_000;

const WORLD_HALF: f32 = 800.0;
const VEL_RANGE: f32 = 16.0;
const ATTRACTION: f32 = 12.0;

// Box size + click hit-test half-extent.
const BOX_SIZE: f32 = 80.0;
// Speed range for explosion debris. Wide range gives a "shockwave plus tail" look.
const EXPLOSION_SPEED_MIN: f32 = 30.0;
const EXPLOSION_SPEED_MAX: f32 = 120.0;

// Warrior_Run.png is a 1152x192 horizontal strip: 6 frames at 192x192. We render
// each warrior at WARRIOR_DRAW_SIZE pixels on screen — small enough that 100k of
// them spread cleanly across the viewport, large enough to read as a sprite.
const WARRIOR_FRAMES: u32 = 6;
const WARRIOR_FRAME_PIXELS: u32 = 192;
const WARRIOR_FPS: f32 = 10.0;
const WARRIOR_DRAW_SIZE: f32 = 24.0;

// ---------------------------------------------------------------------------
// Components — kept small and flat for cache-friendly par_iter
// ---------------------------------------------------------------------------

// Why a separate Position rather than reusing Transform: Transform is 40 bytes
// (Vec3 + Quat + Vec3). Hot motion math only needs 2D coordinates, so a Vec2
// component (8 bytes) lets par_iter stream ~5x more entities per cache line.
// A cheap sync system copies Position into Transform once per render frame.
#[derive(Component, Copy, Clone)]
struct Position(Vec2);

#[derive(Component, Copy, Clone)]
struct Velocity(Vec2);

#[derive(Component, Copy, Clone)]
struct Mass(f32);

#[derive(Component)]
struct SimEntity;

// Marker for the clickable spawn-source box. There is exactly one at startup; it
// despawns when clicked and the explosion replaces it.
#[derive(Component)]
struct ClickableBox;

// Loaded warrior assets, kept in a Resource so spawn_explosion can clone the
// handles cheaply (Handle is Arc-backed) instead of re-loading per spawn.
#[derive(Resource)]
struct WarriorAssets {
    image: Handle<Image>,
    layout: Handle<TextureAtlasLayout>,
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct SimulationPlugin;

impl Plugin for SimulationPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(SimState::new(ENTITY_COUNT_DEFAULT))
            .register_diagnostic(
                Diagnostic::new(DIAG_MOTION_MS)
                    .with_suffix("ms")
                    .with_max_history_length(120),
            )
            .register_diagnostic(
                Diagnostic::new(DIAG_SYNC_MS)
                    .with_suffix("ms")
                    .with_max_history_length(120),
            )
            .add_systems(Startup, (load_warrior_assets, spawn_initial).chain())
            // Physics in FixedUpdate (default 64 Hz) — decouples sim from render rate
            // so FPS reflects render+sync cost cleanly while motion stays deterministic.
            .add_systems(FixedUpdate, update_motion)
            // Input + click-explode + transform sync run every render frame.
            // tick_warrior_animation runs in parallel with the others (different data).
            .add_systems(
                Update,
                (
                    (handle_input, click_to_explode, sync_position_to_transform).chain(),
                    tick_warrior_animation,
                ),
            );
    }
}

// ---------------------------------------------------------------------------
// Resource — entity count, pause flag, and a tiny embedded RNG so we don't
// pull in the `rand` crate just for spawn-time scatter.
// ---------------------------------------------------------------------------

#[derive(Resource)]
pub struct SimState {
    pub entity_count: u32,
    pub paused: bool,
    rng_state: u64,
}

impl SimState {
    fn new(entity_count: u32) -> Self {
        Self {
            entity_count,
            paused: false,
            rng_state: 0xCAFE_BABE_DEAD_BEEF,
        }
    }

    // xorshift64 — 3 ops per number, plenty random for visual scatter.
    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        x
    }

    #[inline]
    fn rand_unit(&mut self) -> f32 {
        // Top 24 bits → [0, 1)
        ((self.next_u64() >> 40) as f32) / ((1u32 << 24) as f32)
    }

    #[inline]
    fn rand_range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + self.rand_unit() * (hi - lo)
    }
}

// ---------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------

fn load_warrior_assets(
    asset_server: Res<AssetServer>,
    mut layouts: ResMut<Assets<TextureAtlasLayout>>,
    mut commands: Commands,
) {
    let image: Handle<Image> = asset_server.load("Warrior_Run.png");
    // 6 columns × 1 row, no padding/offset.
    let layout = TextureAtlasLayout::from_grid(
        UVec2::splat(WARRIOR_FRAME_PIXELS),
        WARRIOR_FRAMES,
        1,
        None,
        None,
    );
    let layout = layouts.add(layout);
    commands.insert_resource(WarriorAssets { image, layout });
}

fn spawn_initial(mut commands: Commands, mut sim: ResMut<SimState>) {
    if let Ok(s) = std::env::var("LIMITS_COUNT") {
        if let Ok(n) = s.parse::<u32>() {
            sim.entity_count = n.clamp(ENTITY_COUNT_MIN, ENTITY_COUNT_MAX);
        }
    }
    // Spawn just the clickable source-box at origin. Particles only appear once the
    // user clicks it (or pressing R / +/- creates ambient ones via handle_input).
    commands.spawn((
        Sprite::from_color(Color::srgb(1.0, 0.55, 0.15), Vec2::splat(BOX_SIZE)),
        Transform::from_xyz(0.0, 0.0, 1.0),
        ClickableBox,
    ));
}

// Random-scatter spawn: positions across the world, low random velocities. Used by
// the +/- rescale and R reset paths in handle_input. Visual is the same warrior
// sprite as the explosion, just with different starting state.
fn spawn_entities(commands: &mut Commands, sim: &mut SimState, warrior: &WarriorAssets) {
    let count = sim.entity_count as usize;
    let mut batch: Vec<(Sprite, Transform, Position, Velocity, Mass, SimEntity)> =
        Vec::with_capacity(count);

    let draw_size = Vec2::splat(WARRIOR_DRAW_SIZE);

    for _ in 0..count {
        let x = sim.rand_range(-WORLD_HALF, WORLD_HALF);
        let y = sim.rand_range(-WORLD_HALF, WORLD_HALF);
        let vx = sim.rand_range(-VEL_RANGE, VEL_RANGE);
        let vy = sim.rand_range(-VEL_RANGE, VEL_RANGE);

        let sprite = Sprite {
            image: warrior.image.clone(),
            texture_atlas: Some(TextureAtlas {
                layout: warrior.layout.clone(),
                index: 0,
            }),
            custom_size: Some(draw_size),
            flip_x: vx < 0.0,
            ..default()
        };

        batch.push((
            sprite,
            Transform::from_xyz(x, y, 0.0),
            Position(Vec2::new(x, y)),
            Velocity(Vec2::new(vx, vy)),
            Mass(sim.rand_range(0.5, 2.0)),
            SimEntity,
        ));
    }

    commands.spawn_batch(batch);
}

// Spawn `count` warriors at `origin` with radial outward velocity (random angle,
// random speed in [EXPLOSION_SPEED_MIN, EXPLOSION_SPEED_MAX]). The existing
// FixedUpdate motion system applies central attraction afterwards, so the visual
// is "burst outward, slow, fall back, oscillate" rather than infinite linear flight.
//
// Each warrior shares the same image + atlas-layout handle (cheap Arc clones) and
// starts at frame 0; the global tick_warrior_animation system advances them.
fn spawn_explosion(
    commands: &mut Commands,
    sim: &mut SimState,
    warrior: &WarriorAssets,
    origin: Vec2,
    count: u32,
) {
    let count = count as usize;
    let mut batch: Vec<(Sprite, Transform, Position, Velocity, Mass, SimEntity)> =
        Vec::with_capacity(count);

    let draw_size = Vec2::splat(WARRIOR_DRAW_SIZE);

    for _ in 0..count {
        let angle = sim.rand_unit() * std::f32::consts::TAU;
        let speed = sim.rand_range(EXPLOSION_SPEED_MIN, EXPLOSION_SPEED_MAX);
        let vel = Vec2::new(angle.cos(), angle.sin()) * speed;

        // Sprite faces right by default; flip horizontally if the warrior is moving
        // left so the run direction matches its velocity.
        let flip_x = vel.x < 0.0;

        let sprite = Sprite {
            image: warrior.image.clone(),
            texture_atlas: Some(TextureAtlas {
                layout: warrior.layout.clone(),
                index: 0,
            }),
            custom_size: Some(draw_size),
            flip_x,
            ..default()
        };

        batch.push((
            sprite,
            Transform::from_xyz(origin.x, origin.y, 0.0),
            Position(origin),
            Velocity(vel),
            Mass(sim.rand_range(0.5, 2.0)),
            SimEntity,
        ));
    }

    commands.spawn_batch(batch);
}

// Detect a left-click on the spawn-source box and trigger the explosion. Hit-test
// is a cheap AABB against the box's Transform — the box never moves, so we don't
// need a real collider system.
fn click_to_explode(
    mut commands: Commands,
    mouse: Res<ButtonInput<MouseButton>>,
    windows: Query<&Window>,
    cameras: Query<(&Camera, &GlobalTransform)>,
    boxes: Query<(Entity, &Transform), With<ClickableBox>>,
    mut sim: ResMut<SimState>,
    warrior: Res<WarriorAssets>,
) {
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }
    let Ok(window) = windows.single() else { return };
    let Some(cursor) = window.cursor_position() else { return };
    let Ok((camera, cam_tf)) = cameras.single() else { return };
    let Ok(world) = camera.viewport_to_world_2d(cam_tf, cursor) else {
        return;
    };

    let half = BOX_SIZE * 0.5;
    for (entity, tf) in &boxes {
        let p = tf.translation.truncate();
        if (world.x - p.x).abs() <= half && (world.y - p.y).abs() <= half {
            commands.entity(entity).despawn();
            let n = sim.entity_count;
            spawn_explosion(&mut commands, &mut sim, &warrior, p, n);
            break;
        }
    }
}

// Single global animation clock — every SimEntity reads the same atlas index, so
// we tick one accumulator and broadcast it to all warriors. With 100k entities,
// per-entity timers would cost 100k * delta-add per frame; this costs one.
//
// We only write Sprite atlas index when the global frame actually advances (10 Hz),
// not every render frame, which keeps render-extract change detection happy at the
// 60+ Hz the renderer would otherwise tick at.
fn tick_warrior_animation(
    time: Res<Time>,
    mut accum: Local<f32>,
    mut frame: Local<u32>,
    mut sprites: Query<&mut Sprite, With<SimEntity>>,
) {
    *accum += time.delta_secs();
    let frame_dur = 1.0 / WARRIOR_FPS;
    let advanced = (*accum / frame_dur) as u32;
    if advanced == 0 {
        return;
    }
    *accum -= advanced as f32 * frame_dur;
    *frame = (*frame + advanced) % WARRIOR_FRAMES;
    let idx = *frame as usize;

    sprites.par_iter_mut().for_each(|mut sprite| {
        if let Some(atlas) = sprite.texture_atlas.as_mut() {
            atlas.index = idx;
        }
    });
}

// ---------------------------------------------------------------------------
// Motion — embarrassingly parallel via par_iter_mut
// ---------------------------------------------------------------------------

fn update_motion(
    sim: Res<SimState>,
    time: Res<Time<Fixed>>,
    mut query: Query<(&mut Position, &mut Velocity, &Mass)>,
    mut diagnostics: Diagnostics,
) {
    if sim.paused {
        return;
    }
    let start = Instant::now();
    let dt = time.delta_secs();
    let pull_k = ATTRACTION;

    query.par_iter_mut().for_each(|(mut pos, mut vel, mass)| {
        // Soft central attraction: a = -k * dir / max(|p|, eps), divided by mass.
        // The min-distance clamp avoids singular forces at the origin.
        let p = pos.0;
        let dist = p.length().max(1.0);
        let accel = (-p / dist) * (pull_k / mass.0);
        vel.0 += accel * dt;
        pos.0 = p + vel.0 * dt;
    });

    diagnostics.add_measurement(&DIAG_MOTION_MS, || {
        start.elapsed().as_secs_f64() * 1000.0
    });
}

fn sync_position_to_transform(
    mut query: Query<(&Position, &mut Transform)>,
    mut diagnostics: Diagnostics,
) {
    let start = Instant::now();
    query.par_iter_mut().for_each(|(pos, mut tf)| {
        tf.translation.x = pos.0.x;
        tf.translation.y = pos.0.y;
    });
    diagnostics.add_measurement(&DIAG_SYNC_MS, || {
        start.elapsed().as_secs_f64() * 1000.0
    });
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn handle_input(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    mut sim: ResMut<SimState>,
    existing: Query<Entity, With<SimEntity>>,
    warrior: Res<WarriorAssets>,
) {
    if keys.just_pressed(KeyCode::Space) {
        sim.paused = !sim.paused;
    }

    let mut new_count: Option<u32> = None;
    if keys.just_pressed(KeyCode::Equal) || keys.just_pressed(KeyCode::NumpadAdd) {
        new_count = Some(sim.entity_count.saturating_mul(2).min(ENTITY_COUNT_MAX));
    } else if keys.just_pressed(KeyCode::Minus) || keys.just_pressed(KeyCode::NumpadSubtract) {
        new_count = Some((sim.entity_count / 2).max(ENTITY_COUNT_MIN));
    }

    let reset = keys.just_pressed(KeyCode::KeyR);

    // Only respawn if there are already entities — pressing +/- before the box has
    // been clicked just updates the count for the eventual explosion.
    if (new_count.is_some() || reset) && !existing.is_empty() {
        for e in &existing {
            commands.entity(e).despawn();
        }
        if let Some(c) = new_count {
            sim.entity_count = c;
        }
        spawn_entities(&mut commands, &mut sim, &warrior);
    } else if let Some(c) = new_count {
        sim.entity_count = c;
    }
}
