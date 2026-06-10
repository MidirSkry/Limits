use bevy::diagnostic::{
    EntityCountDiagnosticsPlugin, FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin,
};
use bevy::prelude::*;
use std::time::Duration;

mod game;
mod hud;
mod items;
mod player;
mod world;

use player::PlayerState;

// Sky color at the surface; fades to near-black as you descend so the mine
// feels like a mine even though we do no real light occlusion.
const SKY_COLOR: Vec3 = Vec3::new(0.36, 0.58, 0.85);
const CAVE_COLOR: Vec3 = Vec3::new(0.01, 0.01, 0.015);
/// Depth (m) over which daylight fades out completely.
const DAYLIGHT_FADE_M: f32 = 8.0;
const SUN_LUX: f32 = 9_000.0;
const AMBIENT_SURFACE: f32 = 220.0;
/// Ambient floor underground so unlit faces aren't pure black.
const AMBIENT_CAVE: f32 = 25.0;

fn main() {
    App::new()
        .insert_resource(ClearColor(Color::linear_rgb(
            SKY_COLOR.x,
            SKY_COLOR.y,
            SKY_COLOR.z,
        )))
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "[Limits] — dig deeper".into(),
                    present_mode: bevy::window::PresentMode::AutoNoVsync,
                    // On wasm, attach to the <canvas id="bevy"> in index.html.
                    canvas: Some("#bevy".to_string()),
                    fit_canvas_to_parent: true,
                    ..default()
                }),
                ..default()
            }),
            FrameTimeDiagnosticsPlugin::default(),
            EntityCountDiagnosticsPlugin::default(),
            // Once-per-second FPS/frame_time/entity_count to stdout for headless
            // benches (see CLAUDE.md bench hooks).
            LogDiagnosticsPlugin {
                wait_duration: Duration::from_secs(1),
                ..default()
            },
            world::WorldPlugin,
            player::PlayerPlugin,
            items::ItemsPlugin,
            game::GamePlugin,
            hud::HudPlugin,
        ))
        .add_systems(Startup, setup_lights)
        .add_systems(Update, (depth_lighting, bench_auto_exit))
        .run();
}

#[derive(Component)]
struct Sun;

fn setup_lights(mut commands: Commands) {
    commands.spawn((
        DirectionalLight {
            illuminance: SUN_LUX,
            // No shadow maps: depth-based darkening below carries the "deep
            // underground" read, and skipping shadows avoids cascade tuning
            // for a shaft hundreds of meters tall.
            shadows_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -1.0, 0.5, 0.0)),
        Sun,
    ));
}

/// Fade sun, ambient, and sky toward darkness as the player descends. The
/// headlamp (child of the camera) becomes the dominant light underground.
fn depth_lighting(
    player: Res<PlayerState>,
    mut clear: ResMut<ClearColor>,
    mut ambients: Query<&mut AmbientLight>,
    mut suns: Query<&mut DirectionalLight, With<Sun>>,
) {
    let daylight = (1.0 - player.depth_m() / DAYLIGHT_FADE_M).clamp(0.0, 1.0);
    for mut ambient in &mut ambients {
        ambient.brightness = AMBIENT_CAVE + (AMBIENT_SURFACE - AMBIENT_CAVE) * daylight;
    }
    if let Ok(mut sun) = suns.single_mut() {
        sun.illuminance = SUN_LUX * daylight;
    }
    let sky = CAVE_COLOR.lerp(SKY_COLOR, daylight);
    clear.0 = Color::linear_rgb(sky.x, sky.y, sky.z);
}

// When LIMITS_BENCH_EXIT_AFTER=<seconds> is set, the process exits after that
// elapsed wall time. Bench escape hatch only. Disabled on wasm (no env/exit).
#[cfg(not(target_arch = "wasm32"))]
fn bench_auto_exit(time: Res<Time>) {
    static SECS: std::sync::OnceLock<Option<f32>> = std::sync::OnceLock::new();
    let limit = SECS.get_or_init(|| {
        std::env::var("LIMITS_BENCH_EXIT_AFTER")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
    });
    if let Some(t) = *limit {
        if time.elapsed_secs() >= t {
            std::process::exit(0);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn bench_auto_exit() {}
