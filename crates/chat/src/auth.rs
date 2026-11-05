//! Signed out: signing in, making an account, verifying an email, and
//! resetting a forgotten password — one card, centred, swapping its body.

use icons_lucide::{ARROW_LEFT, MESSAGES_SQUARE};
use idea_ui::{
    tone, typography_kind, variant, Alert, Button, Field, Icon, IdeaThemeRef, Typography,
};
use runtime_core::{
    component, rx, signal, stylesheet, ui, AlignItems, Element, FlexDirection, JustifyContent, KeyboardKind, Length,
    Signal,
};

use crate::app::{signed_in, take_entry, Entry};
use crate::forms::{cb, on_enter, set, Form};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum Screen {
    #[default]
    SignIn,
    SignUp,
    Verify { email: String },
    Forgot,
    Reset { email: String },
}

stylesheet! {
    Backdrop<IdeaThemeRef> {
        base(t) {
            flex_grow: 1.0,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            padding_left: 16.0,
            padding_right: 16.0,
            padding_top: 24.0,
            padding_bottom: 24.0,
            background: t.color.background(),
        }
    }
}

stylesheet! {
    /// The card: the design's 360-wide panel, never wider than the screen.
    CardBox<IdeaThemeRef> {
        base(t) {
            width: 380.0,
            max_width: Length::pct(100.0),
            flex_direction: FlexDirection::Column,
            gap: 16.0,
            padding_left: 32.0,
            padding_right: 32.0,
            padding_top: 32.0,
            padding_bottom: 28.0,
            border_radius: 16.0,
            background: t.color.surface(),
            border_width: 1.0,
            border_color: t.color.border(),
        }
    }
}

stylesheet! {
    Mark<IdeaThemeRef> {
        base(t) {
            width: 48.0,
            height: 48.0,
            border_radius: 14.0,
            align_self: runtime_core::AlignSelf::Center,
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            background: t.intent.primary.solid_bg(),
        }
    }
}

stylesheet! {
    Heading<()> {
        base(_t) {
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::Center,
            gap: 4.0,
            margin_bottom: 4.0,
        }
    }
}

stylesheet! {
    /// The secondary paths under the button: one each side.
    Links<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            flex_wrap: runtime_core::FlexWrap::Wrap,
            gap: 4.0,
        }
    }
}

/// The signed-out screens.
#[component]
pub fn AuthScreens() -> Element {
    let start = match take_entry(|e| matches!(e, Entry::Verify { .. } | Entry::Reset { .. })) {
        Some(Entry::Verify { email, code }) => {
            // Arrived from the email's button: verify straight away.
            let form = Form::new();
            form.submit(chat_api::auth::verify_email(email.clone(), code), signed_in);
            Screen::Verify { email }
        }
        Some(Entry::Reset { email, .. }) => Screen::Reset { email },
        _ => Screen::SignIn,
    };
    let screen = signal(start);
    // The email carries over between screens, so it is typed once.
    let email = signal(String::new());
    ui! {
        view(style = Backdrop()) {
            view(style = CardBox()) {
                view(style = Mark()) { Icon(data = MESSAGES_SQUARE, size = 24.0, color = Some(runtime_core::Color("#FFFFFF".into()))) }
                match screen.get() {
                    Screen::SignIn => { SignIn(screen = screen, email = email) }
                    Screen::SignUp => { SignUp(screen = screen, email = email) }
                    Screen::Verify { email: e } => { Verify(screen = screen, address = e.clone()) }
                    Screen::Forgot => { Forgot(screen = screen, email = email) }
                    Screen::Reset { email: e } => { Reset(screen = screen, address = e.clone()) }
                }
            }
        }
    }
}

#[component]
fn Title(#[prop(static)] title: &'static str, #[prop(static)] subtitle: String) -> Element {
    ui! {
        view(style = Heading()) {
            Typography(content = title.to_string(), kind = typography_kind::H4, align = runtime_core::TextAlign::Center)
            Typography(content = subtitle, muted = true, align = runtime_core::TextAlign::Center, kind = typography_kind::BodySm)
        }
    }
}

/// The error for the whole form, when there is one.
#[component]
fn FormError(#[prop(static)] form: Form) -> Element {
    ui! {
        if form.general.get().is_some() {
            Alert(title = rx!(form.general.get().unwrap_or_default()), tone = tone::Danger, variant = variant::Soft)
        }
    }
}

#[component]
fn SignIn(#[prop(static)] screen: Signal<Screen>, #[prop(static)] email: Signal<String>) -> Element {
    let password = signal(String::new());
    let form = Form::new();
    let submit = cb(move || {
        let address = email.peek();
        // An unverified account goes to the code screen; signing in sent a
        // fresh code.
        form.submit_or(chat_api::auth::sign_in(address, password.peek()), signed_in, move |e| match e {
            server::ServerError::Failed(chat_api::ApiError::Unverified { email }) => {
                screen.set(Screen::Verify { email: email.clone() });
                true
            }
            _ => false,
        });
    });
    ui! {
        view(style = crate::bits::Col()) {
            Title(title = "Welcome back", subtitle = "Sign in to Idea Chat".to_string())
            FormError(form = form)
            Field(
                label = Some("Email".to_string()),
                value = email,
                on_change = set(email),
                keyboard = KeyboardKind::Email,
                autofocus = true,
                error = rx!(form.error("email")),
                on_key_down = on_enter(submit.clone()),
            )
            Field(
                label = Some("Password".to_string()),
                value = password,
                on_change = set(password),
                secure = true,
                error = rx!(form.error("password")),
                on_key_down = on_enter(submit.clone()),
            )
            Button(label = "Sign in".to_string(), on_click = submit, block = true, loading = rx!(form.busy.get()))
            view(style = Links()) {
                Button(label = "Create an account".to_string(), variant = variant::Ghost, size = idea_ui::size::Sm, on_click = cb(move || screen.set(Screen::SignUp)))
                Button(label = "Forgot password?".to_string(), variant = variant::Ghost, tone = tone::Neutral, size = idea_ui::size::Sm, on_click = cb(move || screen.set(Screen::Forgot)))
            }
        }
    }
}

#[component]
fn SignUp(#[prop(static)] screen: Signal<Screen>, #[prop(static)] email: Signal<String>) -> Element {
    let name = signal(String::new());
    let handle = signal(String::new());
    let password = signal(String::new());
    let form = Form::new();
    let submit = cb(move || {
        let address = email.peek();
        form.submit(
            chat_api::auth::sign_up(address, password.peek(), handle.peek(), name.peek()),
            move |done| screen.set(Screen::Verify { email: done.email }),
        );
    });
    // Suggest a username from the name, until one is typed.
    let on_name = crate::forms::cb1(move |v: String| {
        let suggested: String = name
            .peek()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        if handle.peek().is_empty() || handle.peek() == suggested {
            handle.set(v.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.').collect());
        }
        name.set(v);
    });
    ui! {
        view(style = crate::bits::Col()) {
            Title(title = "Create your account", subtitle = "It takes a minute. We'll email you a code.".to_string())
            FormError(form = form)
            Field(label = Some("Email".to_string()), value = email, on_change = set(email), keyboard = KeyboardKind::Email, autofocus = true, error = rx!(form.error("email")))
            Field(label = Some("Display name".to_string()), value = name, on_change = on_name, help = Some("What people see.".to_string()), error = rx!(form.error("display_name")))
            Field(label = Some("Username".to_string()), value = handle, on_change = set(handle), help = Some("For @mentions and finding friends.".to_string()), error = rx!(form.error("handle")))
            Field(
                label = Some("Password".to_string()),
                value = password,
                on_change = set(password),
                secure = true,
                help = Some(format!("At least {} characters.", chat_model::text::PASSWORD_MIN)),
                error = rx!(form.error("password")),
                on_key_down = on_enter(submit.clone()),
            )
            Button(label = "Create account".to_string(), on_click = submit, block = true, loading = rx!(form.busy.get()))
            view(style = Links()) {
                Button(label = "I have an account".to_string(), variant = variant::Ghost, size = idea_ui::size::Sm, leading_icon = Some(ARROW_LEFT), on_click = cb(move || screen.set(Screen::SignIn)))
            }
        }
    }
}

#[component]
fn Verify(#[prop(static)] screen: Signal<Screen>, #[prop(static)] address: String) -> Element {
    let code = signal(String::new());
    let form = Form::new();
    let sent = signal(false);
    // The code is passed in rather than read back: `Signal::set` stages, so
    // a read in the same handler as the set still sees the old value.
    let verify: std::rc::Rc<dyn Fn(String)> = {
        let address = address.clone();
        std::rc::Rc::new(move |c: String| form.submit(chat_api::auth::verify_email(address.clone(), c), signed_in))
    };
    let submit = {
        let verify = verify.clone();
        cb(move || verify(code.peek()))
    };
    // Six digits typed: no need to press anything.
    let on_code = crate::forms::cb1(move |v: String| {
        let digits: String = v.chars().filter(|c| c.is_ascii_digit()).take(6).collect();
        code.set(digits.clone());
        if digits.len() == 6 {
            verify(digits);
        }
    });
    let resend = {
        let address = address.clone();
        cb(move || form.submit(chat_api::auth::resend_code(address.clone()), move |_| sent.set(true)))
    };
    ui! {
        view(style = crate::bits::Col()) {
            Title(title = "Check your email", subtitle = format!("We sent a 6-digit code to {address}."))
            FormError(form = form)
            if sent.get() {
                Alert(title = "A new code is on its way.".to_string(), tone = tone::Success, variant = variant::Soft)
            }
            Field(
                label = Some("Code".to_string()),
                value = code,
                on_change = on_code,
                keyboard = KeyboardKind::Number,
                autofocus = true,
                placeholder = Some("123456".to_string()),
                error = rx!(form.error("code")),
                on_key_down = on_enter(submit.clone()),
            )
            Button(label = "Verify".to_string(), on_click = submit, block = true, loading = rx!(form.busy.get()))
            view(style = Links()) {
                Button(label = "Back".to_string(), variant = variant::Ghost, tone = tone::Neutral, size = idea_ui::size::Sm, leading_icon = Some(ARROW_LEFT), on_click = cb(move || screen.set(Screen::SignIn)))
                Button(label = "Send a new code".to_string(), variant = variant::Ghost, size = idea_ui::size::Sm, on_click = resend)
            }
        }
    }
}

#[component]
fn Forgot(#[prop(static)] screen: Signal<Screen>, #[prop(static)] email: Signal<String>) -> Element {
    let form = Form::new();
    let submit = cb(move || {
        let address = email.peek();
        let next = address.clone();
        form.submit(chat_api::auth::forgot_password(address), move |_| screen.set(Screen::Reset { email: next }));
    });
    ui! {
        view(style = crate::bits::Col()) {
            Title(title = "Reset your password", subtitle = "We'll email you a code to choose a new one.".to_string())
            FormError(form = form)
            Field(label = Some("Email".to_string()), value = email, on_change = set(email), keyboard = KeyboardKind::Email, autofocus = true, error = rx!(form.error("email")), on_key_down = on_enter(submit.clone()))
            Button(label = "Email me a code".to_string(), on_click = submit, block = true, loading = rx!(form.busy.get()))
            view(style = Links()) {
                Button(label = "Back to sign in".to_string(), variant = variant::Ghost, tone = tone::Neutral, size = idea_ui::size::Sm, leading_icon = Some(ARROW_LEFT), on_click = cb(move || screen.set(Screen::SignIn)))
            }
        }
    }
}

#[component]
fn Reset(#[prop(static)] screen: Signal<Screen>, #[prop(static)] address: String) -> Element {
    let code = signal(String::new());
    let password = signal(String::new());
    let form = Form::new();
    let submit = {
        let address = address.clone();
        cb(move || form.submit(chat_api::auth::reset_password(address.clone(), code.peek(), password.peek()), signed_in))
    };
    ui! {
        view(style = crate::bits::Col()) {
            Title(title = "Choose a new password", subtitle = format!("Enter the code we sent to {address}."))
            FormError(form = form)
            Field(label = Some("Code".to_string()), value = code, on_change = set(code), keyboard = KeyboardKind::Number, autofocus = true, error = rx!(form.error("code")))
            Field(
                label = Some("New password".to_string()),
                value = password,
                on_change = set(password),
                secure = true,
                help = Some(format!("At least {} characters. This signs you out everywhere else.", chat_model::text::PASSWORD_MIN)),
                error = rx!(form.error("password")),
                on_key_down = on_enter(submit.clone()),
            )
            Button(label = "Set password and sign in".to_string(), on_click = submit, block = true, loading = rx!(form.busy.get()))
            view(style = Links()) {
                Button(label = "Back to sign in".to_string(), variant = variant::Ghost, tone = tone::Neutral, size = idea_ui::size::Sm, leading_icon = Some(ARROW_LEFT), on_click = cb(move || screen.set(Screen::SignIn)))
            }
        }
    }
}
