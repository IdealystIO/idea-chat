//! What every form shares: a busy flag, errors per field, an error for the
//! whole form, and Enter to submit.

use std::collections::BTreeMap;
use std::future::Future;
use std::rc::Rc;

use chat_api::{Answer, Failure};
use runtime_core::{signal, KeyEvent, KeyOutcome, Signal};

/// One form's state. `Copy`: all signals.
#[derive(Clone, Copy)]
pub struct Form {
    pub busy: Signal<bool>,
    pub errors: Signal<BTreeMap<String, String>>,
    pub general: Signal<Option<String>>,
}

impl Form {
    pub fn new() -> Form {
        Form { busy: signal(false), errors: signal(BTreeMap::new()), general: signal(None) }
    }

    /// The error for one field, read live (for a `Field`'s `error` prop).
    pub fn error(&self, field: &str) -> Option<String> {
        self.errors.get().get(field).cloned()
    }

    pub fn clear(&self) {
        self.errors.set(BTreeMap::new());
        self.general.set(None);
    }

    /// File a failure: on its field if it names one, else on the form.
    pub fn fail(&self, e: &Failure) {
        let message = chat_api::describe(e);
        match chat_api::failed_field(e) {
            Some(f) => {
                let mut all = BTreeMap::new();
                all.insert(f.to_string(), message);
                self.errors.set(all);
                self.general.set(None);
            }
            None => {
                self.errors.set(BTreeMap::new());
                self.general.set(Some(message));
            }
        }
    }

    /// Run a request: busy while it runs, errors filed if it fails, `ok`
    /// with the answer if it does not. Ignored while one is already running.
    pub fn submit<T: 'static>(&self, request: impl Future<Output = Answer<T>> + 'static, ok: impl FnOnce(T) + 'static) {
        self.submit_or(request, ok, |_| false);
    }

    /// As [`Form::submit`], with first refusal on a failure: `handled`
    /// returning `true` means it dealt with it and nothing is filed.
    pub fn submit_or<T: 'static>(
        &self,
        request: impl Future<Output = Answer<T>> + 'static,
        ok: impl FnOnce(T) + 'static,
        handled: impl FnOnce(&Failure) -> bool + 'static,
    ) {
        if self.busy.peek() {
            return;
        }
        self.clear();
        self.busy.set(true);
        let form = *self;
        runtime_core::spawn_then(request, move |result| {
            form.busy.set(false);
            match result {
                Ok(v) => ok(v),
                Err(e) => {
                    if !handled(&e) {
                        form.fail(&e);
                    }
                }
            }
        });
    }
}

impl Default for Form {
    fn default() -> Self {
        Form::new()
    }
}

/// A key handler that runs `f` on Enter (without Shift).
pub fn on_enter(f: Rc<dyn Fn()>) -> Option<Rc<dyn Fn(&KeyEvent) -> KeyOutcome>> {
    Some(Rc::new(move |k: &KeyEvent| {
        if k.key == "Enter" && !k.shift {
            f();
            KeyOutcome::PreventDefault
        } else {
            KeyOutcome::Default
        }
    }))
}

/// A typed callback for a prop, which `Rc::new` on a bare closure is not.
pub fn cb(f: impl Fn() + 'static) -> Rc<dyn Fn()> {
    Rc::new(f)
}

/// The same, taking a value.
pub fn cb1<T>(f: impl Fn(T) + 'static) -> Rc<dyn Fn(T)> {
    Rc::new(f)
}

/// A setter for a text signal, for a field's `on_change`.
pub fn set(s: Signal<String>) -> Rc<dyn Fn(String)> {
    Rc::new(move |v| s.set(v))
}

/// Say a request failed, as a toast.
pub fn toast(e: &Failure) {
    idea_ui::push_toast(chat_api::describe(e), idea_ui::tone::Danger);
}

/// The monospace family: the platform's own code font.
pub fn mono() -> runtime_core::FontFamily {
    runtime_core::FontFamily::System("ui-monospace, SFMono-Regular, Menlo, Consolas, monospace".into())
}
