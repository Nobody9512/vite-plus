//! Shared Vite+ header rendering.
//!
//! Header coloring behavior:
//! - Colorization and truecolor capability gates
//! - Foreground color OSC query (`ESC ] 10 ; ? ESC \\`) with timeout
//! - ANSI palette queries for blue/magenta with timeout
//! - DA1 sandwich technique to detect unsupported terminals
//! - Gradient/fade generation and RGB ANSI coloring

use std::{
    io::IsTerminal,
    sync::{LazyLock, OnceLock},
};
#[cfg(unix)]
use std::{
    io::Write,
    time::{Duration, Instant},
};

use supports_color::{Stream, on};

#[cfg(unix)]
const ESC: &str = "\x1b";
const CSI: &str = "\x1b[";
const RESET: &str = "\x1b[0m";

const HEADER_SUFFIX: &str = " - The Unified Toolchain for the Web";

const RESET_FG: &str = "\x1b[39m";
const DEFAULT_BLUE: Rgb = Rgb(88, 146, 255);
const DEFAULT_MAGENTA: Rgb = Rgb(187, 116, 247);
const ANSI_BLUE_INDEX: u8 = 4;
const ANSI_MAGENTA_INDEX: u8 = 5;
const HEADER_SUFFIX_FADE_GAMMA: f64 = 1.35;

static HEADER_COLORS: OnceLock<HeaderColors> = OnceLock::new();

/// Whether the terminal is Warp, which does not respond to OSC color queries
/// and renders alternate screen content flush against block edges.
#[must_use]
pub fn is_warp_terminal() -> bool {
    static IS_WARP: LazyLock<bool> =
        LazyLock::new(|| std::env::var("TERM_PROGRAM").as_deref() == Ok("WarpTerminal"));
    *IS_WARP
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

struct HeaderColors {
    blue: Rgb,
    suffix_gradient: Vec<Rgb>,
}

fn bold(text: &str, enabled: bool) -> String {
    if enabled { format!("\x1b[1m{text}\x1b[22m") } else { text.to_string() }
}

fn fg_rgb(color: Rgb) -> String {
    format!("{CSI}38;2;{};{};{}m", color.0, color.1, color.2)
}

fn should_colorize() -> bool {
    let stdout = std::io::stdout();
    stdout.is_terminal() && on(Stream::Stdout).is_some()
}

fn supports_true_color() -> bool {
    let stdout = std::io::stdout();
    stdout.is_terminal() && on(Stream::Stdout).is_some_and(|color| color.has_16m)
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn gradient_eased(count: usize, start: Rgb, end: Rgb, gamma: f64) -> Vec<Rgb> {
    let n = count.max(1);
    let denom = (n - 1).max(1) as f64;

    (0..n)
        .map(|i| {
            let t = (i as f64 / denom).powf(gamma);
            Rgb(
                lerp(start.0 as f64, end.0 as f64, t).round() as u8,
                lerp(start.1 as f64, end.1 as f64, t).round() as u8,
                lerp(start.2 as f64, end.2 as f64, t).round() as u8,
            )
        })
        .collect()
}

fn gradient_three_stop(count: usize, start: Rgb, middle: Rgb, end: Rgb, gamma: f64) -> Vec<Rgb> {
    let n = count.max(1);
    let denom = (n - 1).max(1) as f64;

    (0..n)
        .map(|i| {
            let t = i as f64 / denom;
            if t <= 0.5 {
                let local_t = (t / 0.5).powf(gamma);
                Rgb(
                    lerp(start.0 as f64, middle.0 as f64, local_t).round() as u8,
                    lerp(start.1 as f64, middle.1 as f64, local_t).round() as u8,
                    lerp(start.2 as f64, middle.2 as f64, local_t).round() as u8,
                )
            } else {
                let local_t = ((t - 0.5) / 0.5).powf(gamma);
                Rgb(
                    lerp(middle.0 as f64, end.0 as f64, local_t).round() as u8,
                    lerp(middle.1 as f64, end.1 as f64, local_t).round() as u8,
                    lerp(middle.2 as f64, end.2 as f64, local_t).round() as u8,
                )
            }
        })
        .collect()
}

fn colorize(text: &str, colors: &[Rgb]) -> String {
    if text.is_empty() {
        return String::new();
    }

    let chars: Vec<char> = text.chars().collect();
    let denom = (chars.len() - 1).max(1) as f64;
    let max_idx = colors.len().saturating_sub(1) as f64;

    let mut out = String::new();
    for (i, ch) in chars.into_iter().enumerate() {
        let idx = ((i as f64 / denom) * max_idx).round() as usize;
        out.push_str(&fg_rgb(colors[idx]));
        out.push(ch);
    }
    out.push_str(RESET);
    out
}

#[cfg(unix)]
fn to_8bit(hex: &str) -> Option<u8> {
    match hex.len() {
        2 => u8::from_str_radix(hex, 16).ok(),
        4 => {
            let value = u16::from_str_radix(hex, 16).ok()?;
            Some((f64::from(value) / f64::from(u16::MAX) * 255.0).round() as u8)
        }
        len if len > 0 => {
            let value = u128::from_str_radix(hex, 16).ok()?;
            let max = (16_u128).pow(len as u32) - 1;
            Some(((value as f64 / max as f64) * 255.0).round() as u8)
        }
        _ => None,
    }
}

#[cfg(unix)]
fn parse_rgb_triplet(input: &str) -> Option<Rgb> {
    let mut parts = input.split('/');
    let r_hex = parts.next()?;
    let g_hex = parts.next()?;
    let b_raw = parts.next()?;
    let b_hex = b_raw.chars().take_while(|c| c.is_ascii_hexdigit()).collect::<String>();

    Some(Rgb(to_8bit(r_hex)?, to_8bit(g_hex)?, to_8bit(&b_hex)?))
}

#[cfg(unix)]
fn parse_osc10_rgb(buffer: &str) -> Option<Rgb> {
    let start = buffer.find("\x1b]10;")?;
    let tail = &buffer[start..];
    let rgb_start = tail.find("rgb:")?;
    parse_rgb_triplet(&tail[rgb_start + 4..])
}

#[cfg(unix)]
fn parse_osc4_rgb(buffer: &str, index: u8) -> Option<Rgb> {
    let prefix = format!("\x1b]4;{index};");
    let start = buffer.find(&prefix)?;
    let tail = &buffer[start + prefix.len()..];
    let rgb_start = tail.find("rgb:")?;
    parse_rgb_triplet(&tail[rgb_start + 4..])
}

/// Returns `true` if the terminal is known to not support OSC color queries
/// or if the environment is unreliable for escape-sequence round-trips.
///
/// Modelled after `terminal-colorsaurus`'s quirks detection, extended with
/// additional checks for Docker, CI, devcontainers, and other environments.
#[cfg(unix)]
fn is_osc_query_unsupported() -> bool {
    static UNSUPPORTED: OnceLock<bool> = OnceLock::new();
    *UNSUPPORTED.get_or_init(|| {
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            return true;
        }

        // CI environments have no real terminal emulator behind the PTY.
        if std::env::var_os("CI").is_some() || std::env::var_os("GITHUB_ACTIONS").is_some() {
            return true;
        }

        // Warp terminal does not respond to OSC color queries in its
        // block-mode renderer, causing a hang until the user presses a key.
        if is_warp_terminal() {
            return true;
        }

        // Emacs terminal emulators (ansi-term, vterm, eshell) don't support
        // OSC queries.
        if std::env::var_os("INSIDE_EMACS").is_some() {
            return true;
        }

        // Docker containers and devcontainers may have a PTY with no real
        // terminal emulator, causing OSC responses to leak as visible text.
        if std::path::Path::new("/.dockerenv").exists()
            || std::env::var_os("REMOTE_CONTAINERS").is_some()
            || std::env::var_os("CODESPACES").is_some()
            || std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
        {
            return true;
        }

        match std::env::var("TERM") {
            // Missing or non-unicode TERM is highly suspect.
            Err(_) => return true,
            // `TERM=dumb` indicates a minimal terminal with no escape support.
            Ok(term) if term == "dumb" => return true,
            // GNU Screen responds to OSC queries in the wrong order, breaking
            // the DA1 sandwich technique. It also only supports OSC 11, not
            // OSC 10 or OSC 4.
            Ok(term) if term == "screen" || term.starts_with("screen.") => return true,
            // Eterm doesn't support DA1, so we skip to avoid the timeout.
            Ok(term) if term == "Eterm" => return true,
            _ => {}
        }

        // tmux and GNU Screen (via STY) do not reliably forward OSC color
        // query responses back to the child process.
        if std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some() {
            return true;
        }

        false
    })
}

/// DA1 (Primary Device Attributes) query — supported by virtually all
/// terminals. Used as a sentinel in the "DA1 sandwich" technique:
/// we send our OSC queries followed by DA1, then read responses. If the
/// DA1 response (`ESC [ ? ...`) arrives first, the terminal doesn't
/// support OSC queries and we bail out immediately instead of waiting
/// for a timeout.
#[cfg(unix)]
const DA1: &str = "\x1b[c";

#[cfg(unix)]
fn query_terminal_colors(palette_indices: &[u8]) -> (Option<Rgb>, Vec<(u8, Rgb)>) {
    use std::{
        fs::OpenOptions,
        os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd},
    };

    use nix::{
        poll::{PollFd, PollFlags, PollTimeout, poll},
        sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr},
        unistd::read,
    };

    if is_osc_query_unsupported() {
        return (None, vec![]);
    }

    let mut tty = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(file) => file,
        Err(_) => return (None, vec![]),
    };

    struct RawGuard {
        fd: RawFd,
        original: Termios,
    }

    impl Drop for RawGuard {
        fn drop(&mut self) {
            // SAFETY: `fd` comes from an open `/dev/tty` and the guard does not outlive that file.
            let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
            let _ = tcsetattr(borrowed, SetArg::TCSANOW, &self.original);
        }
    }

    let original = match tcgetattr(tty.as_fd()) {
        Ok(value) => value,
        Err(_) => return (None, vec![]),
    };
    let mut raw = original.clone();
    cfmakeraw(&mut raw);
    if tcsetattr(tty.as_fd(), SetArg::TCSANOW, &raw).is_err() {
        return (None, vec![]);
    }
    let _guard = RawGuard { fd: tty.as_raw_fd(), original };

    // Build the query: OSC 10 (foreground) + OSC 4 (palette) + DA1 (sentinel).
    // BEL (\x07) is used as string terminator instead of ST (\x1b\\) because
    // urxvt has a bug where it terminates responses with bare ESC instead of
    // ST, causing a parse hang. BEL-terminated queries produce BEL-terminated
    // responses, avoiding this issue.
    let mut query = format!("{ESC}]10;?\x07");
    for index in palette_indices {
        query.push_str(&format!("{ESC}]4;{index};?\x07"));
    }
    // DA1 sentinel — its response acts as a fence to detect unsupported
    // terminals.
    query.push_str(DA1);

    if tty.write_all(query.as_bytes()).is_err() {
        return (None, vec![]);
    }
    if tty.flush().is_err() {
        return (None, vec![]);
    }

    // Use a longer timeout for SSH to account for round-trip latency.
    let timeout_ms =
        if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
            1000
        } else {
            200
        };

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut last_data = Instant::now();
    let mut buffer = String::new();
    let mut foreground = None;
    let mut da1_arrived_first = false;
    let mut palette_colors: Vec<(u8, Option<Rgb>)> =
        palette_indices.iter().copied().map(|index| (index, None)).collect();

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(10));

        let mut fds = [PollFd::new(tty.as_fd(), PollFlags::POLLIN)];
        let timeout = match PollTimeout::try_from(wait) {
            Ok(value) => value,
            Err(_) => break,
        };
        let ready = match poll(&mut fds, timeout) {
            Ok(value) => value,
            Err(_) => break,
        };
        if ready == 0 {
            if Instant::now().saturating_duration_since(last_data) >= Duration::from_millis(50) {
                buffer.clear();
            }
            continue;
        }

        let mut chunk = [0_u8; 256];
        let read_size = match read(tty.as_fd(), &mut chunk) {
            Ok(value) => value,
            Err(_) => break,
        };
        if read_size == 0 {
            continue;
        }

        last_data = Instant::now();
        buffer.push_str(&String::from_utf8_lossy(&chunk[..read_size]));
        if buffer.len() > 1024 {
            let keep_from = buffer.len() - 1024;
            buffer = buffer[keep_from..].to_string();
        }

        // Try to parse OSC responses before checking for DA1. On fast
        // terminals all responses (OSC + DA1) may arrive in a single read,
        // so we must extract OSC data first to avoid a false "DA1 arrived
        // first" conclusion.
        if foreground.is_none() {
            foreground = parse_osc10_rgb(&buffer);
        }
        for (index, color) in &mut palette_colors {
            if color.is_none() {
                *color = parse_osc4_rgb(&buffer, *index);
            }
        }

        // DA1 is a completion fence: once its response (`ESC [ ?`) appears,
        // the terminal has finished processing all prior queries. Use it to
        // stop early in two cases:
        //   1. No OSC data parsed → terminal doesn't support OSC → bail out.
        //   2. Partial OSC data parsed → terminal supports some queries but
        //      not all (e.g. OSC 10 but not OSC 4) → return partial results
        //      instead of waiting for the full timeout.
        let any_osc_parsed =
            foreground.is_some() || palette_colors.iter().any(|(_, color)| color.is_some());
        if buffer.contains("\x1b[?") {
            if !any_osc_parsed {
                da1_arrived_first = true;
            }
            // DA1 response is already in our buffer (consumed by read()),
            // so it won't leak after raw mode is restored.
            break;
        }

        if foreground.is_some() && palette_colors.iter().all(|(_, color)| color.is_some()) {
            // All expected responses received. Drain the trailing DA1
            // response so it doesn't leak into the user's input buffer.
            drain_da1(&tty, &deadline);
            break;
        }
    }

    if da1_arrived_first {
        return (None, vec![]);
    }

    let resolved = palette_colors
        .into_iter()
        .filter_map(|(index, color)| color.map(|rgb| (index, rgb)))
        .collect();
    (foreground, resolved)
}

/// Consume the trailing DA1 response (`ESC [ ? ... c`) so it doesn't leak
/// as visible text after raw mode is restored.
#[cfg(unix)]
fn drain_da1(tty: &std::fs::File, deadline: &Instant) {
    use std::os::fd::AsFd;

    use nix::{
        poll::{PollFd, PollFlags, PollTimeout, poll},
        unistd::read,
    };

    while Instant::now() < *deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let wait = remaining.min(Duration::from_millis(20));

        let mut fds = [PollFd::new(tty.as_fd(), PollFlags::POLLIN)];
        let timeout = match PollTimeout::try_from(wait) {
            Ok(value) => value,
            Err(_) => break,
        };
        let ready = match poll(&mut fds, timeout) {
            Ok(value) => value,
            Err(_) => break,
        };
        if ready == 0 {
            // No data yet — keep polling until the deadline expires rather
            // than giving up early, in case the DA1 response is slightly
            // delayed (e.g. SSH jitter).
            continue;
        }

        let mut chunk = [0_u8; 64];
        let n = match read(tty.as_fd(), &mut chunk) {
            Ok(value) => value,
            Err(_) => break,
        };
        // DA1 response ends with 'c'. Once we see it, we're done.
        if chunk[..n].contains(&b'c') {
            break;
        }
    }
}

#[cfg(not(unix))]
fn query_terminal_colors(_palette_indices: &[u8]) -> (Option<Rgb>, Vec<(u8, Rgb)>) {
    (None, vec![])
}

fn palette_color(palette: &[(u8, Rgb)], index: u8) -> Option<Rgb> {
    palette.iter().find_map(|(palette_index, color)| (*palette_index == index).then_some(*color))
}

fn get_header_colors() -> &'static HeaderColors {
    HEADER_COLORS.get_or_init(|| {
        let (foreground, palette) = query_terminal_colors(&[ANSI_BLUE_INDEX, ANSI_MAGENTA_INDEX]);
        let blue = palette_color(&palette, ANSI_BLUE_INDEX).unwrap_or(DEFAULT_BLUE);
        let magenta = palette_color(&palette, ANSI_MAGENTA_INDEX).unwrap_or(DEFAULT_MAGENTA);

        let suffix_gradient = match foreground {
            Some(color) => gradient_three_stop(
                HEADER_SUFFIX.chars().count(),
                blue,
                magenta,
                color,
                HEADER_SUFFIX_FADE_GAMMA,
            ),
            None => gradient_eased(
                HEADER_SUFFIX.chars().count(),
                blue,
                magenta,
                HEADER_SUFFIX_FADE_GAMMA,
            ),
        };

        HeaderColors { blue, suffix_gradient }
    })
}

fn render_header_variant(
    primary: Rgb,
    suffix_colors: &[Rgb],
    prefix_bold: bool,
    suffix_bold: bool,
) -> String {
    let vite_plus = format!("{}VITE+{RESET_FG}", fg_rgb(primary));
    let suffix = colorize(HEADER_SUFFIX, suffix_colors);
    format!("{}{}", bold(&vite_plus, prefix_bold), bold(&suffix, suffix_bold))
}

/// Render the Vite+ CLI header string with JS-parity coloring behavior.
#[must_use]
pub fn vite_plus_header() -> String {
    if !should_colorize() || !supports_true_color() {
        return format!("VITE+{HEADER_SUFFIX}");
    }

    let header_colors = get_header_colors();
    render_header_variant(header_colors.blue, &header_colors.suffix_gradient, true, true)
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        Rgb, gradient_eased, parse_osc4_rgb, parse_osc10_rgb, parse_rgb_triplet,
        query_terminal_colors, to_8bit,
    };

    #[test]
    fn to_8bit_matches_js_rules() {
        assert_eq!(to_8bit("ff"), Some(255));
        assert_eq!(to_8bit("7f"), Some(127));
        assert_eq!(to_8bit("ffff"), Some(255));
        assert_eq!(to_8bit("0000"), Some(0));
        assert_eq!(to_8bit("fff"), Some(255));
    }

    #[test]
    fn to_8bit_single_digit() {
        assert_eq!(to_8bit("f"), Some(255));
        assert_eq!(to_8bit("0"), Some(0));
        assert_eq!(to_8bit("a"), Some(170));
    }

    #[test]
    fn to_8bit_three_digit() {
        assert_eq!(to_8bit("fff"), Some(255));
        assert_eq!(to_8bit("000"), Some(0));
        assert_eq!(to_8bit("800"), Some(128));
    }

    #[test]
    fn to_8bit_empty_returns_none() {
        assert_eq!(to_8bit(""), None);
    }

    #[test]
    fn to_8bit_invalid_hex_returns_none() {
        assert_eq!(to_8bit("zz"), None);
        assert_eq!(to_8bit("gg"), None);
    }

    #[test]
    fn parse_rgb_triplet_standard() {
        assert_eq!(parse_rgb_triplet("ff/ff/ff"), Some(Rgb(255, 255, 255)));
        assert_eq!(parse_rgb_triplet("00/00/00"), Some(Rgb(0, 0, 0)));
    }

    #[test]
    fn parse_rgb_triplet_four_digit_channels() {
        assert_eq!(parse_rgb_triplet("ffff/ffff/ffff"), Some(Rgb(255, 255, 255)));
        assert_eq!(parse_rgb_triplet("0000/0000/0000"), Some(Rgb(0, 0, 0)));
        assert_eq!(parse_rgb_triplet("aaaa/bbbb/cccc"), Some(Rgb(170, 187, 204)));
    }

    #[test]
    fn parse_rgb_triplet_mixed_digit_channels() {
        // Single digit channels
        assert_eq!(parse_rgb_triplet("f/e/d"), Some(Rgb(255, 238, 221)));
    }

    #[test]
    fn parse_rgb_triplet_trailing_junk_ignored() {
        // The parser stops at non-hex chars for the blue channel
        assert_eq!(parse_rgb_triplet("ff/ff/ff\x1b\\"), Some(Rgb(255, 255, 255)));
    }

    #[test]
    fn parse_rgb_triplet_missing_channel_returns_none() {
        assert_eq!(parse_rgb_triplet("ff/ff"), None);
        assert_eq!(parse_rgb_triplet("ff"), None);
    }

    #[test]
    fn parse_osc10_response_extracts_rgb() {
        let response = "\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\";
        assert_eq!(parse_osc10_rgb(response), Some(Rgb(170, 187, 204)));
    }

    #[test]
    fn parse_osc10_bel_terminated() {
        let response = "\x1b]10;rgb:aaaa/bbbb/cccc\x07";
        assert_eq!(parse_osc10_rgb(response), Some(Rgb(170, 187, 204)));
    }

    #[test]
    fn parse_osc10_no_match_returns_none() {
        assert_eq!(parse_osc10_rgb("garbage"), None);
        assert_eq!(parse_osc10_rgb(""), None);
    }

    #[test]
    fn parse_osc4_response_extracts_rgb() {
        let response = "\x1b]4;5;rgb:aaaa/bbbb/cccc\x1b\\";
        assert_eq!(parse_osc4_rgb(response, 5), Some(Rgb(170, 187, 204)));
    }

    #[test]
    fn parse_osc4_bel_terminated() {
        let response = "\x1b]4;4;rgb:5858/9292/ffff\x07";
        assert_eq!(parse_osc4_rgb(response, 4), Some(Rgb(88, 146, 255)));
    }

    #[test]
    fn parse_osc4_wrong_index_returns_none() {
        let response = "\x1b]4;5;rgb:aaaa/bbbb/cccc\x1b\\";
        assert_eq!(parse_osc4_rgb(response, 4), None);
    }

    #[test]
    fn parse_osc4_no_match_returns_none() {
        assert_eq!(parse_osc4_rgb("garbage", 5), None);
        assert_eq!(parse_osc4_rgb("", 0), None);
    }

    #[test]
    fn parse_osc_multiple_responses_in_buffer() {
        // Simulates a buffer containing OSC 10 + OSC 4;4 + OSC 4;5 responses
        let buffer = "\x1b]10;rgb:d0d0/d0d0/d0d0\x07\
                       \x1b]4;4;rgb:5858/9292/ffff\x07\
                       \x1b]4;5;rgb:bbbb/7474/f7f7\x07";
        assert_eq!(parse_osc10_rgb(buffer), Some(Rgb(208, 208, 208)));
        assert_eq!(parse_osc4_rgb(buffer, 4), Some(Rgb(88, 146, 255)));
        assert_eq!(parse_osc4_rgb(buffer, 5), Some(Rgb(187, 116, 247)));
    }

    #[test]
    fn parse_osc_buffer_with_da1_response() {
        // DA1 response mixed in — OSC parsers should still find their data
        let buffer = "\x1b]10;rgb:d0d0/d0d0/d0d0\x07\x1b[?64;1;2;4c";
        assert_eq!(parse_osc10_rgb(buffer), Some(Rgb(208, 208, 208)));
    }

    #[test]
    fn gradient_counts_match() {
        assert_eq!(gradient_eased(0, Rgb(0, 0, 0), Rgb(255, 255, 255), 1.0).len(), 1);
        assert_eq!(gradient_eased(5, Rgb(10, 20, 30), Rgb(40, 50, 60), 1.0).len(), 5);
    }

    /// Regression test ported from terminal-colorsaurus (issue #38).
    /// In CI there is no real terminal, so `query_terminal_colors` must
    /// return `(None, vec![])` without hanging.
    #[test]
    fn query_terminal_colors_does_not_hang() {
        let (fg, palette) = query_terminal_colors(&[4, 5]);
        // In CI, the environment pre-screening or DA1 sandwich will cause an
        // early return. We don't assert specific values — just that it
        // completes promptly and doesn't panic.
        let _ = (fg, palette);
    }
}
