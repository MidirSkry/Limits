//! HUD + floating combat text.
//!
//! Styled panels (dark translucent, bordered, rounded): a stats panel, a
//! reticle with a target name + HP bar, a laser heat bar that lives under the
//! crosshair, a depot panel near the pad, a status toast, and a controls
//! hint. Plus world-anchored float text (damage numbers, credit pops). A few
//! dozen UI entities, so per-frame `format!` is fine here.

use bevy::prelude::*;

use crate::game::{Inventory, NearShop, StatusMsg, Upgrades, Wallet, RECALL_COST};
use crate::player::{Focused, LaserState, PlayerState, TargetInfo};
use crate::world::loot_name;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

const PANEL_BG: Color = Color::srgba(0.03, 0.05, 0.08, 0.82);
const PANEL_BORDER: Color = Color::srgba(0.25, 0.65, 0.80, 0.50);
const CREDITS: Color = Color::srgb(0.55, 1.0, 0.85);
const CYAN: Color = Color::srgb(0.45, 0.9, 1.0);
const GOLD: Color = Color::srgb(1.0, 0.84, 0.30);
const TEXT_DIM: Color = Color::srgb(0.75, 0.84, 0.90);
const TEXT_FAINT: Color = Color::srgb(0.50, 0.60, 0.68);
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
struct HeatBar;
#[derive(Component)]
struct HeatBarFill;
#[derive(Component)]
struct HeatWarnText;
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
                hud_stats,
                hud_target,
                hud_heat,
                hud_shop,
                hud_status,
                hud_controls,
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
    // --- Stats panel (top-left) ---------------------------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(12.0),
                left: Val::Px(12.0),
                flex_direction: FlexDirection::Column,
                min_width: Val::Px(220.0),
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
            p.spawn((Text::new("0 cr"), font(28.0), TextColor(CREDITS), MoneyText));
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
                BorderColor::all(Color::srgba(0.7, 1.0, 1.0, 0.9)),
            ));
        });

    // --- Laser heat bar (just under the reticle) ----------------------------
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
                    width: Val::Px(120.0),
                    height: Val::Px(6.0),
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
        });

    // --- Target panel (below the heat bar) -----------------------------------
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
                        BackgroundColor(Color::srgb(0.3, 0.85, 0.95)),
                        HpBarFill,
                    ));
                });
            });
        });

    // --- Depot panel (right, vertically centered) ----------------------------
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                top: Val::Percent(50.0),
                right: Val::Px(16.0),
                margin: UiRect::top(Val::Px(-140.0)),
                flex_direction: FlexDirection::Column,
                width: Val::Px(350.0),
                padding: UiRect::all(Val::Px(14.0)),
                border: UiRect::all(Val::Px(1.5)),
                row_gap: Val::Px(6.0),
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
        money.0 = format!("{} cr", commas(wallet.credits));
    }
    let Ok(mut detail) = detail_q.single_mut() else {
        return;
    };
    let mut s = format!(
        "Depth {depth:.1} m   ·   best {best:.1} m\n\
         Laser  {dps:.0} DPS   ·   charges ×{charges}\n\
         Hold  {units} units  ({val} cr)",
        depth = player.depth_m(),
        best = player.max_depth,
        dps = upgrades.dps(),
        charges = upgrades.charges,
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

fn hud_heat(
    laser: Res<LaserState>,
    mut bar_q: Query<&mut Visibility, (With<HeatBar>, Without<HeatWarnText>)>,
    mut fill_q: Query<(&mut Node, &mut BackgroundColor), With<HeatBarFill>>,
    mut warn_q: Query<&mut Visibility, (With<HeatWarnText>, Without<HeatBar>)>,
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
        if wallet.credits >= cost {
            ""
        } else {
            "  ✗"
        }
    };
    let recall_line = if upgrades.recall {
        "[4]  Recall rig — OWNED".to_string()
    } else {
        format!(
            "[4]  Recall rig   {}{}",
            commas(RECALL_COST),
            tag(RECALL_COST)
        )
    };
    body.0 = format!(
        "[E]  Sell hold   +{sell} cr\n\
         [1]  Laser output  {dps:.0} → {dps_next:.0} DPS   {p_cost}{t1}\n\
         [2]  Coolant loop  lv{cool}   {c_cost}{t2}\n\
         [3]  Tractor field  {mag:.1} → {mag_next:.1} m   {tr_cost}{t3}\n\
         {recall_line}\n\
         [5]  Plasma charges ×{pack}   {pl_cost}{t5}   (held: {held})",
        sell = commas(inventory.total_value()),
        dps = upgrades.dps(),
        dps_next = upgrades.dps() * 1.5,
        p_cost = commas(upgrades.power_cost()),
        t1 = tag(upgrades.power_cost()),
        cool = upgrades.coolant_lvl + 1,
        c_cost = commas(upgrades.coolant_cost()),
        t2 = tag(upgrades.coolant_cost()),
        mag = upgrades.magnet_range(),
        mag_next = upgrades.magnet_range() + 0.8,
        tr_cost = commas(upgrades.tractor_cost()),
        t3 = tag(upgrades.tractor_cost()),
        pack = crate::game::PLASMA_PACK_SIZE,
        pl_cost = commas(upgrades.plasma_cost()),
        t5 = tag(upgrades.plasma_cost()),
        held = upgrades.charges,
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
        "Click to take the controls".to_string()
    } else if upgrades.recall {
        "WASD move · Space jump/jetpack · LMB laser · Q plasma charge · T surface · G dive · Esc release"
            .to_string()
    } else {
        "WASD move · Space jump/jetpack · LMB laser · Q plasma charge · Esc release"
            .to_string()
    };
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
