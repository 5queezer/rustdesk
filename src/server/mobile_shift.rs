//! One-shot Shift from mobile soft keyboards.
//!
//! Android IMEs (Gboard auto-capitalization) report Shift through a key event with a Shift meta
//! state. Flutter turns that into a ShiftLeft key-down, and the matching key-up only comes with the
//! next hardware key event. Text typed on the soft keyboard is committed by the IME and produces no
//! key events, so the client keeps Shift "held": it is pressed on the host, and every following
//! mouse event carries the Shift modifier (clicks become Shift+clicks).
//!
//! For a mobile peer, a Shift held by key events is treated as one-shot: it is consumed by the
//! next text input, or by a mouse button press once another key has used it. The caller then
//! releases Shift on the host, and Shift is stripped from the peer's modifiers until the peer
//! sends Shift again or reports it released. A Shift that is pressed and then used for a click
//! directly is kept.
//!
//! The client also keeps that state across a reconnect, so a new connection can report Shift
//! from its first event without any Shift key-down. Such a Shift, and a Shift reported on
//! soft-keyboard text without a key-down, is stripped the same way. A Shift the user switches on
//! later from the client's toolbar (no key event) is kept for clicks.

use crate::input::{MOUSE_TYPE_DOWN, MOUSE_TYPE_MASK};
use base::message_proto::{key_event, ControlKey, KeyEvent, KeyboardMode, MouseEvent};
use hbb_common::protobuf::{Enum, EnumOrUnknown};

#[derive(Default)]
pub(super) struct MobileShift {
    // The Shift key-down that is still held, replayed as key-up to release it.
    held: Option<KeyEvent>,
    // Another key was pressed while Shift was held.
    used: bool,
    // Shift was released here; the peer still reports it as a modifier.
    stale: bool,
    // The connection has sent input before.
    started: bool,
}

impl MobileShift {
    /// Updates the state for a key event from the peer. Returns the key-up to inject before
    /// `evt` when the held Shift has been consumed.
    pub fn on_key(&mut self, evt: &mut KeyEvent) -> Option<KeyEvent> {
        let down = evt.down || evt.press;
        let first = !std::mem::replace(&mut self.started, true);
        if is_shift_key(evt) {
            self.held = down.then(|| evt.clone());
            self.used = false;
            self.stale = false;
            return None;
        }
        let release = if is_text(evt) {
            if self.held.is_none() && has_shift(&evt.modifiers) {
                self.stale = true;
            }
            self.consume()
        } else {
            if first && self.held.is_none() && has_shift(&evt.modifiers) {
                self.stale = true;
            }
            if down && self.held.is_some() {
                self.used = true;
            }
            None
        };
        self.strip(&mut evt.modifiers);
        release
    }

    /// Updates the state for a mouse event from the peer. Returns the key-up to inject before
    /// `evt` when the held Shift has been consumed.
    pub fn on_mouse(&mut self, evt: &mut MouseEvent) -> Option<KeyEvent> {
        if !std::mem::replace(&mut self.started, true) && has_shift(&evt.modifiers) {
            self.stale = true;
        }
        let release = if evt.mask & MOUSE_TYPE_MASK == MOUSE_TYPE_DOWN && self.used {
            self.consume()
        } else {
            None
        };
        self.strip(&mut evt.modifiers);
        release
    }

    fn consume(&mut self) -> Option<KeyEvent> {
        let mut up = self.held.take()?;
        up.down = false;
        up.press = false;
        self.used = false;
        self.stale = true;
        Some(up)
    }

    fn strip(&mut self, modifiers: &mut Vec<EnumOrUnknown<ControlKey>>) {
        if !self.stale {
            return;
        }
        if has_shift(modifiers) {
            modifiers.retain(|ck| !is_shift(ck));
        } else {
            // The peer no longer reports Shift: its state agrees with the host again.
            self.stale = false;
        }
    }
}

fn is_shift(ck: &EnumOrUnknown<ControlKey>) -> bool {
    ck.value() == ControlKey::Shift.value() || ck.value() == ControlKey::RShift.value()
}

fn has_shift(modifiers: &[EnumOrUnknown<ControlKey>]) -> bool {
    modifiers.iter().any(is_shift)
}

fn is_shift_key(evt: &KeyEvent) -> bool {
    match (&evt.union, evt.mode.enum_value_or(KeyboardMode::Legacy)) {
        (Some(key_event::Union::ControlKey(ck)), _) => is_shift(ck),
        (Some(key_event::Union::Chr(code)), KeyboardMode::Map | KeyboardMode::Translate) => {
            matches!(
                crate::keyboard::keycode_to_rdev_key(code & 0x0000FFFF),
                rdev::Key::ShiftLeft | rdev::Key::ShiftRight
            )
        }
        _ => false,
    }
}

// Text committed by the soft keyboard: already composed, Shift has done its job.
fn is_text(evt: &KeyEvent) -> bool {
    match evt.union {
        Some(key_event::Union::Seq(_)) | Some(key_event::Union::Unicode(_)) => true,
        Some(key_event::Union::Chr(_)) => {
            evt.mode.enum_value_or(KeyboardMode::Legacy) == KeyboardMode::Legacy
        }
        _ => false,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    // X11 keycode of the left Shift, as the client sends it in Map mode.
    const SHIFT_LEFT_CODE: u32 = 50;
    const KEY_A_CODE: u32 = 38;

    fn map_key(code: u32, down: bool) -> KeyEvent {
        let mut evt = KeyEvent::new();
        evt.set_chr(code);
        evt.mode = KeyboardMode::Map.into();
        evt.down = down;
        evt
    }

    // What the mobile client sends for a soft-keyboard character (`press`, legacy mode).
    fn text_chr(chr: char, shift: bool) -> KeyEvent {
        let mut evt = KeyEvent::new();
        evt.set_chr(chr as u32);
        evt.mode = KeyboardMode::Legacy.into();
        evt.press = true;
        if shift {
            evt.modifiers.push(ControlKey::Shift.into());
        }
        evt
    }

    fn text_seq(seq: &str) -> KeyEvent {
        let mut evt = KeyEvent::new();
        evt.set_seq(seq.to_owned());
        evt
    }

    fn mouse_down(shift: bool) -> MouseEvent {
        let mut evt = MouseEvent::new();
        evt.mask = MOUSE_TYPE_DOWN | (1 << 3);
        if shift {
            evt.modifiers.push(ControlKey::Shift.into());
        }
        evt
    }

    #[test]
    fn soft_keyboard_text_releases_one_shot_shift() {
        let mut guard = MobileShift::default();
        // Gboard capitalizes: Flutter reports ShiftLeft down and never up.
        assert!(guard.on_key(&mut map_key(SHIFT_LEFT_CODE, true)).is_none());

        let mut a = text_chr('A', true);
        let up = guard
            .on_key(&mut a)
            .expect("Shift released before the text");
        assert_eq!(up.chr(), SHIFT_LEFT_CODE);
        assert!(!up.down && !up.press);
        assert!(!has_shift(&a.modifiers));

        let mut b = text_chr('b', true);
        assert!(guard.on_key(&mut b).is_none());
        assert!(!has_shift(&b.modifiers));

        let mut click = mouse_down(true);
        assert!(guard.on_mouse(&mut click).is_none());
        assert!(
            !has_shift(&click.modifiers),
            "click must not be a Shift+click"
        );
    }

    #[test]
    fn committed_word_releases_one_shot_shift() {
        let mut guard = MobileShift::default();
        guard.on_key(&mut map_key(SHIFT_LEFT_CODE, true));
        assert!(guard.on_key(&mut text_seq("Hello")).is_some());
        assert!(guard.on_key(&mut text_seq("world")).is_none());
    }

    #[test]
    fn click_after_used_shift_releases_it() {
        let mut guard = MobileShift::default();
        guard.on_key(&mut map_key(SHIFT_LEFT_CODE, true));
        guard.on_key(&mut map_key(KEY_A_CODE, true));
        guard.on_key(&mut map_key(KEY_A_CODE, false));

        let mut click = mouse_down(true);
        assert!(guard.on_mouse(&mut click).is_some());
        assert!(!has_shift(&click.modifiers));
    }

    #[test]
    fn shift_click_without_other_keys_is_kept() {
        let mut guard = MobileShift::default();
        guard.on_key(&mut map_key(SHIFT_LEFT_CODE, true));
        let mut click = mouse_down(true);
        assert!(guard.on_mouse(&mut click).is_none());
        assert!(has_shift(&click.modifiers));
    }

    #[test]
    fn toolbar_shift_is_kept() {
        // The toolbar toggle sends no key event, only the modifier.
        let mut guard = MobileShift::default();
        guard.on_mouse(&mut mouse_down(false));
        let mut click = mouse_down(true);
        assert!(guard.on_mouse(&mut click).is_none());
        assert!(has_shift(&click.modifiers));
    }

    #[test]
    fn new_connection_with_stale_shift_click() {
        // Reconnect: the client still reports Shift, no Shift key-down on this connection.
        let mut guard = MobileShift::default();
        let mut click = mouse_down(true);
        assert!(guard.on_mouse(&mut click).is_none());
        assert!(
            !has_shift(&click.modifiers),
            "click must not be a Shift+click"
        );
        let mut b = text_chr('b', true);
        assert!(guard.on_key(&mut b).is_none());
        assert!(!has_shift(&b.modifiers));
    }

    #[test]
    fn new_connection_with_stale_shift_text() {
        let mut guard = MobileShift::default();
        let mut b = text_chr('b', true);
        assert!(guard.on_key(&mut b).is_none());
        assert!(!has_shift(&b.modifiers));
        let mut click = mouse_down(true);
        guard.on_mouse(&mut click);
        assert!(!has_shift(&click.modifiers));
    }

    #[test]
    fn shift_on_text_without_key_down_is_stripped() {
        let mut guard = MobileShift::default();
        guard.on_mouse(&mut mouse_down(false));
        let mut b = text_chr('b', true);
        guard.on_key(&mut b);
        assert!(!has_shift(&b.modifiers));
        let mut click = mouse_down(true);
        guard.on_mouse(&mut click);
        assert!(!has_shift(&click.modifiers));
    }

    #[test]
    fn shift_released_by_peer_is_not_replayed() {
        let mut guard = MobileShift::default();
        guard.on_key(&mut map_key(SHIFT_LEFT_CODE, true));
        guard.on_key(&mut map_key(SHIFT_LEFT_CODE, false));
        assert!(guard.on_key(&mut text_chr('b', false)).is_none());
        let mut click = mouse_down(false);
        assert!(guard.on_mouse(&mut click).is_none());
    }

    #[test]
    fn peer_reporting_no_shift_ends_stripping() {
        let mut guard = MobileShift::default();
        guard.on_key(&mut map_key(SHIFT_LEFT_CODE, true));
        guard.on_key(&mut text_chr('A', true));
        // The client's cached Shift was cleared (e.g. the toolbar toggle was tapped).
        guard.on_mouse(&mut mouse_down(false));
        // A later deliberate toolbar Shift+click is honoured again.
        let mut click = mouse_down(true);
        guard.on_mouse(&mut click);
        assert!(has_shift(&click.modifiers));
    }

    #[test]
    fn legacy_shift_control_key_is_tracked() {
        let mut guard = MobileShift::default();
        let mut shift = KeyEvent::new();
        shift.set_control_key(ControlKey::Shift);
        shift.down = true;
        guard.on_key(&mut shift);
        let up = guard.on_key(&mut text_chr('A', false)).unwrap();
        assert_eq!(up.control_key(), ControlKey::Shift);
        assert!(!up.down);
    }
}
