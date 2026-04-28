# [Limits]

A Bevy 0.18 sandbox for high-entity-count simulation. Goal: prove the stack can move 100k+ independently-simulated entities at high frame rates and learn where the cliffs are.

## Pinned versions

- **Bevy: `0.18.1`** — when you bump this, re-read the migration guide before changing any spawn / scheduling / rendering code.
- Reference docs:
  - https://docs.rs/bevy/0.18.1/bevy/ — top-level module list and prelude
  - https://bevy.org/learn/quick-start/getting-started/setup/ — fast-compile recommendations
  - https://bevy.org/learn/migration-guides/0-17-to-0-18/ — what changed in 0.18

> **If you're about to write Bevy code in an area you haven't touched recently, fetch the relevant page from docs.rs first.** The model's training data lags the API. As of 0.18, bundles like `SpriteBundle` / `Camera2dBundle` / `TextBundle` are gone — `Sprite`, `Camera2d`, `Text` / `Text2d` are now plain components and use Required Components to auto-insert `Transform`, `Visibility`, etc. `FrameTimeDiagnosticsPlugin` is no longer a unit struct; use `::default()` or `::new(history_len)`.

## Conventions

- **ECS-first.** New behavior arrives as a system + components, not as a method on a god-resource.
- **Components small and flat.** A motion component should be a `Vec2` or `f32`, not a struct of structs. We deliberately use a separate `Position(Vec2)` rather than reusing `Transform` (40 bytes) to keep the hot motion loop cache-friendly.
- **Prefer `Query` over `ResMut`** wherever possible. Resources serialize systems; queries can be parallelized and scheduled independently.
- **No per-frame heap allocations in hot systems.** No `Vec::new()`, `format!`, `to_string()`, or `Box::new` inside a system that touches more than a handful of entities. Pre-allocate at startup, scratch via local resources.
- **Any system touching >1000 entities should use `par_iter` / `par_iter_mut`** unless there's a measured reason not to (e.g. write contention, ordering dependency).
- **Use `bevy_diagnostic` for observability** — never `println!` / `dbg!` in hot paths; they're synchronized I/O and will tank framerate on the way to telling you why your framerate is bad.

## Build cheat sheet

```sh
cargo run --features dev          # fast iteration (dynamic linking)
cargo run --release               # perf testing (statically linked, no dev feature)
cargo check --features dev        # type-check loop, should be sub-second on warm cache
```

The `dev` feature gates Bevy's `dynamic_linking`. Release builds intentionally don't enable it — shipping a dylib is awkward and it disables some optimizations.

The Windows linker config in `.cargo/config.toml` uses `rust-lld.exe` (bundled with rustup) for ~5–10x faster relinks vs the default MSVC linker.

### Toolchain on this machine (GNU, not MSVC)

`rust-toolchain.toml` pins `stable-x86_64-pc-windows-gnu` because the dev box doesn't have the Windows 10 SDK installed (so MSVC-ABI linking can't find `kernel32.lib` / `ws2_32.lib`). Two consequences:

1. **`PATH` must include the MSYS2 mingw-w64 binutils.** Rustup's bundled mingw subset is missing `dlltool`'s helpers (`ar`, `as`), which the bevy_dylib build needs when `dynamic_linking` is enabled. Add `C:\msys64\mingw64\bin` to your shell's `PATH`. Without it, `cargo build --features dev` fails with `dlltool ... CreateProcess`.
2. **Switching to MSVC.** If you ever install VS 2022 Build Tools + Win10 SDK, change `rust-toolchain.toml` to `stable-x86_64-pc-windows-msvc` and the existing `[target.x86_64-pc-windows-msvc]` block in `.cargo/config.toml` activates `rust-lld` automatically — Bevy's official recommended setup.

### AV gotcha — folder names with `[` `]`

Don't put this project inside a directory whose name contains square brackets. Both Windows Defender and AVG (and likely most AVs) treat `[` `]` as glob metacharacters in folder exclusions, so an exclusion for `D:\foo\[Bar]\` matches a character-class `B|a|r` rather than the literal folder. The linker then fails with `Permission denied` on every emitted `.exe`. Stick to plain ASCII letters/numbers/hyphens for any folder above `target/`.

## Performance discipline

1. **Profile before optimizing.** Cargo-flamegraph or Tracy. A guess about the bottleneck is wrong about half the time.
2. **Suspect allocation first.** A single `Vec::new()` inside a 100k-entity loop will eat your frame budget faster than any rendering inefficiency.
3. **Suspect the renderer last.** Bevy's batched sprite renderer is genuinely fast; if FPS is bad at 100k entities and your CPU profile shows render at 5%, the bug is in your sim, not the GPU.
4. **Measure, don't theorize.** "This system *should* be fine in serial" is a hypothesis. Confirm or refute it with the diagnostics plugin and a stopwatch.

## Deferred work — parking lot

Don't build these yet; note them so we don't forget.

- **Steam integration** — likely `bevy_steamworks` or raw `steamworks-rs`. Decision pending: which is more actively maintained against current Bevy.
- **Asset pipeline** — currently zero assets, just colored quads. When we add real sprites/audio, set up `AssetPlugin` paths and a hot-reload story.
- **Save system** — serde + bincode for component snapshots. Decide later: per-entity or chunked-archetype.

## Layout

```
src/
  main.rs    App setup, camera, HUD, optional bench-exit hook
  sim.rs     SimulationPlugin: components, spawn, motion, input
.cargo/
  config.toml      Windows fast-link config (rust-lld for MSVC ABI)
rust-toolchain.toml  Pins GNU ABI on this machine; see "Toolchain" above
```

## Bench hooks

Two env vars let you drive headless benchmarks without UI interaction:

- `LIMITS_COUNT=<n>` — initial entity count (overrides `ENTITY_COUNT_DEFAULT`).
- `LIMITS_BENCH_EXIT_AFTER=<seconds>` — process exits after that elapsed wall time.

`LogDiagnosticsPlugin` writes FPS / frame_time to stdout once per second, so:

```sh
LIMITS_COUNT=250000 LIMITS_BENCH_EXIT_AFTER=15 ./target/release/limits.exe > bench.log 2>&1
```

…gives you a clean log to scrape.
