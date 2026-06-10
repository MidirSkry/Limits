//! HUD + floating combat text.
//!
//! Cockpit-style layout: a slim status bar across the top (credits · range /
//! sector · speed), a reticle with heat bar + charge pips under it, a target
//! panel, a per-row depot panel with affordability coloring, a status toast,
//! a controls hint, and a screen-edge nav marker pointing home to the depot.
//! Plus world-anchored float text (damage numbers, credit pops). A few dozen
//! UI entities, so per-frame `format!` is fine here.

use bevy::prelude::*;

use crate::game::{shop_pos, Inventory, NearShop, StatusMsg, Upgrades, Wallet, RECALL_COST};
use crate::player::{Focused, LaserState, PlayerState, TargetInfo};
use crate::world::{loot_name, TIER_M};

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

const PANEL_BG: Color = Color::srgba(0.03, 0.05, 0.08, 0.80);
const PANEL_BORDER: Color = Color::srgba(0.25, 0.65, 0.80, 0.45);
const CREDITS: Color = Color::srgb(0.55, 1.0, 0.85);
const CYAN: Color = Color::srgb(0.45, 0.9, 1.0);
const GOLD: Color = Color::srgb(1.0, 0.84, 0.30);
const TEXT_DIM: Color = Color::srgb(0.75, 0.84, 0.90);
const TEXT_FAINT: Color = Color::srgb(0.50, 0.60, 0.68);
const BAR_BG: Color = Color::srgba(0.0, 0.0, 0.0, 0.55);
const AFFORD: Color = Color::srgb(0.55, 1.0, 0.75);
const TOO_RICH: Color = Color::srgb(0.55, 0.58, 0.64);

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

#[derive(Component)]
struct CreditsText;
#[derive(Component)]
struct RangeText;
#[derive(Component)]
struct SpeedText;
#[derive(Component)]
struct HoldText;
#[derive(Component)]
struct TargetPanel;
#[derive(Component)]
struct TargetNameText;
#[derive(Component)]
struct HpBarFill;
#[derive(Component)]
struct HeatBar;
#[derive(Component)]
struct HeatBarFill;
#[derive(Component)]
struct HeatWarnText;
#[derive(Component)]
struct ChargePips;
#[derive(Component)]
struct ShopPanel;
/// One purchasable line in the depot panel; index keys the updater.
#[derive(Component)]
struct ShopRow(usize);
#[derive(Component)]
struct StatusPanel;
#[derive(Component)]
struct StatusText;
#[derive(Component)]
struct ControlsText;
#[derive(Component)]
struct NavMarker;

/// World-anchored floating text. Lives on a UI Text node; the animate system
/// projects `world_pos` to the screen each frame and fades it out.
#[derive(Component)]
struct FloatText {
    world_pos: Vec3,
    age: f32,
    ttl: f32,
    rise: f32,
}

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_hud).add_systems(
            Update,
            (
                hud_top_bar,
                hud_target,
                hud_heat,
                hud_shop,
                hud_status,
                hud_controls,
                hud_nav_marker,
                animate_float_text,
            ),
        );
    }
}

#[inline]
fn font(size: f32) -> TextFont {
    TextFont {
        font_size: size,
        ..default()
    }
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

fn setup_hud(mut commands: Commands) {
    // --- Top status bar -------------------------------------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(10.0),
                left: Val::Px(10.0),
                right: Val::Px(10.0),
                padding: UiRect::axes(Val::Px(16.0), Val::Px(8.0)),
                border: UiRect::all(Val::Px(1.5)),
                justify_content: JustifyContent::SpaceBetween,
                align_items: AlignItems::Center,
                column_gap: Val::Px(18.0),
                border_radius: BorderRadius::all(Val::Px(10.0)),
                ..default()
            },
            BackgroundColor(PANEL_BG),
            BorderColor::all(PANEL_BORDER),
        ))
        .with_children(|bar| {
            bar.spawn((Text::new("0 cr"), font(24.0), TextColor(CREDITS), CreditsText));
            bar.spawn((
                Text::new(""),
                font(16.0),
                TextColor(TEXT_DIM),
                RangeText,
            ));
            bar.spawn((Text::new(""), font(16.0), TextColor(CYAN), SpeedText));
        });

    // --- Hold summary (bottom-left, above controls) ---------------------------
    commands.spawn((
        Text::new(""),
        font(14.0),
        TextColor(TEXT_DIM),
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(34.0),
            left: Val::Px(12.0),
            ..default()
        },
        HoldText,
    ));

    // --- Crosshair reticle (true center) ------------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|p| {
            p.spawn((
                Node {
                    width: Val::Px(10.0),
                    height: Val::Px(10.0),
                    border: UiRect::all(Val::Px(2.0)),
                    border_radius: BorderRadius::all(Val::Px(5.0)),
                    ..default()
                },
                BorderColor::all(Color::srgba(0.7, 1.0, 1.0, 0.9)),
            ));
        });

    // --- Heat bar + charge pips (just under the reticle) ----------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Percent(52.5),
            width: Val::Percent(100.0),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            row_gap: Val::Px(4.0),
            ..default()
        })
        .with_children(|col| {
            col.spawn((
                Node {
                    width: Val::Px(140.0),
                    height: Val::Px(7.0),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(3.0)),
                    ..default()
                },
                BackgroundColor(BAR_BG),
                BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.6)),
                Visibility::Hidden,
                HeatBar,
            ))
            .with_children(|bar| {
                bar.spawn((
                    Node {
                        width: Val::Percent(0.0),
                        height: Val::Percent(100.0),
                        border_radius: BorderRadius::all(Val::Px(2.0)),
                        ..default()
                    },
                    BackgroundColor(CYAN),
                    HeatBarFill,
                ));
            });
            col.spawn((
                Text::new("OVERHEAT — VENTING"),
                font(13.0),
                TextColor(Color::srgb(1.0, 0.35, 0.25)),
                Visibility::Hidden,
                HeatWarnText,
            ));
            col.spawn((
                Text::new(""),
                font(13.0),
                TextColor(Color::srgb(0.9, 0.4, 1.0)),
                ChargePips,
            ));
        });

    // --- Target panel (below the heat cluster) --------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Percent(58.5),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(Val::Px(12.0), Val::Px(7.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    row_gap: Val::Px(5.0),
                    border_radius: BorderRadius::all(Val::Px(8.0)),
                    ..default()
                },
                BackgroundColor(PANEL_BG),
                BorderColor::all(PANEL_BORDER),
                Visibility::Hidden,
                TargetPanel,
            ))
            .with_children(|t| {
                t.spawn((
                    Text::new(""),
                    font(16.0),
                    TextColor(Color::WHITE),
                    TargetNameText,
                ));
                // HP bar: fixed track with a width-driven fill.
                t.spawn((
                    Node {
                        width: Val::Px(190.0),
                        height: Val::Px(12.0),
                        border: UiRect::all(Val::Px(1.0)),
                        border_radius: BorderRadius::all(Val::Px(3.0)),
                        ..default()
                    },
                    BackgroundColor(BAR_BG),
                    BorderColor::all(Color::srgba(0.0, 0.0, 0.0, 0.7)),
                ))
                .with_children(|bar| {
                    bar.spawn((
                        Node {
                            width: Val::Percent(100.0),
                            height: Val::Percent(100.0),
                            border_radius: BorderRadius::all(Val::Px(2.0)),
                            ..default()
                        },
                        BackgroundColor(Color::srgb(0.3, 0.85, 0.95)),
                        HpBarFill,
                    ));
                });
            });
        });

    // --- Depot panel (right, vertically centered, one row per item) -----------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Percent(50.0),
                right: Val::Px(16.0),
                margin: UiRect::top(Val::Px(-150.0)),
                flex_direction: FlexDirection::Column,
                width: Val::Px(370.0),
                padding: UiRect::all(Val::Px(14.0)),
                border: UiRect::all(Val::Px(1.5)),
                row_gap: Val::Px(7.0),
                border_radius: BorderRadius::all(Val::Px(10.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.06, 0.08, 0.88)),
            BorderColor::all(CYAN),
            Visibility::Hidden,
            ShopPanel,
        ))
        .with_children(|p| {
            p.spawn((Text::new("◇ SUPPLY DEPOT"), font(20.0), TextColor(CYAN)));
            for i in 0..6 {
                p.spawn((Text::new(""), font(15.0), TextColor(TEXT_DIM), ShopRow(i)));
            }
        });

    // --- Status toast (bottom-center) ---------------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(64.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                Node {
                    padding: UiRect::axes(Val::Px(16.0), Val::Px(8.0)),
                    border: UiRect::all(Val::Px(1.0)),
                    border_radius: BorderRadius::all(Val::Px(16.0)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.03, 0.06, 0.09, 0.9)),
                BorderColor::all(PANEL_BORDER),
                Visibility::Hidden,
                StatusPanel,
            ))
            .with_children(|t| {
                t.spawn((
                    Text::new(""),
                    font(17.0),
                    TextColor(Color::srgb(0.75, 0.95, 1.0)),
                    StatusText,
                ));
            });
        });

    // --- Controls hint (bottom-left) ----------------------------------------
    commands.spawn((
        Text::new(""),
        font(13.0),
        TextColor(TEXT_FAINT),
        Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(10.0),
            left: Val::Px(12.0),
            ..default()
        },
        ControlsText,
    ));

    // --- Depot nav marker (screen-space, repositioned every frame) ------------
    commands.spawn((
        Text::new("⌂"),
        font(20.0),
        TextColor(Color::srgba(0.45, 0.9, 1.0, 0.85)),
        Node {
            position_type: PositionType::Absolute,
            ..default()
        },
        Visibility::Hidden,
        NavMarker,
    ));
}

// ---------------------------------------------------------------------------
// Update systems
// ---------------------------------------------------------------------------

/// Group a number with thousands separators: 12345 -> "12,345".
fn commas(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn roman(n: i32) -> String {
    const R: [&str; 10] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X"];
    if (1..=10).contains(&n) {
        R[(n - 1) as usize].to_string()
    } else {
        n.to_string()
    }
}

#[allow(clippy::type_complexity)]
fn hud_top_bar(
    wallet: Res<Wallet>,
    inventory: Res<Inventory>,
    player: Res<PlayerState>,
    mut q: ParamSet<(
        Query<&mut Text, With<CreditsText>>,
        Query<&mut Text, With<RangeText>>,
        Query<&mut Text, With<SpeedText>>,
        Query<&mut Text, With<HoldText>>,
    )>,
) {
    if let Ok(mut t) = q.p0().single_mut() {
        t.0 = format!("{} cr", commas(wallet.credits));
    }
    if let Ok(mut t) = q.p1().single_mut() {
        let sector = (player.range_m() / TIER_M) as i32 + 1;
        t.0 = format!(
            "RANGE {:.0} m   ·   BEST {:.0} m   ·   SECTOR {}",
            player.range_m(),
            player.max_range,
            roman(sector),
        );
    }
    if let Ok(mut t) = q.p2().single_mut() {
        t.0 = format!("{:5.1} m/s", player.vel.length());
    }
    if let Ok(mut t) = q.p3().single_mut() {
        let mut s = format!(
            "HOLD  {} units  ({} cr)",
            inventory.units,
            commas(inventory.total_value())
        );
        for (&(id, tier), &count) in inventory.stacks.iter().rev().take(3) {
            s.push_str(&format!("   ·  {} ×{}", loot_name(id, tier), count));
        }
        t.0 = s;
    }
}

fn hud_target(
    target: Res<TargetInfo>,
    mut panel_q: Query<&mut Visibility, With<TargetPanel>>,
    mut name_q: Query<&mut Text, With<TargetNameText>>,
    mut fill_q: Query<(&mut Node, &mut BackgroundColor), With<HpBarFill>>,
) {
    let Ok(mut vis) = panel_q.single_mut() else {
        return;
    };
    let Some(t) = &target.0 else {
        *vis = Visibility::Hidden;
        return;
    };
    *vis = Visibility::Visible;

    if let Ok(mut name) = name_q.single_mut() {
        name.0 = if t.indestructible {
            format!("{} — impervious", t.name)
        } else if t.value > 0 {
            format!("{}   ({} cr)", t.name, commas(t.value))
        } else {
            t.name.clone()
        };
    }
    if let Ok((mut node, mut color)) = fill_q.single_mut() {
        let frac = if t.indestructible { 1.0 } else { t.hp_frac };
        node.width = Val::Percent(frac * 100.0);
        color.0 = if t.indestructible {
            Color::srgb(0.5, 0.55, 0.62)
        } else {
            // Cool cyan when healthy, white-hot as it nears breaking.
            Color::srgb(0.3 + (1.0 - frac) * 0.7, 0.85, 0.95)
        };
    }
}

#[allow(clippy::type_complexity)]
fn hud_heat(
    laser: Res<LaserState>,
    upgrades: Res<Upgrades>,
    mut bar_q: Query<&mut Visibility, (With<HeatBar>, Without<HeatWarnText>)>,
    mut fill_q: Query<(&mut Node, &mut BackgroundColor), With<HeatBarFill>>,
    mut warn_q: Query<&mut Visibility, (With<HeatWarnText>, Without<HeatBar>)>,
    mut pips_q: Query<&mut Text, With<ChargePips>>,
) {
    if let Ok(mut vis) = bar_q.single_mut() {
        *vis = if laser.heat > 0.02 {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }
    if let Ok((mut node, mut color)) = fill_q.single_mut() {
        node.width = Val::Percent(laser.heat * 100.0);
        // Cyan -> amber -> red as heat builds.
        let h = laser.heat;
        color.0 = if laser.locked {
            Color::srgb(1.0, 0.3, 0.2)
        } else if h < 0.6 {
            Color::srgb(0.45 + h * 0.5, 0.9, 1.0 - h * 0.4)
        } else {
            Color::srgb(1.0, 0.9 - (h - 0.6) * 1.5, 0.3)
        };
    }
    if let Ok(mut vis) = warn_q.single_mut() {
        *vis = if laser.locked {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }
    if let Ok(mut pips) = pips_q.single_mut() {
        let n = upgrades.charges.min(10) as usize;
        let mut s = String::with_capacity(24);
        for _ in 0..n {
            s.push('◆');
        }
        if upgrades.charges > 10 {
            s.push_str(&format!(" +{}", upgrades.charges - 10));
        }
        pips.0 = s;
    }
}

fn hud_shop(
    near: Res<NearShop>,
    wallet: Res<Wallet>,
    inventory: Res<Inventory>,
    upgrades: Res<Upgrades>,
    mut panel_q: Query<&mut Visibility, With<ShopPanel>>,
    mut rows: Query<(&ShopRow, &mut Text, &mut TextColor)>,
) {
    let Ok(mut vis) = panel_q.single_mut() else {
        return;
    };
    if !near.0 {
        *vis = Visibility::Hidden;
        return;
    }
    *vis = Visibility::Visible;

    for (row, mut text, mut color) in &mut rows {
        let (line, affordable) = match row.0 {
            0 => (
                format!("[E]  Sell hold    +{} cr", commas(inventory.total_value())),
                inventory.units > 0,
            ),
            1 => {
                let cost = upgrades.power_cost();
                (
                    format!(
                        "[1]  Laser output   {:.0} → {:.0} DPS    {} cr",
                        upgrades.dps(),
                        upgrades.dps() * 1.5,
                        commas(cost)
                    ),
                    wallet.credits >= cost,
                )
            }
            2 => {
                let cost = upgrades.coolant_cost();
                (
                    format!(
                        "[2]  Coolant loop   lv{}    {} cr",
                        upgrades.coolant_lvl + 1,
                        commas(cost)
                    ),
                    wallet.credits >= cost,
                )
            }
            3 => {
                let cost = upgrades.tractor_cost();
                (
                    format!(
                        "[3]  Tractor field   {:.1} → {:.1} m    {} cr",
                        upgrades.magnet_range(),
                        upgrades.magnet_range() + 0.8,
                        commas(cost)
                    ),
                    wallet.credits >= cost,
                )
            }
            4 => {
                if upgrades.recall {
                    ("[4]  Recall rig — OWNED".to_string(), false)
                } else {
                    (
                        format!("[4]  Recall rig    {} cr", commas(RECALL_COST)),
                        wallet.credits >= RECALL_COST,
                    )
                }
            }
            _ => {
                let cost = upgrades.plasma_cost();
                (
                    format!(
                        "[5]  Plasma charges ×{}    {} cr    (held {})",
                        crate::game::PLASMA_PACK_SIZE,
                        commas(cost),
                        upgrades.charges
                    ),
                    wallet.credits >= cost,
                )
            }
        };
        text.0 = line;
        color.0 = if affordable { AFFORD } else { TOO_RICH };
    }
}

fn hud_status(
    status: Res<StatusMsg>,
    mut panel_q: Query<&mut Visibility, With<StatusPanel>>,
    mut text_q: Query<&mut Text, With<StatusText>>,
) {
    let Ok(mut vis) = panel_q.single_mut() else {
        return;
    };
    if status.ttl > 0.0 {
        *vis = Visibility::Visible;
        if let Ok(mut text) = text_q.single_mut() {
            text.0.clone_from(&status.text);
        }
    } else {
        *vis = Visibility::Hidden;
    }
}

fn hud_controls(
    focused: Res<Focused>,
    upgrades: Res<Upgrades>,
    mut q: Query<&mut Text, With<ControlsText>>,
) {
    let Ok(mut text) = q.single_mut() else { return };
    text.0 = if !focused.0 {
        "Click to take the controls".to_string()
    } else if upgrades.recall {
        "WASD thrust · Space up · C down · Shift brake · LMB laser · Q charge · T home · G far site · Esc release"
            .to_string()
    } else {
        "WASD thrust · Space up · C down · Shift brake · LMB laser · Q charge · Esc release"
            .to_string()
    };
}

/// Point home: a ⌂ marker pinned to the depot, clamped to the screen edge
/// when it's off-camera. Hidden once you're close enough to see the pad.
fn hud_nav_marker(
    player: Res<PlayerState>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut markers: Query<(&mut Node, &mut Visibility, &mut Text), With<NavMarker>>,
) {
    let Ok((camera, cam_tf)) = cameras.single() else {
        return;
    };
    let Ok((mut node, mut vis, mut text)) = markers.single_mut() else {
        return;
    };
    let target = shop_pos() + Vec3::Y * 1.5;
    let dist = player.pos.distance(target);
    if dist < 12.0 {
        *vis = Visibility::Hidden;
        return;
    }
    *vis = Visibility::Visible;
    text.0 = format!("⌂ {:.0}m", dist);

    let Some(view_size) = camera.logical_viewport_size() else {
        return;
    };
    let center = view_size * 0.5;
    let margin = 56.0;

    match camera.world_to_viewport(cam_tf, target) {
        Ok(screen) => {
            // On screen (or near it): clamp into the visible frame.
            node.left = Val::Px(screen.x.clamp(margin, view_size.x - margin));
            node.top = Val::Px(screen.y.clamp(margin, view_size.y - margin));
        }
        Err(_) => {
            // Behind the camera: project the direction into view space and
            // pin the marker to the screen edge it's closest to.
            let local = cam_tf.affine().inverse().transform_point3(target);
            let dir2 = Vec2::new(local.x, -local.y).normalize_or_zero();
            let pos = center + dir2 * (center.min_element() - margin);
            node.left = Val::Px(pos.x.clamp(margin, view_size.x - margin));
            node.top = Val::Px(pos.y.clamp(margin, view_size.y - margin));
        }
    }
}

// ---------------------------------------------------------------------------
// Floating world-anchored text
// ---------------------------------------------------------------------------

/// Spawn floating text at a world position (damage numbers, credit pops, ...).
pub fn spawn_float_text(
    commands: &mut Commands,
    world_pos: Vec3,
    text: String,
    color: Color,
    size: f32,
    ttl: f32,
) {
    commands.spawn((
        Text::new(text),
        font(size),
        TextColor(color),
        Node {
            position_type: PositionType::Absolute,
            ..default()
        },
        // Starts off-screen; the animate system places it on the first frame.
        FloatText {
            world_pos,
            age: 0.0,
            ttl,
            rise: 0.7,
        },
    ));
}

/// Damage number: a killing blow is bigger and gold; ticks are small + white.
pub fn spawn_damage_number(commands: &mut Commands, world_pos: Vec3, amount: f32, killed: bool) {
    let (size, color, ttl) = if killed {
        (30.0, GOLD, 0.95)
    } else {
        (19.0, Color::srgb(0.9, 1.0, 1.0), 0.6)
    };
    spawn_float_text(
        commands,
        world_pos,
        format!("{}", amount.round().max(1.0) as i64),
        color,
        size,
        ttl,
    );
}

fn animate_float_text(
    time: Res<Time>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut texts: Query<(Entity, &mut FloatText, &mut Node, &mut TextColor)>,
    mut commands: Commands,
) {
    let Ok((camera, cam_tf)) = cameras.single() else {
        return;
    };
    let dt = time.delta_secs();
    for (entity, mut ft, mut node, mut color) in &mut texts {
        ft.age += dt;
        let t = ft.age / ft.ttl;
        if t >= 1.0 {
            commands.entity(entity).despawn();
            continue;
        }
        // Drift upward in world space, then project to the screen.
        let world = ft.world_pos + Vec3::Y * (t * ft.rise);
        match camera.world_to_viewport(cam_tf, world) {
            Ok(screen) => {
                node.left = Val::Px(screen.x);
                node.top = Val::Px(screen.y);
                // Ease-out fade; full opacity for the first third of life.
                let alpha = (1.0 - (t - 0.33).max(0.0) / 0.67).clamp(0.0, 1.0);
                color.0 = color.0.with_alpha(alpha);
            }
            // Behind the camera or off-target: hide this frame.
            Err(_) => color.0 = color.0.with_alpha(0.0),
        }
    }
}
