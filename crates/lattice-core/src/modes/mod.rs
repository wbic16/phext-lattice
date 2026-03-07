/// Pluggable editor modes.
///
/// Each mode interprets keystrokes differently.
/// VimMode is the default. Others can be added by implementing EditorMode.

pub mod vim;

pub use vim::VimMode;
