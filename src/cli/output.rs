//! `NO_COLOR`-aware terminal styling, banners, status lines, and aligned tables.
//!
//! Enforces strict `clig.dev` stream separation (`stderr` for interactive TUI/notices,
//! `stdout` reserved exclusively for machine-readable `--version`, `export`, and
//! `diagnostics` payloads) matching `02_linux/_project_template`.

use std::io::{self, IsTerminal, Write};

use crate::core::constants::app_meta::{DESCRIPTION, SOFTWARE_NAME, VERSION};
use crate::core::constants::cli_style::{
    ANSI_BLUE, ANSI_BOLD, ANSI_CLEAR_SCREEN, ANSI_CYAN, ANSI_DIM, ANSI_GREEN, ANSI_RED, ANSI_RESET,
    ANSI_WHITE, ANSI_YELLOW, BANNER_WIDTH, STATUS_LABEL_WIDTH, SYM_ERR, SYM_INFO, SYM_OK,
};
use crate::setup::SetupEvent;

// =============================================================================
/// Writes pure machine-readable data to `stdout` without panicking on closed pipes (`EPIPE`).
pub fn write_stdout_line(payload: &str) {
    let mut out = io::stdout().lock();
    if let Err(_pipe_err) = writeln!(out, "{payload}") {
        // Downstream consumer (e.g., `head`) closed the pipe early (`EPIPE`).
    }
}

// =============================================================================
/// Dependency-injected console renderer honoring `NO_COLOR` and TTY detection.
#[derive(Debug, Clone, Copy)]
pub struct ConsolePrinter {
    /// Whether ANSI escape codes are enabled for `stderr` output.
    colors_enabled: bool,
}

impl ConsolePrinter {
    /// Creates a new printer with pre-evaluated ANSI color support.
    #[must_use]
    pub const fn new(colors_enabled: bool) -> Self {
        Self { colors_enabled }
    }

    /// Clears the interactive terminal screen when attached to a non-`dumb` TTY.
    pub fn clear_screen(self) {
        let term_ok = std::env::var("TERM")
            .map(|t| !t.eq_ignore_ascii_case("dumb"))
            .unwrap_or(true);
        if io::stderr().is_terminal() && term_ok {
            let mut err = io::stderr().lock();
            if write!(err, "{ANSI_CLEAR_SCREEN}").is_ok() {
                if let Err(_flush_err) = err.flush() {
                    // Best-effort TTY clear; non-fatal if stream closed concurrently.
                }
            }
        }
    }

    /// Prints the 63-column application mode banner (`<App> Setup` / `<App> Runtime`).
    pub fn print_banner(self, mode: &str) {
        let border = "═".repeat(BANNER_WIDTH);
        if self.colors_enabled {
            eprintln!("\n{ANSI_BOLD}{ANSI_CYAN}{border}{ANSI_RESET}");
            eprintln!("{ANSI_BOLD}{ANSI_WHITE}  {SOFTWARE_NAME} {mode}{ANSI_RESET}");
            eprintln!("  {DESCRIPTION}");
            eprintln!("{ANSI_DIM}  Version: {VERSION}{ANSI_RESET}");
            eprintln!("{ANSI_BOLD}{ANSI_CYAN}{border}{ANSI_RESET}\n");
        } else {
            eprintln!("\n{border}");
            eprintln!("  {SOFTWARE_NAME} {mode}");
            eprintln!("  {DESCRIPTION}");
            eprintln!("  Version: {VERSION}");
            eprintln!("{border}\n");
        }
    }

    /// Prints a prominent bold-cyan section header (`═══ Title ═══`) on `stderr`.
    pub fn print_header(self, title: &str) {
        let raw = format!("═══ {title} ═══");
        if self.colors_enabled {
            eprintln!("\n{ANSI_BOLD}{ANSI_CYAN}{raw}{ANSI_RESET}\n");
        } else {
            eprintln!("\n{raw}\n");
        }
    }

    /// Prints an inline cyan subsection header (`═══ Title ═══`) without trailing blank line.
    pub fn print_subsection(self, title: &str) {
        let raw = format!("═══ {title} ═══");
        if self.colors_enabled {
            eprintln!("\n{ANSI_CYAN}{raw}{ANSI_RESET}");
        } else {
            eprintln!("\n{raw}");
        }
    }

    /// Prints the blue `Available options:` menu heading.
    pub fn print_options_heading(self) {
        if self.colors_enabled {
            eprintln!("{ANSI_BLUE}Available options:{ANSI_RESET}");
        } else {
            eprintln!("Available options:");
        }
    }

    /// Renders a dynamically numbered menu (`1..=N` + `0) <exit_label>`) and returns `N`.
    #[must_use]
    pub fn print_menu<S: AsRef<str>>(self, options: &[S], exit_label: &str) -> u8 {
        self.print_options_heading();
        let num_width = if options.len() >= 10 { 2 } else { 1 };
        for (idx, label) in options.iter().enumerate() {
            let num = idx + 1;
            let text = label.as_ref();
            eprintln!("  {num:>num_width$}) {text}");
        }
        let zero = 0u8;
        eprintln!("  {zero:>num_width$}) {exit_label}\n");
        u8::try_from(options.len()).unwrap_or(u8::MAX)
    }

    /// Renders a typed `(action, label)` slice as a numbered menu (`1..=N` + `0)`) and returns `N`.
    #[must_use]
    pub fn print_action_menu<T>(self, items: &[(T, &str)], exit_label: &str) -> u8 {
        self.print_options_heading();
        let num_width = if items.len() >= 10 { 2 } else { 1 };
        for (idx, (_, label)) in items.iter().enumerate() {
            let num = idx + 1;
            eprintln!("  {num:>num_width$}) {label}");
        }
        let zero = 0u8;
        eprintln!("  {zero:>num_width$}) {exit_label}\n");
        u8::try_from(items.len()).unwrap_or(u8::MAX)
    }

    /// Prints a 15-char aligned key-value status row (`  Label:          ✓ Message`).
    pub fn print_status_line(self, label: &str, ansi_color: &str, symbol: &str, message: &str) {
        let label_col = format!("{label}:");
        let status_text = format!("{symbol} {message}");
        let colored = self.colorize(&status_text, ansi_color);
        eprintln!("  {label_col:<STATUS_LABEL_WIDTH$} {colored}");
    }

    /// Prints a green `✓` success line on `stderr`.
    pub fn print_success(self, message: &str) {
        eprintln!(
            "{}",
            self.colorize(&format!("{SYM_OK} {message}"), ANSI_GREEN)
        );
    }

    /// Prints a yellow `✗` warning line on `stderr`.
    pub fn print_warning(self, message: &str) {
        eprintln!(
            "{}",
            self.colorize(&format!("{SYM_ERR} {message}"), ANSI_YELLOW)
        );
    }

    /// Prints a red `✗` error line on `stderr`.
    pub fn print_error(self, message: &str) {
        eprintln!(
            "{}",
            self.colorize(&format!("{SYM_ERR} {message}"), ANSI_RED)
        );
    }

    /// Prints a blue `ℹ` informational line on `stderr`.
    pub fn print_info(self, message: &str) {
        eprintln!(
            "{}",
            self.colorize(&format!("{SYM_INFO} {message}"), ANSI_BLUE)
        );
    }

    /// Writes pure machine-readable data to `stdout` without panicking on `EPIPE`.
    pub fn stdout_line(self, payload: &str) {
        write_stdout_line(payload);
    }

    /// Formats a colored `✓` / `✗` inline status badge for table cells.
    #[must_use]
    pub fn status_badge(self, ok: bool, ok_label: &str, err_label: &str) -> String {
        if ok {
            self.colorize(&format!("{SYM_OK} {ok_label}"), ANSI_GREEN)
        } else {
            self.colorize(&format!("{SYM_ERR} {err_label}"), ANSI_RED)
        }
    }

    /// Wraps `text` in `ansi_code` when colors are active, or returns `text` unchanged.
    #[must_use]
    pub fn colorize(self, text: &str, ansi_code: &str) -> String {
        if self.colors_enabled && !ansi_code.is_empty() {
            format!("{ansi_code}{text}{ANSI_RESET}")
        } else {
            text.to_owned()
        }
    }

    /// Prints a formatted, column-aligned table on `stderr` with 2-space indentation.
    pub fn print_table(self, headers: &[&str], rows: &[Vec<String>], min_widths: Option<&[usize]>) {
        for line in format_table(headers, rows, min_widths) {
            eprintln!("{line}");
        }
    }

    /// Renders a domain [`SetupEvent`] to `stderr`.
    pub fn render_setup_event(self, event: &SetupEvent) {
        match event {
            SetupEvent::Step(msg) | SetupEvent::Skipped(msg) => self.print_info(msg),
            SetupEvent::Completed(msg) => self.print_success(msg),
            SetupEvent::Warning(msg) => self.print_warning(msg),
        }
    }
}

// =============================================================================
/// Builds aligned table lines accounting for ANSI escape codes and UTF-8 glyphs.
#[must_use]
pub fn format_table(
    headers: &[&str],
    rows: &[Vec<String>],
    min_widths: Option<&[usize]>,
) -> Vec<String> {
    let col_count = headers.len();
    let mut widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(idx, h)| {
            let min_w = min_widths.and_then(|mw| mw.get(idx).copied()).unwrap_or(0);
            visible_char_width(h).max(min_w)
        })
        .collect();

    for row in rows {
        for (idx, cell) in row.iter().enumerate().take(col_count) {
            widths[idx] = widths[idx].max(visible_char_width(cell));
        }
    }

    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(format_table_row_str(headers, &widths));

    let underlines: Vec<String> = widths.iter().map(|&w| "-".repeat(w)).collect();
    let underline_refs: Vec<&str> = underlines.iter().map(String::as_str).collect();
    lines.push(format_table_row_str(&underline_refs, &widths));

    for row in rows {
        let cell_refs: Vec<&str> = row.iter().map(String::as_str).collect();
        lines.push(format_table_row_str(&cell_refs, &widths));
    }
    lines
}

// =============================================================================
/// Formats a single indented table row using precomputed visible column widths.
fn format_table_row_str(cells: &[&str], widths: &[usize]) -> String {
    let mut line = String::from("  ");
    let last_idx = widths.len().saturating_sub(1);

    for (idx, &col_width) in widths.iter().enumerate() {
        let cell = cells.get(idx).copied().unwrap_or("");
        line.push_str(cell);
        if idx < last_idx {
            let vis = visible_char_width(cell);
            let pad = col_width.saturating_sub(vis) + 2;
            line.push_str(&" ".repeat(pad));
        }
    }
    line
}

// =============================================================================
/// Computes printable character width of `text` after stripping ANSI CSI sequences.
#[must_use]
pub fn visible_char_width(text: &str) -> usize {
    let mut count = 0usize;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for seq_ch in chars.by_ref() {
                if seq_ch.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            count += 1;
        }
    }
    count
}
