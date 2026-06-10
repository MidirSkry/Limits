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

use player::Enclosure;

/// Systems that read player input and mutate gameplay state. The demo driver
/// runs before this set so injected input is seen the same frame.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GameplaySet;

/// Hard vacuum: the sun switches off fast once rock swallows the sky.
const SUN_LUX: f32 = 8_000.0;
/// Cool fill from the anti-sun side — physically it's starlight/planetshine,
/// practically it keeps shadow-side voxel faces from being void-black stripes
/// against space.
const FILL_LUX: f32 = 1_000.0;
const AMBIENT_SURFACE: f32 = 130.0;
/// Ambient floor in tunnels so unlit faces aren't pure black.
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

/// Marks a sky light; the wrapped value is its full-daylight illuminance.
#[derive(Component)]
struct Sun(f32);

fn setup_lights(mut commands: Commands) {
    commands.spawn((
        DirectionalLight {
            illuminance: SUN_LUX,
            // No shadow maps: the Enclosure probe carries the "inside rock"
            // read, and skipping shadows avoids cascade tuning over an
            // effectively unbounded world.
            shadows_enabled: false,
            ..default()
        },
        // Shine from the sun's sky position toward the origin.
        Transform::default().looking_to(-sky::sun_direction(), Vec3::Y),
        Sun(SUN_LUX),
    ));
    // Anti-sun fill, tilted so the two lights never zero out the same face.
    let fill_from = (-sky::sun_direction() + Vec3::new(0.2, 0.55, -0.25)).normalize();
    commands.spawn((
        DirectionalLight {
            illuminance: FILL_LUX,
            color: Color::srgb(0.55, 0.65, 1.0),
            shadows_enabled: false,
            ..default()
        },
        Transform::default().looking_to(-fill_from, Vec3::Y),
        Sun(FILL_LUX),
    ));
}

/// Fade sun and ambient as the player tunnels into rock (no shadow maps —
/// the Enclosure probe fakes occlusion). The helmet lamp becomes the
/// dominant light inside; the skybox stays, so a shaft mouth shows stars.
fn depth_lighting(
    enclosure: Res<Enclosure>,
    mut ambients: Query<&mut AmbientLight>,
    mut suns: Query<(&Sun, &mut DirectionalLight)>,
) {
    let daylight = 1.0 - enclosure.0;
    for mut ambient in &mut ambients {
        ambient.brightness = AMBIENT_CAVE + (AMBIENT_SURFACE - AMBIENT_CAVE) * daylight;
    }
    for (sun, mut light) in &mut suns {
        light.illuminance = sun.0 * daylight;
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
