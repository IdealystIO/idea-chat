//! The code-splitting boundary for noise suppression.
//!
//! DeepFilterNet is not a small dependency: `tract` plus the model plumbing is
//! about **6.8 MB** of the release wasm, which is more than twice the rest of
//! the app put together. Suppression is off by default, so paying for it on
//! every first visit — including the visits that never open a voice room —
//! is the wrong default by a wide margin.
//!
//! So it lives in a lazily-loaded chunk, and this component is the door.
//! `#[lazy_component]` compiles the body into a separate wasm module and ships
//! a loader in its place; everything reachable *only* from here goes with it.
//!
//! # The rule that makes it work
//!
//! There is exactly one path from the app into the inference engine:
//! [`crate::voice::engage`] and [`crate::voice::install_denoise_probe`], both
//! called only from this body. Reachability is what the splitter computes, so a
//! second caller anywhere — a convenience wrapper, a settings handler that
//! "just" builds the model, a probe installed at boot — pulls the whole engine
//! back into the main bundle. Nothing errors; the chunk simply stops being
//! worth having. `voice::set_suppression` handles every other mode precisely so
//! that it never needs to reach this one.
//!
//! # When it mounts
//!
//! Whenever the setting is "This device" — call or no call. Choosing it starts
//! the chunk fetch and the model download immediately, so the first call after
//! choosing is not also the one that waits for 8 MB of weights. The switch key
//! includes the call's identity, so starting a call remounts this and engages
//! the model for that call.

use runtime_core::{lazy_component, switch, ui, Element};
use voice_dsp::Mode;

use crate::model::App;

/// Mount the suppression chunk while it is wanted.
///
/// Rendered once, high in the shell, and draws nothing — this is a lifecycle
/// hook wearing a component's clothes, because a component is the unit the
/// splitter works on.
pub fn suppression_boundary(app: App) -> Element {
    switch(
        // `voice_live` and `in_voice` are in the key so that starting a call —
        // or moving to another room — remounts the body and engages the model
        // for the call that is actually up. The chunk is already loaded by
        // then, so a remount costs a model build and nothing else.
        move || (app.suppress.get() == Mode::Client, app.voice_live.get(), app.in_voice.get()),
        move |(wanted, live, _room)| {
            if *wanted {
                ui! { Suppressor(app = app, engage = *live) }
            } else {
                ui! { view {} }
            }
        },
    )
}

/// The chunk boundary. Everything this reaches ships separately.
///
/// `engage` rather than reading `app.voice_live` inside: props are what cross
/// the split, and a body that read the signal itself would still be correct but
/// would hide the dependency that `suppression_boundary`'s switch key encodes.
#[lazy_component]
fn Suppressor(#[prop(static)] app: App, #[prop(static)] engage: bool) -> Element {
    // The benchmark seam, which also has to live on this side of the split.
    crate::voice::install_denoise_probe();

    if engage {
        crate::voice::engage(app);
    } else {
        // No call yet. Fetching the weights now is the whole reason this
        // component mounts on the setting rather than on the call: it turns
        // "your first call takes four seconds to connect" into "the setting
        // took a moment to apply", which is a much better place to spend it.
        crate::voice::warm(app);
    }

    // Draws nothing. The suppression it turns on is audible, not visible.
    ui! { view {} }
}
