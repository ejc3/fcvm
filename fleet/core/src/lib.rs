//! fcvm fleet core: the shared state of the GPU wall, the events that change it and the
//! one pure function that applies them.
//!
//! Nothing in this crate does I/O, reads a clock or draws a random number. Time arrives
//! inside each [`Event`]. Two viewers see the same wall because there is one sequence of
//! events and one [`apply`] over it; the tests replay recorded sequences
//! (`tests/vectors/*.json`) and compare the states.

pub mod apply;
pub mod budget;
pub mod director;
pub mod event;
pub mod history;
pub mod protocol;
pub mod scene;
pub mod state;
pub mod tiling;
pub mod view;

pub use apply::{apply, precheck_word};
pub use event::{Event, Intent};
pub use state::{Config, Outcome, Receipt, State, TextCheck};
pub use view::view;
