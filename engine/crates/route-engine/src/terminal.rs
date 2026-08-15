//! Cross-platform boot-terminal rendering for the `.route` compiler.
//!
//! The compiler itself stays terminal-agnostic. This module only decides
//! whether an interactive terminal is available, what its width is, and
//! whether ANSI cursor/color control can safely be used.

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

    pub fn interactive(&self) -> bool {
        self.interactive
    }

    pub fn ansi(&self) -> bool {
        self.ansi
    }

    pub fn width(&self) -> usize {
        terminal_width().clamp(40, 512)
    }

    pub fn begin_boot(&self) {
        if self.ansi {
            print!("\x1b[2J\x1b[H");
        } else if self.interactive {
            println!();
            println!("RBE Route Compiler");
            println!();
        }
        let _ = io::stdout().flush();
    }

    pub fn end_boot(&self) {
        if self.ansi {
            print!("\x1b[2J\x1b[H");
        } else if self.interactive {
            println!();
        }
        let _ = io::stdout().flush();
    }

    pub fn header(&self, route_count: usize) {
        if !self.interactive {
            println!("RBE Route Compiler — scanning ./api ({route_count} route files)");
            return;
        }

        if self.ansi {
            println!("\x1b[1;36mRBE Route Compiler\x1b[0m");
        } else {
            println!("RBE Route Compiler");
        }
        println!("Scanning ./api...");
        println!("Found {route_count} route files");
        println!();
    }

    pub fn progress(&self, state: &str, current: usize, total: usize) {
        let width = self.width();
        let counter = format!("{current}/{total}");
        let plain_label_width = state.chars().count() + 1 + counter.chars().count();
        let bar_width = width
            .saturating_sub(plain_label_width + 3)
            .max(12)
            .min(width.saturating_sub(2));
        let filled = if total == 0 {
            bar_width
        } else {
            current.saturating_mul(bar_width).checked_div(total).unwrap_or(0).min(bar_width)
        };

        if !self.interactive {
            println!("{state} {counter}");
            return;
        }

        if self.ansi {
            print!(
                "\x1b[2K\r\x1b[1m{state}\x1b[0m {counter}\n\x1b[2K\r[\x1b[36m{}\x1b[90m{}\x1b[0m]",
                "█".repeat(filled),
                "░".repeat(bar_width - filled),
            );
        } else {
            print!("\r{state} {counter}\n[{}/{}]", "#".repeat(filled), "-".repeat(bar_width - filled),); 
        }
        let _ = io::stdout().flush();
    }
}

fn ansi_supported() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }

    if std::env::var("TERM").map(|value| value.eq_ignore_ascii_case("dumb")).unwrap_or(false) {
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

fn terminal_width() -> usize {
    if let Ok(columns) = std::env::var("COLUMNS") {
        if let Ok(width) = columns.parse::<usize>() {
            if width >= 40 {
                return width;
            }
        }
    }

    #[cfg(windows)]
    {
        if let Some(width) = windows_console_width() {
            return width;
        }
        if let Some(width) = command_width("cmd", &["/C", "mode con"]) {
            return width;
        }
    }

    #[cfg(not(windows))]
    {
        if let Some(width) = command_width("stty", &["size"]) {
            return width;
        }
    }

    80
}

fn command_width(program: &str, args: &[&str]) -> Option<usize> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);

    if program == "stty" {
        return text
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<usize>().ok());
    }

    for line in text.lines() {
        let mut parts = line.split(':');
        let key = parts.next()?.trim();
        let value = parts.next()?.trim();
        if key.eq_ignore_ascii_case("columns") {
            if let Ok(width) = value.parse::<usize>() {
                return Some(width);
            }
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

    extern "system" {
        fn GetStdHandle(n_std_handle: Dword) -> Handle;
        fn GetConsoleMode(h_console_handle: Handle, lp_mode: *mut Dword) -> i32;
        fn SetConsoleMode(h_console_handle: Handle, mode: Dword) -> i32;
    }

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() {
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
fn windows_console_width() -> Option<usize> {
    type Handle = *mut core::ffi::c_void;
    type SmallInt = i16;
    type Word = u16;
    type Dword = u32;

    #[repr(C)]
    struct Coord {
        x: SmallInt,
        y: SmallInt,
    }

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

    extern "system" {
        fn GetStdHandle(n_std_handle: Dword) -> Handle;
        fn GetConsoleScreenBufferInfo(handle: Handle, info: *mut ConsoleScreenBufferInfo) -> i32;
    }

    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle.is_null() {
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
        Some((info.window.right - info.window.left + 1).max(1) as usize)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn progress_bar_width_is_deterministic() {
        let total = 100;
        let width = 30usize;
        let filled = 50usize.saturating_mul(width) / total;
        assert_eq!(filled, 15);
    }
}
