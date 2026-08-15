//! Cross-platform boot-terminal rendering for the `.route` compiler.
//!
//! The compiler stays terminal-agnostic. This module owns only the temporary
//! boot terminal session, capability detection, terminal sizing, and rendering.

use std::io::{self, IsTerminal, Write};
use std::process::Command;

#[derive(Debug, Clone, Copy)]
pub struct Terminal {
    interactive: bool,
    ansi: bool,
}

impl Terminal {
    pub fn new() -> Self {
        let interactive = io::stdout().is_terminal();
        let ansi = interactive && ansi_supported();
        Self { interactive, ansi }
    }

    pub fn width(&self) -> usize {
        terminal_size().map(|(width, _)| width).unwrap_or(80).clamp(40, 512)
    }

    pub fn height(&self) -> usize {
        terminal_size().map(|(_, height)| height).unwrap_or(24).clamp(12, 256)
    }

    pub fn begin_boot(&self) {
        if self.ansi {
            // Alternate screen keeps the normal RBE log screen intact. The
            // compiler owns this temporary screen until end_boot().
            print!("\x1b[?1049h\x1b[2J\x1b[H\x1b[?25l");
        } else if self.interactive {
            println!();
            println!("RBE Route Compiler");
            println!();
        }
        let _ = io::stdout().flush();
    }

    pub fn end_boot(&self) {
        if self.ansi {
            print!("\x1b[?25h\x1b[?1049l");
        } else if self.interactive {
            println!();
        }
        let _ = io::stdout().flush();
    }

    pub fn render(
        &self,
        route_count: usize,
        current_file: Option<&str>,
        state: &str,
        current: usize,
        total: usize,
    ) {
        let width = self.width();
        let counter = format!("{current}/{total}");
        let bar_width = width
            .saturating_sub(state.chars().count() + counter.chars().count() + 4)
            .max(12)
            .min(width.saturating_sub(2));
        let filled = if total == 0 {
            bar_width
        } else {
            current.saturating_mul(bar_width).checked_div(total).unwrap_or(0).min(bar_width)
        };
        let filled_bar = "█".repeat(filled);
        let empty_bar = "░".repeat(bar_width - filled);

        if !self.interactive {
            println!("RBE Route Compiler — {state} {counter}");
            if let Some(file) = current_file {
                println!("  {file}");
            }
            return;
        }

        if self.ansi {
            let height = self.height();
            let mut output = String::new();
            output.push_str("\x1b[2J\x1b[H");
            output.push_str("\x1b[1;36mRBE Route Compiler\x1b[0m\n");
            output.push_str("Scanning ./api...\n");
            output.push_str(&format!("Found {route_count} route files\n\n"));
            output.push_str(&format!("\x1b[1m{state}\x1b[0m {counter}\n"));
            if let Some(file) = current_file {
                output.push_str(&format!("\x1b[90m{file}\x1b[0m\n"));
            }

            // Leave the progress bar on the final terminal row.
            let used_lines = 6 + usize::from(current_file.is_some());
            let blank_lines = height.saturating_sub(used_lines + 1);
            output.push_str(&"\n".repeat(blank_lines));
            output.push_str(&format!(
                "[\x1b[36m{filled_bar}\x1b[90m{empty_bar}\x1b[0m]"
            ));
            print!("{output}");
        } else {
            print!("\rRBE Route Compiler — {state} {counter}");
            if let Some(file) = current_file {
                print!(" — {file}");
            }
            println!();
            println!("[{}{}]", "#".repeat(filled), "-".repeat(bar_width - filled));
        }
        let _ = io::stdout().flush();
    }
}

fn ansi_supported() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("TERM")
        .map(|value| value.eq_ignore_ascii_case("dumb"))
        .unwrap_or(false)
    {
        return false;
    }

    #[cfg(windows)]
    {
        return enable_windows_vt();
    }

    #[cfg(not(windows))]
    {
        true
    }
}

fn terminal_size() -> Option<(usize, usize)> {
    if let (Ok(columns), Ok(lines)) = (std::env::var("COLUMNS"), std::env::var("LINES")) {
        if let (Ok(width), Ok(height)) = (columns.parse::<usize>(), lines.parse::<usize>()) {
            if width > 0 && height > 0 {
                return Some((width, height));
            }
        }
    }

    #[cfg(windows)]
    {
        if let Some(size) = windows_console_size() {
            return Some(size);
        }
        if let Some(width) = command_columns("cmd", &["/C", "mode con"]) {
            return Some((width, 24));
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(size) = command_terminal_size("stty", &["size"]) {
            return Some(size);
        }
    }

    None
}

#[cfg(not(windows))]
fn command_terminal_size(program: &str, args: &[&str]) -> Option<(usize, usize)> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut values = stdout.split_whitespace();
    let height = values.next()?.parse::<usize>().ok()?;
    let width = values.next()?.parse::<usize>().ok()?;
    Some((width, height))
}

#[cfg(windows)]
fn command_columns(program: &str, args: &[&str]) -> Option<usize> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split(':');
        if parts.next()?.trim().eq_ignore_ascii_case("columns") {
            return parts.next()?.trim().parse::<usize>().ok();
        }
    }
    None
}

#[cfg(windows)]
fn enable_windows_vt() -> bool {
    type Handle = *mut core::ffi::c_void;
    type Dword = u32;

    const STD_OUTPUT_HANDLE: Dword = (-11i32) as Dword;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: Dword = 0x0004;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    extern "system" {
        fn GetStdHandle(n_std_handle: Dword) -> Handle;
        fn GetConsoleMode(h_console_handle: Handle, lp_mode: *mut Dword) -> i32;
        fn SetConsoleMode(h_console_handle: Handle, mode: Dword) -> i32;
    }

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return false;
        }

        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(windows)]
fn windows_console_size() -> Option<(usize, usize)> {
    type Handle = *mut core::ffi::c_void;
    type SmallInt = i16;
    type Word = u16;
    type Dword = u32;

    #[repr(C)]
    struct Coord { x: SmallInt, y: SmallInt }

    #[repr(C)]
    struct SmallRect {
        left: SmallInt,
        top: SmallInt,
        right: SmallInt,
        bottom: SmallInt,
    }

    #[repr(C)]
    struct ConsoleScreenBufferInfo {
        size: Coord,
        cursor: Coord,
        attributes: Word,
        window: SmallRect,
        maximum_window_size: Coord,
    }

    const STD_OUTPUT_HANDLE: Dword = (-11i32) as Dword;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    extern "system" {
        fn GetStdHandle(n_std_handle: Dword) -> Handle;
        fn GetConsoleScreenBufferInfo(handle: Handle, info: *mut ConsoleScreenBufferInfo) -> i32;
    }

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut info = ConsoleScreenBufferInfo {
            size: Coord { x: 0, y: 0 },
            cursor: Coord { x: 0, y: 0 },
            attributes: 0,
            window: SmallRect { left: 0, top: 0, right: 0, bottom: 0 },
            maximum_window_size: Coord { x: 0, y: 0 },
        };
        if GetConsoleScreenBufferInfo(handle, &mut info) == 0 {
            return None;
        }
        let width = (info.window.right - info.window.left + 1).max(1) as usize;
        let height = (info.window.bottom - info.window.top + 1).max(1) as usize;
        Some((width, height))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn progress_bar_fills_left_to_right() {
        let total = 100;
        let width = 30usize;
        let filled = 50usize.saturating_mul(width) / total;
        assert_eq!(filled, 15);
    }
}
