//! Everything you see before you are signed in.
//!
//! The design never drew these — it starts you already inside a space — so the
//! visual language here is borrowed rather than invented: the same tokens, the
//! same idea-ui components, a single centred card. The tone is the product's:
//! it says what will happen and does not sell anything.
//!
//! Four screens, chosen by [`Phase`], because they are mutually exclusive and
//! a boolean per screen would eventually render two at once.

use icons_lucide::{ALERT_TRIANGLE, FLAME};
use idea_ui::{
    shape, size, tone, typography_kind, variant, Button, Field, Stack, StackAlign, StackAxis,
    StackGap, StackPadding, ToneRef, Typography, VariantRef,
};
use runtime_core::{component, switch, ui, Element, StyleApplication};

use crate::model::{App, AuthTab, Phase};
use crate::net;
use crate::styles;
use crate::views::bits::{cb, cb1, Glyph, Muted, Strong};

#[component]
pub fn AuthScreen(#[prop(static)] app: App) -> Element {
    switch(
        move || (app.phase.get(), app.auth_tab.get(), app.busy.get(), app.notice.get()),
        move |_| auth_body(app),
    )
}

fn auth_body(app: App) -> Element {
    let card: Element = match app.phase.get() {
        Phase::Connecting => connecting(),
        Phase::Unreachable(why) => unreachable(app, why),
        Phase::NeedsClaim => claim_form(app),
        _ => sign_in_form(app),
    };

    ui! {
        view(style = || StyleApplication::new(styles::AuthPage::sheet())) {
            view(style = || StyleApplication::new(styles::AuthCard::sheet())) {
                card
            }
        }
    }
}

/// The first frame, before the server has answered.
///
/// Says nothing but the product's name. A spinner here would be a lie about
/// how long this takes on a local instance, and an error would be a lie about
/// what we know.
fn connecting() -> Element {
    ui! {
        Stack(gap = StackGap::Sm, align = StackAlign::Center, padding = StackPadding::Xl) {
            Glyph(data = Some(FLAME), size = 30.0)
            Muted(label = "Connecting…".to_string(), kind = typography_kind::BodySm)
        }
    }
}

/// The server could not be reached at all.
///
/// Distinct from a failed sign-in, and it matters: the answer is "the server
/// is not there", not "your password is wrong". Telling someone to check their
/// password when the process is down is how support tickets are made.
fn unreachable(app: App, why: String) -> Element {
    ui! {
        Stack(gap = StackGap::Md, padding = StackPadding::Lg) {
            Stack(axis = StackAxis::Row, gap = StackGap::Sm, align = StackAlign::Center) {
                Glyph(data = Some(ALERT_TRIANGLE), size = 20.0)
                Typography(content = "Can't reach Hearth".to_string(), kind = typography_kind::H3)
            }
            Muted(label = why)
            Muted(
                label = "The server may still be starting. Check that it is running, then try again."
                    .to_string(),
                kind = typography_kind::BodySm,
            )
            Stack(axis = StackAxis::Row, align = StackAlign::Start) {
                Button(
                    label = "Try again".to_string(),
                    tone = tone::Primary,
                    variant = variant::Filled,
                    size = size::Md,
                    shape = shape::Md,
                    on_click = cb(move || {
                        app.phase.set(Phase::Connecting);
                        net::start(app);
                    }),
                )
            }
        }
    }
}

/// Claim a fresh instance.
///
/// Shown only when the server reports no accounts. The claim link is in the
/// operator's terminal — this asks them to paste it, which is the one moment
/// where a copy-paste is better than a redirect: it proves they can read the
/// server's output, which is the only thing standing between a fresh instance
/// and whoever finds it first.
fn claim_form(app: App) -> Element {
    let busy = app.busy.get();
    let notice_row = notice(app);
    ui! {
        Stack(gap = StackGap::Lg, padding = StackPadding::Lg) {
            Stack(gap = StackGap::Xs) {
                Typography(
                    content = "Claim this Hearth".to_string(),
                    kind = typography_kind::H2,
                )
                Muted(
                    label = "Nobody has an account here yet. The server printed a one-time link when it started — paste it below to make the first one."
                        .to_string(),
                )
            }

            Field(
                label = Some("Claim link".to_string()),
                value = app.form_claim_token,
                on_change = cb1(move |v: String| {
                    // Accept the whole URL or just the token; people paste
                    // whichever they can select from a terminal.
                    app.form_claim_token.set(net::invite_code_of(&v))
                }),
                placeholder = Some("http://localhost:8090/claim/…".to_string()),
            )
            Field(
                label = Some("Handle".to_string()),
                value = app.form_handle,
                on_change = cb1(move |v: String| app.form_handle.set(v)),
                placeholder = Some("ana".to_string()),
                help = Some("Lowercase letters, numbers and underscores. This one is permanent.".to_string()),
            )
            Field(
                label = Some("Display name".to_string()),
                value = app.form_display_name,
                on_change = cb1(move |v: String| app.form_display_name.set(v)),
                placeholder = Some("Ana Ruiz".to_string()),
            )
            Field(
                label = Some("Password".to_string()),
                value = app.form_password,
                on_change = cb1(move |v: String| app.form_password.set(v)),
                secure = true,
                help = Some("At least ten characters.".to_string()),
            )

            notice_row

            Button(
                label = if busy { "Claiming…".to_string() } else { "Claim this instance".to_string() },
                tone = tone::Primary,
                variant = variant::Filled,
                size = size::Md,
                shape = shape::Md,
                block = true,
                disabled = busy,
                on_click = cb(move || net::claim(app)),
            )
        }
    }
}

/// Sign in, or create an account from an invite.
fn sign_in_form(app: App) -> Element {
    let busy = app.busy.get();
    let tab = app.auth_tab.get();
    let joining = tab == AuthTab::Invite;
    let instance = app.instance_name.get();
    let notice_row = notice(app);

    // The invite path needs everything sign-in needs plus an account to make.
    let invite_fields: Vec<Element> = if joining {
        vec![ui! {
            Stack(gap = StackGap::Md) {
                Field(
                    label = Some("Invite link".to_string()),
                    value = app.form_invite,
                    on_change = cb1(move |v: String| app.form_invite.set(net::invite_code_of(&v))),
                    placeholder = Some("quiet-porch-42".to_string()),
                )
                Field(
                    label = Some("Handle".to_string()),
                    value = app.form_handle,
                    on_change = cb1(move |v: String| app.form_handle.set(v)),
                    placeholder = Some("theo".to_string()),
                    help = Some("Lowercase letters, numbers and underscores. This one is permanent.".to_string()),
                )
                Field(
                    label = Some("Display name".to_string()),
                    value = app.form_display_name,
                    on_change = cb1(move |v: String| app.form_display_name.set(v)),
                    placeholder = Some("Theo Lang".to_string()),
                )
            }
        }]
    } else {
        vec![ui! {
            Field(
                label = Some("Handle or email".to_string()),
                value = app.form_identifier,
                on_change = cb1(move |v: String| app.form_identifier.set(v)),
                placeholder = Some("ana".to_string()),
            )
        }]
    };

    ui! {
        Stack(gap = StackGap::Lg, padding = StackPadding::Lg) {
            Stack(gap = StackGap::Xs) {
                Stack(axis = StackAxis::Row, gap = StackGap::Sm, align = StackAlign::Center) {
                    Glyph(data = Some(FLAME), size = 22.0)
                    Typography(content = instance, kind = typography_kind::H2)
                }
                Muted(label = "A small, quiet place to talk.".to_string())
            }

            Stack(axis = StackAxis::Row, gap = StackGap::Sm) {
                Button(
                    label = "Sign in".to_string(),
                    tone = if joining { ToneRef::from(tone::Neutral) } else { ToneRef::from(tone::Primary) },
                    variant = if joining {
                        VariantRef::from(variant::Ghost)
                    } else {
                        VariantRef::from(variant::Filled)
                    },
                    shape = shape::Md,
                    block = true,
                    on_click = cb(move || {
                        app.auth_tab.set(AuthTab::SignIn);
                        app.notice.set(String::new());
                    }),
                )
                Button(
                    label = "I have an invite".to_string(),
                    tone = if joining { ToneRef::from(tone::Primary) } else { ToneRef::from(tone::Neutral) },
                    variant = if joining {
                        VariantRef::from(variant::Filled)
                    } else {
                        VariantRef::from(variant::Ghost)
                    },
                    shape = shape::Md,
                    block = true,
                    on_click = cb(move || {
                        app.auth_tab.set(AuthTab::Invite);
                        app.notice.set(String::new());
                    }),
                )
            }

            invite_fields

            Field(
                label = Some("Password".to_string()),
                value = app.form_password,
                on_change = cb1(move |v: String| app.form_password.set(v)),
                secure = true,
            )

            notice_row

            Button(
                label = if busy {
                    "One moment…".to_string()
                } else if joining {
                    "Create account".to_string()
                } else {
                    "Sign in".to_string()
                },
                tone = tone::Primary,
                variant = variant::Filled,
                size = size::Md,
                shape = shape::Md,
                block = true,
                disabled = busy,
                on_click = cb(move || if joining { net::register(app) } else { net::sign_in(app) }),
            )
        }
    }
}

/// The line under a form — an error, or what is happening.
///
/// Occupies no space when there is nothing to say, rather than reserving a
/// blank row: a form that shifts down when it fails draws the eye to the
/// movement instead of to the message.
fn notice(app: App) -> Vec<Element> {
    let text = app.notice.get();
    if text.is_empty() {
        return Vec::new();
    }
    vec![ui! {
        view(style = || StyleApplication::new(styles::Notice::sheet())) {
            Strong(label = text, kind = typography_kind::BodySm)
        }
    }]
}
