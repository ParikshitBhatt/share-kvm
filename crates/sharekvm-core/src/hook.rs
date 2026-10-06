//! The global input hook, one implementation per OS, behind one interface.
//!
//! Every event says whether it was *injected* (made by software, such as
//! ShareKVM replaying the other computer's input) or came from this
//! computer's own mouse and keyboard. That's what lets either computer take
//! control: touching a device's own mouse while it's being driven hands
//! control back to it, and ShareKVM's replayed input never triggers that.

use rdev::EventType;

#[derive(Debug, Clone, Copy)]
pub struct Hooked {
    pub ev: EventType,
    /// Raw movement for `MouseMove`, when the OS reports it (macOS).
    pub delta: Option<(f64, f64)>,
    /// Created by software (including ShareKVM itself), not this computer's devices.
    pub injected: bool,
}

/// Installs the hook on the current (main) thread and runs forever.
/// The callback returns true to let an event through to this computer.
pub fn run(callback: impl FnMut(Hooked) -> bool + 'static) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return crate::mac_hook::run(callback);
    #[cfg(windows)]
    return crate::win_hook::run(callback);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        // Elsewhere rdev can't tell injected input apart; treat it all as local.
        let mut callback = callback;
        rdev::grab(move |e| if callback(Hooked { ev: e.event_type, delta: None, injected: false }) { Some(e) } else { None })
            .map_err(|e| format!("could not install input hook: {e:?}"))
    }
}
