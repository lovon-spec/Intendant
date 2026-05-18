//! D-1 trace tool: poll X11 XDamage on a target display, partition
//! into tiles via inline math mirroring [`TileGrid`], emit per-tick
//! stats. Run on the X11 peer to verify the damage backend produces
//! non-trivial output during an `xdotool` sweep.
//!
//! Usage:
//!
//! ```sh
//! # On the X11 peer:
//! cargo run --release --example damage-trace
//! cargo run --release --example damage-trace -- --display :0 --tile-size 64 --interval-ms 33
//! ```
//!
//! The example is deliberately self-contained: it duplicates the small
//! tile-partition math from `display::tile::grid` rather than depending
//! on the binary crate's modules. The library is unit-tested already
//! (`cargo test display::tile::*`); this example exists to verify that
//! XDamage actually reports events on the X11 peer.
//!
//! Strict non-goals (per D-1 scope): no datachannels, no encoder, no
//! browser, no integration with the existing capture pipeline. This
//! binary opens its own X11 connection to observe damage events
//! independently of the production capture path.
//!
//! ## Cursor-only motion does not produce XDamage events
//!
//! Verified on the D-1 smoke peer: a 10-second `xdotool mousemove`
//! sweep produced zero `DamageNotify` events because the X server
//! renders the pointer as a hardware-cursor overlay and the
//! underlying framebuffer doesn't change. This is the X11 quirk that
//! [`super::super::tile::synthetic_dirty::SyntheticDirtySources`]
//! exists to bridge — it injects synthetic dirty rects around the
//! cursor on every observed move so the tile path sees cursor
//! freshness even when XDamage doesn't. Don't re-discover this case
//! as a backend bug; it's expected and addressed in D-3 integration
//! (where capture wires the synthetic source alongside the OS damage).

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("damage-trace is X11/Linux only — D-1 scope. Other platforms get None backend.");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};
    use x11rb::connection::{Connection, RequestConnection};
    use x11rb::protocol::damage::{self, ConnectionExt as DamageExt, ReportLevel};
    use x11rb::protocol::xfixes::{self, ConnectionExt as XfixesExt};
    use x11rb::protocol::Event;

    let mut display: String = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".to_string());
    let mut tile_size_px: u32 = 64;
    let mut interval_ms: u64 = 33;
    let mut duration_secs: u64 = 60;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--display" => {
                display = args.get(i + 1).cloned().unwrap_or(display);
                i += 2;
            }
            "--tile-size" => {
                tile_size_px = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(tile_size_px);
                i += 2;
            }
            "--interval-ms" => {
                interval_ms = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(interval_ms);
                i += 2;
            }
            "--duration" => {
                duration_secs = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(duration_secs);
                i += 2;
            }
            "-h" | "--help" => {
                eprintln!("usage: damage-trace [--display :N] [--tile-size N] [--interval-ms N] [--duration SECS]");
                eprintln!(
                    "defaults: DISPLAY={} tile_size=64 interval_ms=33 duration=60",
                    display
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }

    eprintln!(
        "damage-trace: display={} tile_size={} interval_ms={} duration={}s",
        display, tile_size_px, interval_ms, duration_secs
    );

    // Connect.
    let (conn, screen_num) = match x11rb::connect(Some(&display)) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("  fatal: cannot connect to {display}: {e}");
            std::process::exit(3);
        }
    };

    // Capability check — same logic as X11DamageBackend::new (the
    // explicit-degradation path the user wants).
    let damage_present = conn
        .extension_information(damage::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some();
    let xfixes_present = conn
        .extension_information(xfixes::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some();

    let setup = conn.setup();
    let screen = &setup.roots[screen_num];
    let (sw, sh) = (
        screen.width_in_pixels as u32,
        screen.height_in_pixels as u32,
    );

    if !damage_present || !xfixes_present {
        let missing = if !damage_present { "DAMAGE" } else { "XFIXES" };
        eprintln!("  capability=None  X11 extension '{missing}' missing — explicit degradation");
        eprintln!("  geometry={sw}x{sh}");
        eprintln!("  No further work; example exiting since there's nothing to trace.");
        std::process::exit(0);
    }

    // Negotiate versions. The .and_then(|c| c.reply()) idiom doesn't
    // type-check because the cookie's send error and reply error are
    // distinct types in x11rb; explicit match is cleaner here.
    match conn.damage_query_version(1, 1) {
        Err(e) => {
            eprintln!("  fatal: damage_query_version send: {e}");
            std::process::exit(4);
        }
        Ok(c) => {
            if let Err(e) = c.reply() {
                eprintln!("  fatal: damage_query_version reply: {e}");
                std::process::exit(4);
            }
        }
    }
    match conn.xfixes_query_version(5, 0) {
        Err(e) => {
            eprintln!("  fatal: xfixes_query_version send: {e}");
            std::process::exit(4);
        }
        Ok(c) => {
            if let Err(e) = c.reply() {
                eprintln!("  fatal: xfixes_query_version reply: {e}");
                std::process::exit(4);
            }
        }
    }

    let root = screen.root;
    let damage_id = match conn.generate_id() {
        Ok(id) => id,
        Err(e) => {
            eprintln!("  fatal: generate_id: {e}");
            std::process::exit(4);
        }
    };
    match conn.damage_create(damage_id, root, ReportLevel::BOUNDING_BOX) {
        Err(e) => {
            eprintln!("  fatal: damage_create send: {e}");
            std::process::exit(4);
        }
        Ok(c) => {
            if let Err(e) = c.check() {
                eprintln!("  fatal: damage_create check: {e}");
                std::process::exit(4);
            }
        }
    }
    let _ = conn.flush();

    let total_w_tiles = (sw + tile_size_px - 1) / tile_size_px;
    let total_h_tiles = (sh + tile_size_px - 1) / tile_size_px;
    let total_tiles = (total_w_tiles * total_h_tiles) as usize;

    eprintln!("  capability=OsLevel  geometry={sw}x{sh}");
    eprintln!(
        "  grid: {}x{} tiles ({} total) @ {}px",
        total_w_tiles, total_h_tiles, total_tiles, tile_size_px
    );
    eprintln!();
    eprintln!("ts_ms\tdirty_rects\tdirty_tiles\tdirty_fraction");

    let start = Instant::now();
    let interval = Duration::from_millis(interval_ms);
    let deadline = start + Duration::from_secs(duration_secs);

    while Instant::now() < deadline {
        let tick_start = Instant::now();

        // Drain queued events.
        let mut rects: Vec<(i32, i32, u32, u32)> = Vec::new();
        loop {
            match conn.poll_for_event() {
                Ok(Some(Event::DamageNotify(ev))) => {
                    let r = (
                        ev.area.x as i32,
                        ev.area.y as i32,
                        ev.area.width as u32,
                        ev.area.height as u32,
                    );
                    if r.2 != 0 && r.3 != 0 {
                        rects.push(r);
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("poll error: {e}");
                    break;
                }
            }
        }

        // Acknowledge damage so we get future events.
        if !rects.is_empty() {
            if let Err(e) = conn.damage_subtract(damage_id, 0u32, 0u32) {
                eprintln!("subtract error: {e}");
            } else {
                let _ = conn.flush();
            }
        }

        // Partition into tiles (mirrors TileGrid::dirty_tiles math).
        let mut tiles: BTreeSet<(u16, u16)> = BTreeSet::new();
        for (x, y, w, h) in &rects {
            let x0 = (*x as i64).max(0);
            let y0 = (*y as i64).max(0);
            let x1 = ((*x as i64) + *w as i64).min(sw as i64);
            let y1 = ((*y as i64) + *h as i64).min(sh as i64);
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            let ts = tile_size_px as i64;
            let tx0 = (x0 / ts) as u16;
            let ty0 = (y0 / ts) as u16;
            let tx1 = ((x1 - 1) / ts) as u16;
            let ty1 = ((y1 - 1) / ts) as u16;
            for ty in ty0..=ty1 {
                for tx in tx0..=tx1 {
                    tiles.insert((tx, ty));
                }
            }
        }

        let frac = if total_tiles == 0 {
            0.0
        } else {
            (tiles.len() as f32 / total_tiles as f32).clamp(0.0, 1.0)
        };

        println!(
            "{}\t{}\t{}\t{:.4}",
            tick_start.duration_since(start).as_millis(),
            rects.len(),
            tiles.len(),
            frac,
        );

        let elapsed = tick_start.elapsed();
        if elapsed < interval {
            std::thread::sleep(interval - elapsed);
        }
    }

    eprintln!("damage-trace: done after {:?}", start.elapsed());
}
