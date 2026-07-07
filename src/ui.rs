//! Terminal UI primitives — framed panels, section rules, tables, badges, bars.
//!
//! Everything adapts to the real terminal width (via `COLUMNS` / `tput`, with a
//! sane fallback) and degrades to plain text when output isn't a TTY. Colour is
//! handled by the `colored` crate, which already disables itself when piped.

use std::io::IsTerminal;

use colored::Colorize;

const MIN_W: usize = 50;
const MAX_W: usize = 88;

/// Real terminal width, clamped to a readable range.
pub fn cols() -> usize {
    if let Ok(c) = std::env::var("COLUMNS") {
        if let Ok(n) = c.trim().parse::<usize>() {
            return n.clamp(MIN_W, MAX_W);
        }
    }
    if std::io::stdout().is_terminal() {
        if let Ok(out) = std::process::Command::new("tput").arg("cols").output() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                if let Ok(n) = s.trim().parse::<usize>() {
                    return n.clamp(MIN_W, MAX_W);
                }
            }
        }
    }
    72
}

/// Visible length of a string, ignoring ANSI colour escapes.
pub fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut in_esc = false;
    for c in s.chars() {
        if in_esc {
            if c == 'm' {
                in_esc = false;
            }
        } else if c == '\x1b' {
            in_esc = true;
        } else {
            n += 1;
        }
    }
    n
}

/// Pad a string on the right with spaces to a visible width.
pub fn pad(s: &str, width: usize) -> String {
    let len = visible_len(s);
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

fn box_width() -> usize {
    cols().min(MAX_W)
}

/// A rounded, titled panel of label/value rows:
///
/// ```text
/// ╭─ title ───────────────────────╮
/// │  label   value                │
/// ╰───────────────────────────────╯
/// ```
pub fn panel(title: &str, rows: &[(String, String)]) {
    let bw = box_width();
    let inner = bw - 4; // "│ " + text + " │"
    let label_w = rows.iter().map(|(l, _)| visible_len(l)).max().unwrap_or(0);

    // Top border with embedded title.
    let title_seg = format!("─ {} ", title.bold());
    let used = visible_len(&title_seg);
    let dashes = (bw - 2).saturating_sub(used);
    println!(
        "{}",
        format!("╭{}{}╮", title_seg, "─".repeat(dashes)).cyan()
    );

    for (label, value) in rows {
        let cell = format!("{}  {}", pad(&label.dimmed().to_string(), label_w), value);
        println!("{} {} {}", "│".cyan(), pad(&cell, inner), "│".cyan());
    }

    println!("{}", format!("╰{}╯", "─".repeat(bw - 2)).cyan());
}

/// A section heading: blank line, bold title, and a rule to the right edge.
pub fn section(title: &str) {
    let w = box_width();
    let t = format!("{}", title.bold());
    let used = visible_len(&t) + 1;
    let dashes = w.saturating_sub(used);
    println!("\n{} {}", t, "─".repeat(dashes).dimmed());
}

/// A coloured status badge: `[ ok ]`, `[ !! ]`, `[ xx ]`.
pub fn badge(kind: Badge) -> String {
    match kind {
        Badge::Ok => format!("[{}]", " ok ".green()),
        Badge::Warn => format!("[{}]", " !! ".yellow()),
        Badge::Bad => format!("[{}]", " xx ".red()),
    }
}

#[derive(Clone, Copy)]
pub enum Badge {
    Ok,
    Warn,
    Bad,
}

/// A compact unicode meter, e.g. `████████░░░░ 63%`.
pub fn bar(fraction: f64, width: usize) -> String {
    let f = fraction.clamp(0.0, 1.0);
    let filled = (f * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    let color = if f > 0.85 {
        "red"
    } else if f > 0.6 {
        "yellow"
    } else {
        "green"
    };
    let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));
    format!("{} {:>3.0}%", bar.color(color), f * 100.0)
}

/// The v2 wordmark, for `v2 about` and help.
pub fn logo() -> String {
    let art = r#"
   ██╗   ██╗██████╗
   ██║   ██║╚════██╗
   ██║   ██║ █████╔╝
   ╚██╗ ██╔╝██╔═══╝
    ╚████╔╝ ███████╗
     ╚═══╝  ╚══════╝"#;
    art.cyan().to_string()
}

/// Program version, from Cargo — never hardcoded.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
