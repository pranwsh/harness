//! Pure TUI state: transcript, input buffer, wrapping, and the renderer
//! seam. Knows nothing about terminals, event loops, or markdown engines.
//!
//! Layout of concerns:
//! - [`wrap`]: display-width-aware wrapping shared by editor and renderers.
//! - [`editor`]: multi-line input buffer with visual cursor motion.
//! - [`app`]: chat transcript, scroll position, and quit flag.
//! - [`render`]: [`MessageRenderer`] trait decoupling views from engines.

pub mod app;
pub mod editor;
pub mod render;
pub mod wrap;
