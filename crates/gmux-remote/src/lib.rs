//! gmux-remote — the M9 remote-tmux client plumbing on top of [`gmux_tmux`]'s sans-io parser:
//!
//! - [`transport`]: spawn the control-mode process (`ssh … tmux -CC new -As gmux`), strip the
//!   `-CC` DCS wrapper on a reader thread, parse the stream into [`gmux_tmux::Event`]s, and
//!   write commands (with typed helpers for the handful gmux sends) to its stdin.
//! - [`convert`]: pure tmux-layout → gmux-split-tree conversion, mapping the n-ary
//!   absolute-size [`gmux_tmux::Cell`] tree onto [`gmux_mux`]'s binary ratio tree.
//!
//! No ssh library, no async: the transport is any child process whose stdio speaks control
//! mode, which is also how the tests work (canned streams piped through stub processes).

pub mod convert;
pub mod transport;

pub use convert::layout_to_node;
pub use transport::{RemoteTmux, TransportEvent};
