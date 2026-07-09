//! Per-pane attention state — the seed of the killer feature. A pane goes `Pending` when an agent
//! fires a notification (OSC 9/777/99) or bell while unfocused, and clears when the user focuses it.
//! M2 wires `Pending` to Windows toasts + the pane ring; M1 just tracks the state.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Attention {
    /// Nothing waiting.
    #[default]
    Quiet,
    /// An agent signalled it needs attention; ring the pane until focused.
    Pending,
}

impl Attention {
    /// Mark that the pane needs attention.
    pub fn set_pending(&mut self) {
        *self = Attention::Pending;
    }

    /// Clear attention (the user focused the pane).
    pub fn focus(&mut self) {
        *self = Attention::Quiet;
    }

    pub fn is_pending(self) -> bool {
        self == Attention::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_quiet() {
        assert_eq!(Attention::default(), Attention::Quiet);
    }

    #[test]
    fn pending_then_focus_clears() {
        let mut a = Attention::default();
        assert!(!a.is_pending());
        a.set_pending();
        assert!(a.is_pending());
        a.focus();
        assert_eq!(a, Attention::Quiet);
    }
}
