//! The app's emails: idealyst components rendered to inline-styled HTML (with
//! a plain-text alternative) by the email backend, then sent through SES — or,
//! in development, printed to the log.

use std::future::Future;
use std::pin::Pin;

use chat_api::srv::Mail;
use idea_ui_mail::{Button, Divider, EmailBody, EmailContainer, Heading, Section, Text};
use runtime_core::{component, ui, Element};

/// The accent from the app's theme, so the button matches the app.
const ACCENT: &str = "#4A78D6";
const MUTED: &str = "#6B6B66";

/// How mail leaves the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// Print each message, codes and links included, to stdout.
    Log,
    /// Send through the provider `email::configure`d at start.
    Send,
}

pub struct Mailer {
    pub delivery: Delivery,
    pub from: email::Mailbox,
}

/// One email: a heading, what happened, the code to type, a button that
/// does the same, and why they got it.
#[component]
fn Letter(
    heading: String,
    body: String,
    code: Option<String>,
    action: Option<String>,
    href: String,
    footer: String,
) -> Element {
    let heading = heading.get();
    let body = body.get();
    let code = code.get();
    let action = action.get();
    let href = href.get();
    let footer = footer.get();
    ui! {
        EmailBody(background = "#F7F7F5".to_string()) {
            EmailContainer(width = 520.0, radius = 16.0, background = "#FFFFFF".to_string()) {
                Section(padding = 36.0, gap = 18.0) {
                    Text(content = "Idea Chat".to_string(), size = 14.0, color = ACCENT.to_string())
                    Heading(content = heading, size = 22.0)
                    Text(content = body, size = 15.0)
                    if let Some(code) = code.clone() {
                        Heading(content = code, size = 32.0, center = true)
                    }
                    if let Some(label) = action.clone() {
                        Button(label = label, href = href.clone(), background = ACCENT.to_string(), color = "#FFFFFF".to_string(), radius = 8.0)
                    }
                    Divider()
                    Text(content = footer, size = 13.0, color = MUTED.to_string())
                }
            }
        }
    }
}

fn compose(mail: Mail, from: email::Mailbox) -> email::Email {
    let (to, subject, heading, body, code, action, footer) = match mail {
        Mail::Verify { to, name, code, link } => (
            to,
            format!("{code} is your Idea Chat code"),
            format!("Welcome, {name}"),
            "Enter this code in Idea Chat to confirm your email address. It works for 15 minutes.".to_string(),
            Some(code),
            Some(("Confirm my email".to_string(), link)),
            "You're getting this because someone signed up for Idea Chat with this address. \
             If that wasn't you, ignore it — no account is made without this code."
                .to_string(),
        ),
        Mail::Reset { to, name, code, link } => (
            to,
            format!("{code} resets your Idea Chat password"),
            format!("Reset your password, {name}"),
            "Enter this code to choose a new password. It works for 15 minutes, and signs you out everywhere else."
                .to_string(),
            Some(code),
            Some(("Choose a new password".to_string(), link)),
            "If you didn't ask to reset your password, ignore this email; your password stays as it is.".to_string(),
        ),
        Mail::PasswordChanged { to, name } => (
            to,
            "Your Idea Chat password was changed".to_string(),
            format!("Password changed, {name}"),
            "Your password was just changed, and every other session was signed out.".to_string(),
            None,
            None,
            "If this wasn't you, reset your password straight away from the sign-in screen.".to_string(),
        ),
    };
    email::Email::to(to.as_str()).from(from).subject(subject).template(move || {
        let (action, href) = match action {
            Some((label, href)) => (Some(label), href),
            None => (None, String::new()),
        };
        ui! { Letter(heading = heading, body = body, code = code, action = action, href = href, footer = footer) }
    })
}

impl chat_api::srv::Mailer for Mailer {
    fn send(&self, mail: Mail) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let email = compose(mail, self.from.clone());
        Box::pin(async move {
            match self.delivery {
                Delivery::Log => {
                    let to: Vec<&str> = email.to.iter().map(|m| m.address.as_str()).collect();
                    println!(
                        "[mail] to {} — {}\n{}\n[mail] end",
                        to.join(", "),
                        email.subject,
                        email.text.as_deref().unwrap_or("")
                    );
                    Ok(())
                }
                Delivery::Send => email::send(email).await.map(|_| ()).map_err(|e| e.to_string()),
            }
        })
    }
}
