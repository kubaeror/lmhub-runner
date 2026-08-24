//! Per-screen reducers: `State` methods that fold [`Action`]s into state
//! mutations plus the [`Effect`]s the event loop should run.
//!
//! `State::reduce` (in `reduce.rs`) is the single entry point; it is a thin
//! dispatcher that routes each action to the module that owns it. Each
//! reducer is deliberately "pure-ish": it mutates state and returns effects,
//! but never spawns tasks or does blocking IO itself — those are effects.

pub(crate) mod global;
pub(crate) mod history;
pub(crate) mod map;
pub(crate) mod run;
pub(crate) mod setup;

pub(crate) use global::PaletteCmd;
