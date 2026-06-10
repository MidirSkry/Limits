//! HUD + floating combat text.
//!
//! Styled panels (dark translucent, bordered, rounded) rather than raw text
//! dumps: a stats panel, a reticle with a target name + real HP bar, a shop
//! panel near the pad, a status toast, and a controls hint. Plus world-anchored
//! damage numbers that pop off blocks as you mine. A few dozen UI entities, so
//! per-frame `format!` is fine here.

use bevy::prelude::*;

use crate::game::{Inventory, NearShop, StatusMsg, Upgrades, Wallet, RECALL_COST};
use crate::player::{Focused, PlayerState, TargetInfo};
use crate::world::loot_name;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

const PANEL_BG: Color = Color::srgba(0.04, 0.05, 0.07, 0.82);
const PANEL_BORDER: Color = Color::srgba(0.45, 0.55, 0.70, 0.50);
const GOLD: Color = Color::srgb(1.0, 0.84, 0.30);
const TEXT_DIM: Color = Color::srgb(0.78, 0.82, 0.88);
const TEXT_FAINT: Color = Color::srgb(0.55, 0.60, 0.68);
const BAR_BG: Color = Color::srgba(0.0, 0.0, 0.0, 0.55);

// ---------------------------------------------------------------------------
// Markers
// ---------------------------------------------------------------------------

#[derive(Component)]
struct MoneyText;
#[derive(Component)]
struct StatsDetailText;
#[derive(Component)]
struct TargetPanel;
#[derive(Component)]
struct TargetNameText;
#[derive(Component)]
struct HpBarFill;
#[derive(Component)]
struct ShopPanel;
#[derive(Component)]
struct ShopBodyText;
#[derive(Component)]
struct StatusPanel;
#[derive(Component)]
struct StatusText;
#[derive(Component)]
struct ControlsText;

/// World-anchored floating damage number. Lives on a UI Text node; the animate
/// system projects `world_pos` to the screen each frame and fades it out.
#[derive(Component)]
struct DamageNumber {
    world_pos: Vec3,
    age: f32,
    ttl: f32,
}

pub struct HudPlugin;

impl Plugin for HudPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_hud).add_systems(
            Update,
            (
                hud_stats,
                hud_target,
                hud_shop,
                hud_status,
                hud_controls,
                animate_damage_numbers,
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
    // --- Stats panel (top-left) ---------------------------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(12.0),
                left: Val::Px(12.0),
                flex_direction: FlexDirection::Column,
                min_width: Val::Px(210.0),
                padding: UiRect::all(Val::Px(12.0)),
                border: UiRect::all(Val::Px(1.5)),
                row_gap: Val::Px(2.0),
                border_radius: BorderRadius::all(Val::Px(10.0)),
                ..default()
            },
            BackgroundColor(PANEL_BG),
            BorderColor::all(PANEL_BORDER),
        ))
        .with_children(|p| {
            p.spawn((Text::new("$0"), font(28.0), TextColor(GOLD), MoneyText));
            p.spawn((
                Text::new(""),
                font(15.0),
                TextColor(TEXT_DIM),
                StatsDetailText,
            ));
        });

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
                BorderColor::all(Color::srgba(1.0, 1.0, 1.0, 0.9)),
            ));
        });

    // --- Target panel (just below center) -----------------------------------
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Percent(57.0),
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
                        BackgroundColor(Color::srgb(0.3, 0.85, 0.35)),
                        HpBarFill,
                    ));
                });
            });
        });

    // --- Shop panel (right, vertically centered) ----------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Percent(50.0),
                right: Val::Px(16.0),
                margin: UiRect::top(Val::Px(-120.0)),
                flex_direction: FlexDirection::Column,
                width: Val::Px(330.0),
                padding: UiRect::all(Val::Px(14.0)),
                border: UiRect::all(Val::Px(1.5)),
                row_gap: Val::Px(6.0),
                border_radius: BorderRadius::all(Val::Px(10.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.06, 0.05, 0.02, 0.88)),
            BorderColor::all(GOLD),
            Visibility::Hidden,
            ShopPanel,
        ))
        .with_children(|p| {
            p.spawn((Text::new("⛏  SHOP"), font(20.0), TextColor(GOLD)));
            p.spawn((
                Text::new(""),
                font(15.0),
                TextColor(TEXT_DIM),
                ShopBodyText,
            ));
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
                BackgroundColor(Color::srgba(0.05, 0.06, 0.09, 0.9)),
                BorderColor::all(PANEL_BORDER),
                Visibility::Hidden,
                StatusPanel,
            ))
            .with_children(|t| {
                t.spawn((
                    Text::new(""),
                    font(17.0),
                    TextColor(Color::srgb(1.0, 0.82, 0.45)),
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

fn hud_stats(
    wallet: Res<Wallet>,
    inventory: Res<Inventory>,
    upgrades: Res<Upgrades>,
    player: Res<PlayerState>,
    mut money_q: Query<&mut Text, (With<MoneyText>, Without<StatsDetailText>)>,
    mut detail_q: Query<&mut Text, (With<StatsDetailText>, Without<MoneyText>)>,
) {
    if let Ok(mut money) = money_q.single_mut() {
        money.0 = format!("${}", commas(wallet.money));
    }
    let Ok(mut detail) = detail_q.single_mut() else {
        return;
    };
    let mut s = format!(
        "Depth {depth:.1} m   ·   best {best:.1} m\n\
         Pickaxe  {dmg:.0} dmg @ {spd:.2}/s\n\
         Carrying  {units} items  (${val})",
        depth = player.depth_m(),
        best = player.max_depth,
        dmg = upgrades.damage(),
        spd = upgrades.swings_per_sec(),
        units = inventory.units,
        val = commas(inventory.total_value()),
    );
    // A few most-valuable stacks, so the readout stays compact.
    for (&(id, band), &count) in inventory.stacks.iter().rev().take(5) {
        s.push_str(&format!("\n  {} ×{}", loot_name(id, band), count));
    }
    detail.0 = s;
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
            format!("{} — indestructible", t.name)
        } else if t.value > 0 {
            format!("{}   (${})", t.name, commas(t.value))
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
            // Green when healthy, red as it nears breaking.
            Color::srgb(1.0 - frac * 0.75, 0.25 + frac * 0.6, 0.2)
        };
    }
}

fn hud_shop(
    near: Res<NearShop>,
    wallet: Res<Wallet>,
    inventory: Res<Inventory>,
    upgrades: Res<Upgrades>,
    mut panel_q: Query<&mut Visibility, With<ShopPanel>>,
    mut body_q: Query<&mut Text, With<ShopBodyText>>,
) {
    let Ok(mut vis) = panel_q.single_mut() else {
        return;
    };
    if !near.0 {
        *vis = Visibility::Hidden;
        return;
    }
    *vis = Visibility::Visible;
    let Ok(mut body) = body_q.single_mut() else {
        return;
    };

    let tag = |cost: u64| -> &'static str {
        if wallet.money >= cost {
            ""
        } else {
            "  ✗"
        }
    };
    let recall_line = if upgrades.recall {
        "[3]  Recall device — OWNED".to_string()
    } else {
        format!("[3]  Recall device   ${}{}", commas(RECALL_COST), tag(RECALL_COST))
    };
    body.0 = format!(
        "[E]  Sell everything   +${sell}\n\
         [1]  Damage  {dmg:.0} → {dmg_next:.0}   ${dmg_cost}{t1}\n\
         [2]  Speed  {spd:.2} → {spd_next:.2}/s   ${spd_cost}{t2}\n\
         {recall_line}",
        sell = commas(inventory.total_value()),
        dmg = upgrades.damage(),
        dmg_next = upgrades.damage() * 1.5,
        dmg_cost = commas(upgrades.damage_cost()),
        t1 = tag(upgrades.damage_cost()),
        spd = upgrades.swings_per_sec(),
        spd_next = upgrades.swings_per_sec() * 1.12,
        spd_cost = commas(upgrades.swings_cost()),
        t2 = tag(upgrades.swings_cost()),
    );
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
        "Click to play".to_string()
    } else if upgrades.recall {
        "WASD move · Space jump · LMB mine · Q TNT · T surface · G dive · Esc release"
            .to_string()
    } else {
        "WASD move · Space jump · LMB mine · Q TNT · Esc release".to_string()
    };
}

// ---------------------------------------------------------------------------
// Floating damage numbers
// ---------------------------------------------------------------------------

/// Spawn a floating combat number at a world position. A killing blow is
/// bigger and gold; ordinary hits are small and white.
pub fn spawn_damage_number(commands: &mut Commands, world_pos: Vec3, amount: f32, killed: bool) {
    let (size, color, ttl) = if killed {
        (30.0, GOLD, 0.95)
    } else {
        (19.0, Color::srgb(1.0, 0.95, 0.85), 0.6)
    };
    commands.spawn((
        Text::new(format!("{}", amount.round().max(1.0) as i64)),
        font(size),
        TextColor(color),
        Node {
            position_type: PositionType::Absolute,
            ..default()
        },
        // Start off-screen; the animate system places it on the first frame.
        DamageNumber {
            world_pos,
            age: 0.0,
            ttl,
        },
    ));
}

fn animate_damage_numbers(
    time: Res<Time>,
    cameras: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut numbers: Query<(Entity, &mut DamageNumber, &mut Node, &mut TextColor)>,
    mut commands: Commands,
) {
    let Ok((camera, cam_tf)) = cameras.single() else {
        return;
    };
    let dt = time.delta_secs();
    for (entity, mut dn, mut node, mut color) in &mut numbers {
        dn.age += dt;
        let t = dn.age / dn.ttl;
        if t >= 1.0 {
            commands.entity(entity).despawn();
            continue;
        }
        // Drift upward in world space, then project to the screen.
        let world = dn.world_pos + Vec3::Y * (t * 0.7);
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
