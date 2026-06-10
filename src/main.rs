use bevy::diagnostic::{
    EntityCountDiagnosticsPlugin, FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin,
};
use bevy::prelude::*;
use std::time::Duration;

mod audio;
mod demo;
mod game;
mod hud;
mod items;
mod player;
mod sky;
mod world;

use player::PlayerState;

/// Systems that read player input and mutate gameplay state. The demo driver
/// runs before this set so injected input is seen the same frame.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GameplaySet;

/// Hard vacuum: the sun switches off fast once rock swallows the sky.
const DAYLIGHT_FADE_M: f32 = 6.0;
const SUN_LUX: f32 = 8_000.0;
const AMBIENT_SURFACE: f32 = 60.0;
/// Ambient floor underground so unlit faces aren't pure black.
const AMBIENT_CAVE: f32 = 7.0;

fn main() {
    App::new()
        // Behind the skybox; effectively only visible for one frame at boot.
        .insert_resource(ClearColor(Color::linear_rgb(0.002, 0.003, 0.006)))
        .add_plugins((
            DefaultPlugins.set(WindowPlugin {
                primary_window: Some(Window {
                    title: "LIMITS — asteroid claim".into(),
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
            sky::SkyPlugin,
            audio::SoundPlugin,
            demo::DemoPlugin,
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
        // Shine from the sun's sky position toward the claim.
        Transform::default().looking_to(-sky::sun_direction(), Vec3::Y),
        Sun,
    ));
}

/// Fade sun and ambient toward darkness as the player descends. The helmet
/// lamp (child of the camera) becomes the dominant light underground; the
/// skybox stays — looking up a deep shaft shows stars, as it should.
fn depth_lighting(
    player: Res<PlayerState>,
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
