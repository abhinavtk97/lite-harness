//! A small, curated true-color (24-bit RGB) palette -- named, semantic
//! constants used throughout `ui.rs` and `markdown.rs` instead of scattered
//! `Color::Cyan`/`Color::Yellow`/`Color::DarkGray` literals, so the whole
//! app reads as one deliberately-designed system instead of whatever a
//! given terminal's basic 16-color ANSI palette happens to remap those
//! names to. Loosely Tokyo-Night-inspired -- the same soft-blue/violet,
//! muted-gray aesthetic real coding-agent TUIs (Claude Code, Codex,
//! opencode) use, rather than the harsher primary red/green/yellow a
//! default ANSI palette gives you. Deliberately does *not* set an explicit
//! background for the whole screen -- only specific elements (diff lines,
//! a selected list row) get an explicit `bg`, so the app still respects
//! whatever background the user's own terminal theme provides everywhere
//! else, the same restraint real polished TUIs show.

use ratatui::style::Color;

/// Primary body text.
pub const FG: Color = Color::Rgb(202, 211, 245);
/// Secondary/de-emphasized text -- system lines, dividers, dim labels.
pub const FG_MUTED: Color = Color::Rgb(128, 135, 162);
/// Structural accent -- the user's own label, prompt glyph, active borders.
pub const ACCENT: Color = Color::Rgb(138, 173, 244);
/// Secondary accent -- the agent's "thinking" label, distinct from the
/// user's own accent color without competing with it.
pub const ACCENT_ALT: Color = Color::Rgb(198, 160, 246);
/// Done / allowed / healthy.
pub const SUCCESS: Color = Color::Rgb(166, 218, 149);
/// In progress / needs attention, but not an error.
pub const WARNING: Color = Color::Rgb(238, 153, 90);
/// Errors, denials, failures.
pub const DANGER: Color = Color::Rgb(237, 135, 150);
/// A file-write diff's added lines (text + tinted background).
pub const DIFF_ADD_FG: Color = Color::Rgb(166, 218, 149);
pub const DIFF_ADD_BG: Color = Color::Rgb(30, 43, 32);
/// A file-write diff's removed lines (text + tinted background).
pub const DIFF_REMOVE_FG: Color = Color::Rgb(237, 135, 150);
pub const DIFF_REMOVE_BG: Color = Color::Rgb(46, 28, 34);
