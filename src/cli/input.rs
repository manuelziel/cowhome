//! `EOF`- (`Ctrl+D`) and `SIGINT`-safe interactive terminal prompts and pagination.
//!
//! Enforces explicit bracket defaults (`[0]`, `[Y/n]`, destructive `[y/N]`), validated
//! integer/string prompts, and paginated list navigation per `02_linux/_project_template`.

use std::io::{self, BufRead, ErrorKind, IsTerminal, Write};

use crate::cli::output::ConsolePrinter;
use crate::core::constants::cli_style::{ANSI_BOLD, ANSI_DIM, BANNER_WIDTH, DEFAULT_PAGE_SIZE};
use crate::core::error::{AppError, AppResult};
use crate::core::process::is_shutdown_requested;

// =============================================================================
/// Returns `true` if `stdin` is attached to an interactive terminal.
#[must_use]
pub fn stdin_is_interactive() -> bool {
    io::stdin().is_terminal()
}

// =============================================================================
/// Prompts the user for a single line of text with an optional bracketed default.
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn prompt_line(label: &str, default_value: Option<&str>) -> AppResult<String> {
    match default_value {
        Some(def) if !def.is_empty() => eprint!("{label} [{def}]: "),
        _ => eprint!("{label}: "),
    }
    flush_streams()?;

    let raw = read_stdin_line_or_cancel()?;
    let trimmed = raw.trim();
    if trimmed.is_empty()
        && let Some(def) = default_value
    {
        return Ok(def.to_owned());
    }
    Ok(trimmed.to_owned())
}

// =============================================================================
/// Continuously prompts until a valid integer within `[min_val, max_val]` is entered.
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn prompt_validated_int(
    label: &str,
    min_val: Option<i64>,
    max_val: Option<i64>,
    default_val: Option<i64>,
    printer: ConsolePrinter,
) -> AppResult<i64> {
    let def_str = default_val.map(|v| v.to_string());
    loop {
        let entered = prompt_line(label, def_str.as_deref())?;
        let Ok(value) = entered.parse::<i64>() else {
            printer.print_error("Enter a valid number!");
            continue;
        };
        if let Some(min_bound) = min_val.filter(|&m| value < m) {
            printer.print_error(&format!("Value must be at least {min_bound}!"));
            continue;
        }
        if let Some(max_bound) = max_val.filter(|&m| value > m) {
            printer.print_error(&format!("Value must be at most {max_bound}!"));
            continue;
        }
        return Ok(value);
    }
}

// =============================================================================
/// Continuously prompts until `validator` accepts the entered string.
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn prompt_validated_string<F>(
    label: &str,
    default_val: Option<&str>,
    validator: F,
    error_message: &str,
    printer: ConsolePrinter,
) -> AppResult<String>
where
    F: Fn(&str) -> bool,
{
    loop {
        let value = prompt_line(label, default_val)?;
        if value.is_empty() {
            printer.print_error("Input cannot be empty!");
            continue;
        }
        if !validator(&value) {
            printer.print_error(error_message);
            continue;
        }
        return Ok(value);
    }
}

// =============================================================================
/// Reads a single numbered menu choice (`Enter your choice (0-N) [default]: `).
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn read_menu_choice_once(max_choice: u8, default_choice: u8) -> AppResult<Option<u8>> {
    let label = format!("Enter your choice (0-{max_choice})");
    let def_str = default_choice.to_string();
    let input = prompt_line(&label, Some(&def_str))?;

    match input.parse::<u8>() {
        Ok(num) if num <= max_choice => Ok(Some(num)),
        _ => Ok(None),
    }
}

// =============================================================================
/// Maps a 1-based menu number (`1..=N`) onto its typed action in `items`.
#[must_use]
pub fn resolve_menu_action<T: Copy>(choice: Option<u8>, items: &[(T, &str)]) -> Option<T> {
    choice
        .and_then(|num| num.checked_sub(1))
        .and_then(|idx| items.get(usize::from(idx)))
        .map(|(action, _)| *action)
}

// =============================================================================
/// Displays a dynamically numbered list (`  1) ...`, `  0) Cancel`) and prompts for selection.
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn prompt_choice_from_list<S: AsRef<str>>(
    items: &[S],
    prompt: &str,
    printer: ConsolePrinter,
) -> AppResult<Option<usize>> {
    if items.is_empty() {
        printer.print_warning("No items available.");
        return Ok(None);
    }
    eprintln!();
    let _max = printer.print_menu(items, "Cancel");

    let count = items.len();
    loop {
        let label = format!("{prompt} (0-{count})");
        let input = prompt_line(&label, Some("0"))?;
        match input.parse::<usize>() {
            Ok(0) => return Ok(None),
            Ok(num) if num <= count => return Ok(Some(num - 1)),
            Ok(_) => printer.print_error(&format!("Value must be at most {count}!")),
            Err(_) => printer.print_error("Enter a valid number!"),
        }
    }
}

// =============================================================================
/// Displays a dynamically numbered `(action, label)` selector (`0) Cancel`) and returns `Option<T>`.
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn prompt_action_from_list<T: Copy>(
    items: &[(T, &str)],
    prompt: &str,
    printer: ConsolePrinter,
) -> AppResult<Option<T>> {
    if items.is_empty() {
        printer.print_warning("No items available.");
        return Ok(None);
    }
    eprintln!();
    let max_choice = printer.print_action_menu(items, "Cancel");

    loop {
        let label = format!("{prompt} (0-{max_choice})");
        let input = prompt_line(&label, Some("0"))?;
        match input.parse::<u8>() {
            Ok(0) => return Ok(None),
            Ok(num) if num <= max_choice => return Ok(resolve_menu_action(Some(num), items)),
            Ok(_) => printer.print_error(&format!("Value must be at most {max_choice}!")),
            Err(_) => printer.print_error("Enter a valid number!"),
        }
    }
}

// =============================================================================
/// Renders `items` interactively in pages (`n` Next, `p` Previous, `0` Back).
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn print_paginated_list<T, F>(
    items: &[T],
    title: &str,
    page_size: Option<usize>,
    printer: ConsolePrinter,
    mut render_item: F,
) -> AppResult<()>
where
    F: FnMut(&T),
{
    if items.is_empty() {
        printer.print_info(&format!("No {} found.", title.to_ascii_lowercase()));
        return wait_for_enter();
    }
    let per_page = page_size.unwrap_or(DEFAULT_PAGE_SIZE).max(1);
    let total_pages = items.len().div_ceil(per_page);
    let mut page = 1usize;

    loop {
        printer.clear_screen();
        printer.print_header(&format!("{title} (Page {page} of {total_pages})"));
        let start = (page - 1) * per_page;
        let end = (start + per_page).min(items.len());
        for item in &items[start..end] {
            render_item(item);
        }
        render_pagination_footer(start, end, items.len(), page, total_pages, title, printer);
        let choice = prompt_line("Select an option", Some("0"))?.to_ascii_lowercase();
        match choice.as_str() {
            "0" | "" => return Ok(()),
            "n" if page < total_pages => page += 1,
            "p" if page > 1 => page -= 1,
            _ => {}
        }
    }
}

// =============================================================================
/// Prints the pagination summary bar (`Showing X-Y of Z`) and `n` / `p` / `0` controls.
fn render_pagination_footer(
    start: usize,
    end: usize,
    total_items: usize,
    page: usize,
    total_pages: usize,
    title: &str,
    printer: ConsolePrinter,
) {
    eprintln!(
        "\n{}",
        printer.colorize(&"—".repeat(BANNER_WIDTH), ANSI_DIM)
    );
    eprintln!(
        "Showing {}-{end} of {total_items} {}\n",
        start + 1,
        title.to_ascii_lowercase()
    );
    let mut opts = Vec::with_capacity(3);
    if page < total_pages {
        opts.push(format!("{} Next page", printer.colorize("n)", ANSI_BOLD)));
    }
    if page > 1 {
        opts.push(format!(
            "{} Previous page",
            printer.colorize("p)", ANSI_BOLD)
        ));
    }
    opts.push(format!("{} Back", printer.colorize("0)", ANSI_BOLD)));
    eprintln!("Options:  {}\n", opts.join("  |  "));
}

// =============================================================================
/// Prompts for a boolean confirmation (`[Y/n]` when `default_yes`, `[y/N]` when destructive).
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn prompt_confirm(question: &str, default_yes: bool) -> AppResult<bool> {
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    eprint!("{question} {suffix}: ");
    flush_streams()?;

    let raw = read_stdin_line_or_cancel()?;
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(normalized.as_str(), "y" | "yes" | "j" | "ja"))
}

// =============================================================================
/// Pauses an interactive menu screen until the user presses Enter (`Press Enter to continue...`).
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
pub fn wait_for_enter() -> AppResult<()> {
    flush_streams()?;
    eprint!("\nPress Enter to continue...");
    flush_streams()?;
    read_stdin_line_or_cancel().map(|_| ())
}

// =============================================================================
/// Flushes `stdout` and `stderr` so inline prompts appear immediately before reading `stdin`.
///
/// # Errors
/// Returns [`AppError::Io`] if flushing `stderr` fails.
fn flush_streams() -> AppResult<()> {
    if let Err(_stdout_err) = io::stdout().flush() {
        // Non-fatal if `stdout` is redirected to a closed pipe while `stderr` drives TUI.
    }
    io::stderr()
        .flush()
        .map_err(|err| AppError::io("Failed flushing stderr prompt", err))
}

// =============================================================================
/// Reads one line from `stdin`, mapping `EOF` (`Ctrl+D`) and `EINTR` (`Ctrl+C`) to `Cancelled`.
///
/// # Errors
/// Returns [`AppError::Cancelled`] on `EOF` / `SIGINT` or [`AppError::Io`] on stream error.
fn read_stdin_line_or_cancel() -> AppResult<String> {
    if is_shutdown_requested() {
        eprintln!();
        return Err(AppError::Cancelled);
    }
    let mut buf = String::new();
    match io::stdin().lock().read_line(&mut buf) {
        Ok(0) => {
            eprintln!();
            Err(AppError::Cancelled)
        }
        Ok(_) if is_shutdown_requested() => {
            eprintln!();
            Err(AppError::Cancelled)
        }
        Ok(_) => Ok(buf),
        Err(err) if err.kind() == ErrorKind::Interrupted => {
            eprintln!();
            Err(AppError::Cancelled)
        }
        Err(err) => Err(AppError::io("Failed reading user input from stdin", err)),
    }
}
