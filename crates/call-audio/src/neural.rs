//! Where the neural suppression stage comes from.
//!
//! DeepFilterNet is heavy — `tract` and the model plumbing are several
//! megabytes of wasm, more than the rest of an app — so whether it is linked
//! in is the app's choice, not this crate's:
//!
//! - With the `neural` feature (on by default) this crate brings its own
//!   (`denoise`), which fetches its model from the path the sender is given.
//! - Without it, the app installs a builder with [`install_neural`] — from a
//!   lazily loaded chunk, say, so the engine downloads only when someone
//!   turns suppression on. Until one is installed, choosing
//!   `Suppression::Neural` fails with a message saying so, and nothing else
//!   changes.
//!
//! An installed builder wins over the built-in one.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use media_stream::AudioStream;

use crate::ModelSettings;

/// A running neural stage: what it emits, and how far behind its input that
/// is, in samples at the stream's rate (48 kHz for DeepFilterNet). Dropping
/// `clean` stops it.
pub struct NeuralStage {
    pub clean: AudioStream,
    pub latency: usize,
}

type Boxed<T> = Pin<Box<dyn Future<Output = Result<T, String>>>>;

/// Builds a stage reading the given microphone stream, tuned by the settings.
pub type BuildNeural = Rc<dyn Fn(AudioStream, ModelSettings) -> Boxed<NeuralStage>>;

/// Gets ready to build — fetches a model, say — so a join can fail before
/// anything is opened. Optional.
pub type PrepareNeural = Rc<dyn Fn() -> Boxed<()>>;

thread_local! {
    static INSTALLED: RefCell<Option<(BuildNeural, Option<PrepareNeural>)>> = const { RefCell::new(None) };
}

/// Use this builder for `Suppression::Neural` from now on.
pub fn install_neural(build: BuildNeural, prepare: Option<PrepareNeural>) {
    INSTALLED.with(|i| *i.borrow_mut() = Some((build, prepare)));
}

/// Whether a neural stage can be built now: one installed, or built in.
pub fn neural_available() -> bool {
    cfg!(feature = "neural") || INSTALLED.with(|i| i.borrow().is_some())
}

/// Get the neural stage ready, so a join that will need it fails early.
/// `model_path` is for the built-in stage.
pub async fn prepare_neural(model_path: &'static str) -> Result<(), String> {
    let installed = INSTALLED.with(|i| i.borrow().clone());
    match installed {
        Some((_, Some(prepare))) => prepare().await,
        Some((_, None)) => Ok(()),
        None => builtin::prepare(model_path).await,
    }
}

pub(crate) async fn build(mic: AudioStream, model: ModelSettings, model_path: &'static str) -> Result<NeuralStage, String> {
    let installed = INSTALLED.with(|i| i.borrow().as_ref().map(|(b, _)| b.clone()));
    match installed {
        Some(build) => build(mic, model).await,
        None => builtin::build(mic, model, model_path).await,
    }
}

#[cfg(feature = "neural")]
mod builtin {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::NeuralStage;
    use crate::{ModelSettings, UNLIMITED_DB};
    use media_stream::AudioStream;

    /// DeepFilterNet 3's latency at 48 kHz, in samples: three 10 ms hops.
    /// Measured natively by cross-correlating the model's output against its
    /// input.
    const DFN_LATENCY: usize = 1440;

    thread_local! {
        /// Fetched models, by path, for the life of the page. Leaked once
        /// each, on purpose: `Denoiser::with_weights` needs `'static`, and a
        /// model outlives every call made with it.
        static MODELS: RefCell<HashMap<&'static str, &'static [u8]>> = RefCell::new(HashMap::new());
    }

    /// Fetch the model, or return the copy already fetched. The size check
    /// catches a missing file served as an HTML fallback page, which would
    /// otherwise surface as an opaque model-init error.
    async fn weights(path: &'static str) -> Result<&'static [u8], String> {
        if let Some(cached) = MODELS.with(|m| m.borrow().get(path).copied()) {
            return Ok(cached);
        }
        let bytes = net::Client::new()
            .get(path)
            .send()
            .await
            .map_err(|e| format!("could not fetch the DeepFilterNet model: {e}"))?
            .bytes()
            .await
            .map_err(|e| format!("could not read the DeepFilterNet model: {e}"))?;
        if bytes.len() < 1_000_000 {
            return Err(format!(
                "the model at {path} is only {} bytes — run tools/fetch-model.sh and rebuild",
                bytes.len()
            ));
        }
        let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        MODELS.with(|m| m.borrow_mut().insert(path, leaked));
        Ok(leaked)
    }

    pub async fn prepare(path: &'static str) -> Result<(), String> {
        weights(path).await.map(|_| ())
    }

    pub async fn build(mic: AudioStream, model: ModelSettings, path: &'static str) -> Result<NeuralStage, String> {
        let mut denoiser = denoise::Denoiser::with_weights(weights(path).await?)
            .post_filter(model.post_filter)
            .thresholds(model.min_db_thresh, model.max_db_erb_thresh, model.max_db_df_thresh);
        if model.max_reduction_db < UNLIMITED_DB {
            denoiser = denoiser.attenuation_limit_db(model.max_reduction_db);
        }
        let clean = denoiser.process(&mic).await.map_err(|e| format!("DeepFilterNet could not start: {e}"))?;
        Ok(NeuralStage { clean, latency: DFN_LATENCY })
    }
}

#[cfg(not(feature = "neural"))]
mod builtin {
    use super::NeuralStage;
    use crate::ModelSettings;
    use media_stream::AudioStream;

    const NOT_LOADED: &str = "noise suppression is not loaded yet";

    pub async fn prepare(_path: &'static str) -> Result<(), String> {
        Err(NOT_LOADED.to_string())
    }

    pub async fn build(_mic: AudioStream, _model: ModelSettings, _path: &'static str) -> Result<NeuralStage, String> {
        Err(NOT_LOADED.to_string())
    }
}
