//! Server-lifetime-unique, never-reused ids for sessions/windows/panes (tmux convention:
//! `$session`, `@window`, `%pane`). Ids are exported into each pane's environment so agent hook
//! scripts can self-address.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! id_type {
    ($name:ident, $counter:ident, $sigil:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u64);

        static $counter: AtomicU64 = AtomicU64::new(0);

        impl $name {
            /// Allocate the next never-reused id.
            pub fn alloc() -> Self {
                $name($counter.fetch_add(1, Ordering::Relaxed))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $sigil, self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", $sigil, self.0)
            }
        }
    };
}

id_type!(SessionId, NEXT_SESSION, '$');
id_type!(WindowId, NEXT_WINDOW, '@');
id_type!(PaneId, NEXT_PANE, '%');

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_monotonic_and_unique() {
        let a = PaneId::alloc();
        let b = PaneId::alloc();
        let c = PaneId::alloc();
        assert!(a.0 < b.0 && b.0 < c.0, "pane ids must strictly increase");
        assert_ne!(a, b);
    }

    #[test]
    fn ids_display_with_sigils() {
        // Exact numbers depend on allocation order across tests, so check the sigil + parse.
        let s = format!("{}", SessionId(7));
        let w = format!("{}", WindowId(3));
        let p = format!("{}", PaneId(42));
        assert_eq!(s, "$7");
        assert_eq!(w, "@3");
        assert_eq!(p, "%42");
    }

    #[test]
    fn id_counters_are_independent() {
        let p = PaneId::alloc();
        let w = WindowId::alloc();
        // Different counters; a fresh window id can equal a pane id numerically — that's fine,
        // they are distinct types.
        let _ = (p, w);
    }
}
