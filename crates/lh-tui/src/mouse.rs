//! Mouse handling -- additive on top of the fully keyboard-driven flows
//! elsewhere in this crate; nothing here is a hard dependency for any flow
//! to work. Pure `App` mutation only (mirrors `dispatch::handle_key`'s own
//! split): the actual daemon round trip for a clicked permission choice is
//! `dispatch::handle_mouse_event`'s job, same as `respond_to_permission` is
//! for the keyboard path.
//!
//! Click hit-testing reuses `ui::permission_popup_rect`/`permission_modal_rows`/
//! `permission_option_rects` -- the exact same `Rect` math `ui::draw` renders
//! into -- rather than a second, separately-maintained guess at "where the
//! modal is", which would drift out of sync with the real layout the moment
//! either one changed without the other.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::app::{App, PermissionChoice};
use crate::ui;

/// Scroll wheel ticks are finer-grained than a page-scroll key press -- a
/// few lines per tick reads as smooth scrolling, not a jarring page jump.
const SCROLL_WHEEL_LINES: u16 = 3;

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Applies one mouse event to `app`. Returns `Some(choice)` when a click
/// landed on a specific permission option -- the caller (`dispatch`) is the
/// one that actually answers the request with it, matching how `mouse.rs`
/// never touches `DaemonClient` itself.
pub fn handle_mouse(app: &mut App, event: MouseEvent, terminal_area: Rect) -> Option<PermissionChoice> {
    if app.pending_permission.is_some() {
        return handle_mouse_over_modal(app, event, terminal_area);
    }

    // Scrolling works regardless of input state, matching the keyboard
    // Up/Down/PageUp/PageDown handling in `dispatch::handle_key`.
    match event.kind {
        MouseEventKind::ScrollUp => app.scroll_up(SCROLL_WHEEL_LINES),
        MouseEventKind::ScrollDown => app.scroll_down(SCROLL_WHEEL_LINES),
        _ => {}
    }
    None
}

fn handle_mouse_over_modal(app: &mut App, event: MouseEvent, terminal_area: Rect) -> Option<PermissionChoice> {
    match event.kind {
        // The wheel cycles the highlighted option, mirroring the Left/
        // Right/Up/Down keyboard bindings -- doesn't answer the request by
        // itself, same as those keys don't.
        MouseEventKind::ScrollUp => {
            app.cycle_permission_selection(false);
            None
        }
        MouseEventKind::ScrollDown => {
            app.cycle_permission_selection(true);
            None
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let popup = ui::permission_popup_rect(terminal_area);
            let (_, options_row, _) = ui::permission_modal_rows(popup);
            if !rect_contains(options_row, event.column, event.row) {
                return None;
            }
            ui::permission_option_rects(options_row)
                .iter()
                .position(|r| rect_contains(*r, event.column, event.row))
                .map(|i| PermissionChoice::ALL[i])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyModifiers;

    use super::*;
    use crate::app::PendingPermission;

    const TERMINAL: Rect = Rect { x: 0, y: 0, width: 120, height: 40 };

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    fn app_with_pending_permission() -> App {
        let mut app = App::new();
        app.pending_permission =
            Some(PendingPermission { request_id: -1, request: crate::app::fake_permission_request(), selected: 0 });
        app
    }

    #[test]
    fn scrolling_with_nothing_pending_scrolls_the_transcript() {
        let mut app = App::new();
        let result = handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 5, 5), TERMINAL);
        assert!(result.is_none());
        assert_eq!(app.transcript_scroll, SCROLL_WHEEL_LINES);

        handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 5, 5), TERMINAL);
        assert_eq!(app.transcript_scroll, 0);
    }

    #[test]
    fn a_left_click_outside_the_modal_does_nothing() {
        let mut app = app_with_pending_permission();
        let result = handle_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), 0, 0), TERMINAL);
        assert!(result.is_none());
        assert!(app.pending_permission.is_some(), "must not have been answered");
    }

    #[test]
    fn scrolling_over_a_pending_permission_cycles_its_selection_instead_of_the_transcript() {
        let mut app = app_with_pending_permission();
        assert_eq!(app.selected_permission_choice(), Some(PermissionChoice::Deny));

        let result = handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 5, 5), TERMINAL);
        assert!(result.is_none(), "scrolling must not answer the request by itself");
        assert_eq!(app.selected_permission_choice(), Some(PermissionChoice::Allow));
        assert_eq!(app.transcript_scroll, 0, "must not also scroll the (hidden) transcript underneath");
    }

    #[test]
    fn clicking_the_deny_option_answers_with_deny() {
        let mut app = app_with_pending_permission();
        let popup = ui::permission_popup_rect(TERMINAL);
        let (_, options_row, _) = ui::permission_modal_rows(popup);
        let rects = ui::permission_option_rects(options_row);
        let deny_rect = rects[PermissionChoice::ALL.iter().position(|c| *c == PermissionChoice::Deny).unwrap()];

        let result = handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), deny_rect.x, deny_rect.y),
            TERMINAL,
        );
        assert_eq!(result, Some(PermissionChoice::Deny));
    }

    #[test]
    fn clicking_the_always_allow_option_answers_with_always_allow() {
        let mut app = app_with_pending_permission();
        let popup = ui::permission_popup_rect(TERMINAL);
        let (_, options_row, _) = ui::permission_modal_rows(popup);
        let rects = ui::permission_option_rects(options_row);
        let idx = PermissionChoice::ALL.iter().position(|c| *c == PermissionChoice::AllowAlways).unwrap();
        let rect = rects[idx];

        let result =
            handle_mouse(&mut app, mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y), TERMINAL);
        assert_eq!(result, Some(PermissionChoice::AllowAlways));
    }

    #[test]
    fn a_right_click_on_an_option_does_not_answer_it() {
        let mut app = app_with_pending_permission();
        let popup = ui::permission_popup_rect(TERMINAL);
        let (_, options_row, _) = ui::permission_modal_rows(popup);
        let rect = ui::permission_option_rects(options_row)[0];

        let result = handle_mouse(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Right), rect.x, rect.y),
            TERMINAL,
        );
        assert!(result.is_none(), "only a left click should answer the request");
        assert!(app.pending_permission.is_some());
    }
}
