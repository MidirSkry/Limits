use bevy::diagnostic::{
    DiagnosticsStore, EntityCountDiagnosticsPlugin, FrameTimeDiagnosticsPlugin,
    LogDiagnosticsPlugin,
};
use bevy::input::mouse::AccumulatedMouseScroll;
use bevy::prelude::*;
use std::time::Duration;

// Camera zoom limits, expressed as orthographic `scale` (world units per screen unit).
// 1.0 = default. Smaller = zoomed in, larger = zoomed out.
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 20.0;
// Per-scroll-tick exponent. Multiplicative so each tick gives the same perceptual step
// regardless of current zoom level.
const ZOOM_STEP: f32 = 0.15;

mod sim;
use sim::{SimState, SimulationPlugin};

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "[Limits] — Bevy 0.18 stress test".into(),
                        present_mode: bevy::window::PresentMode::AutoNoVsync,
                        ..default()
                    }),
                    ..default()
                })
                // Use the existing Images/ directory as the asset root rather than the
                // conventional assets/. Keeps the user's chosen layout intact.
                .set(AssetPlugin {
                    file_path: "Images".to_string(),
                    ..default()
                }),
            FrameTimeDiagnosticsPlugin::default(),
            EntityCountDiagnosticsPlugin::default(),
            // Logs all registered diagnostics (FPS, frame_time, entity_count, plus our
            // custom motion/sync timers from SimulationPlugin) once per second so
            // headless benches can capture numbers without screen-scraping the HUD.
            LogDiagnosticsPlugin {
                wait_duration: Duration::from_secs(1),
                ..default()
            },
            SimulationPlugin,
        ))
        .add_systems(Startup, setup)
        .add_systems(Update, (update_hud, bench_auto_exit, zoom_camera))
        .run();
}

#[derive(Component)]
struct HudText;

fn setup(mut commands: Commands) {
    commands.spawn(Camera2d);

    commands.spawn((
        Text::new("FPS: --"),
        TextFont {
            font_size: 14.0,
            ..default()
        },
        TextColor(Color::WHITE),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(8.0),
            left: Val::Px(8.0),
            ..default()
        },
        HudText,
    ));
}

// Mouse-wheel zoom on the 2D camera's orthographic projection. Multiplicative so each
// scroll tick feels like the same step regardless of current zoom.
fn zoom_camera(
    scroll: Res<AccumulatedMouseScroll>,
    mut cameras: Query<&mut Projection, With<Camera2d>>,
) {
    if scroll.delta.y == 0.0 {
        return;
    }
    let factor = (-scroll.delta.y * ZOOM_STEP).exp();
    for mut projection in &mut cameras {
        if let Projection::Orthographic(ortho) = projection.as_mut() {
            ortho.scale = (ortho.scale * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        }
    }
}

// When LIMITS_BENCH_EXIT_AFTER=<seconds> is set, the process exits after that elapsed
// wall time. Bypasses Bevy's messaging system on purpose — it's just a bench escape
// hatch, not part of normal app lifecycle.
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

fn update_hud(
    diagnostics: Res<DiagnosticsStore>,
    sim: Res<SimState>,
    mut q: Query<&mut Text, With<HudText>>,
) {
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    let frame_time_ms = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FRAME_TIME)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);

    if let Ok(mut text) = q.single_mut() {
        let state = if sim.paused { "PAUSED" } else { "RUNNING" };
        text.0 = format!(
            "FPS:      {fps:>6.1}\n\
             Frame:    {frame_time_ms:>6.2} ms\n\
             Entities: {count}\n\
             State:    {state}\n\
             \n\
             [+/-]  scale entity count   [Space] pause   [R] reset",
            count = sim.entity_count,
        );
    }
}
