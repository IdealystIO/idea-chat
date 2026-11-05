//! The client's side of the wire.
//!
//! Every call the UI makes to the server goes through here, so a view never
//! writes `spawn_async` or touches a `ServerError`. The pattern throughout: a
//! synchronous fn the UI can call from a press handler, which spawns the
//! request and writes the answer into signals.
//!
//! Compiled only on the client build — the server binary calls the function
//! bodies directly and has no use for their stubs.

use runtime_core::driver::spawn_async;
use server::ServerError;

use crate::api;
use crate::link::{self, Landing};
use crate::model::{App, AuthTab, JoinTab, Phase, Scope};

/// Point the server-fn client at the right origin.
///
/// On web that is the page's own origin, which is what makes the browser
/// attach the httpOnly session cookie — a call to any other host silently
/// arrives unauthenticated. On native there is no cookie jar, so a credential
/// provider announces the client kind (which is how `login` knows to return a
/// bearer instead of setting a cookie) and attaches the stored token.
pub fn configure() {
    #[cfg(target_arch = "wasm32")]
    {
        let origin = web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_else(|| "http://127.0.0.1:8090".to_string());
        server::configure(server::ClientConfig::new(origin));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        server::configure(server::ClientConfig::new(native_server_url()).with_credentials(
            server::credentials_from_fn(|| {
                let mut headers = vec![("x-idealyst-client".to_string(), "native".to_string())];
                if let Some(token) = keystore().get("session").ok().flatten() {
                    headers.push(("authorization".to_string(), format!("Bearer {token}")));
                }
                headers
            }),
        ));
    }
}

/// Where a native build looks for the server.
///
/// `10.0.2.2` is the Android emulator's alias for the host's localhost. On a
/// physical device this needs the dev machine's LAN address, which is why it
/// reads an env var rather than being a constant.
#[cfg(not(target_arch = "wasm32"))]
fn native_server_url() -> String {
    std::env::var("HEARTH_SERVER").unwrap_or_else(|_| "http://10.0.2.2:8090".to_string())
}

#[cfg(not(target_arch = "wasm32"))]
fn keystore() -> std::sync::Arc<dyn credentials::Credentials> {
    use std::sync::{Arc, OnceLock};
    static C: OnceLock<Arc<dyn credentials::Credentials>> = OnceLock::new();
    C.get_or_init(|| credentials::platform_credentials("hearth")).clone()
}

/// Persist a bearer token. No-op on web, where the cookie is the mechanism and
/// nothing is stored anywhere JS can read.
fn remember_token(_token: &Option<String>) {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(token) = _token {
        let _ = keystore().set("session", token);
    }
}

fn forget_token() {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = keystore().remove("session");
    }
}

/// Flatten a server error into something worth showing a person.
///
/// `Failed` carries copy the server wrote for exactly this; anything else is a
/// transport or protocol fault, which is not the user's problem to read.
pub fn message(e: ServerError) -> String {
    match e {
        ServerError::Failed(m) => m,
        _ => "Could not reach the server.".to_string(),
    }
}

// ===========================================================================
// Boot
// ===========================================================================

/// Work out what screen to show, then load it.
///
/// Runs once on mount. The order matters: ask what the instance is before
/// asking who we are, because an unclaimed instance has no accounts and
/// `bootstrap` would fail in a way that reads like "signed out" when the real
/// answer is "nobody has claimed this yet".
pub fn start(app: App) {
    // Read the address bar BEFORE anything async, and clear it immediately.
    //
    // Immediately, because a claim token is a credential and should not sit in
    // history or in the referrer of the next request — and because an invite
    // left in the URL would be re-applied on every refresh, so someone who
    // joined and reloaded would be asked to join again.
    let landing = link::landing();
    link::clear_path();

    spawn_async(async move {
        match api::instance_status().await {
            Ok(status) => {
                app.instance_name.set(status.instance_name);
                if status.needs_claim {
                    // The link the server prints at startup, now clickable.
                    if let Landing::Claim(token) = &landing {
                        app.form_claim_token.set(token.clone());
                    }
                    app.phase.set(Phase::NeedsClaim);
                    return;
                }
            }
            Err(e) => {
                app.phase.set(Phase::Unreachable(message(e)));
                return;
            }
        }
        // Filled before the session check rather than after, because it has to
        // be there for BOTH outcomes: on the sign-in screen it pre-fills the
        // invite field, and for someone already signed in it is the code the
        // join dialog redeems.
        if let Landing::Invite(code) = &landing {
            app.form_invite.set(code.clone());
            app.join_tab.set(JoinTab::Join);
        }

        // A session may already exist — the cookie survives a reload, so a
        // refresh should not ask for a password again.
        load_shell(app, false).await;

        // Signed in already, and arrived on an invite link: open the dialog on
        // it rather than dropping them on their own spaces with no sign the
        // link did anything. Signed OUT, the code is already in the form and
        // the sign-in screen shows it.
        if matches!(landing, Landing::Invite(_)) && app.phase.get() == Phase::Ready {
            app.join_open.set(true);
        }
    });
}

/// Load everything the shell needs, and pick a landing channel.
///
/// `after_auth` distinguishes "we just signed in, so a failure is a real
/// problem" from "we are checking whether a cookie survived, so a failure just
/// means signed out".
async fn load_shell(app: App, after_auth: bool) {
    match api::bootstrap().await {
        Ok(data) => {
            let spaces = data.spaces.clone();
            app.me.set(Some(data.me));
            app.spaces.set(data.spaces);
            app.dms.set(data.dms);
            app.unread.set(data.unread);

            // Land somewhere: the first space's first text channel, or direct
            // messages if this account has no spaces at all.
            //
            // Computed from `spaces` — the local value — rather than by reading
            // back the signal we just wrote. `Signal::set` STAGES: reads return
            // the committed value until the world flushes, so
            // `app.spaces.with(…)` on the next line would see the empty vec
            // this call was made to replace, and every fresh sign-in would land
            // in Direct messages with a space sitting right there in the rail.
            let mut opened: Option<String> = None;
            if app.channel_id.get().is_empty() {
                let landing = spaces.first().map(|s| {
                    (
                        s.id.clone(),
                        s.channels
                            .iter()
                            .find(|c| c.kind == "text")
                            .map(|c| c.id.clone())
                            .unwrap_or_default(),
                    )
                });
                match landing {
                    Some((space, channel)) if !channel.is_empty() => {
                        app.space_id.set(space.clone());
                        app.channel_id.set(channel.clone());
                        app.last_chat.set(channel.clone());
                        opened = Some(channel);
                        load_members_of(app, &space);
                    }
                    _ => app.scope.set(Scope::Dms),
                }
            } else {
                opened = Some(app.channel_id.get());
                load_members_of(app, &app.space_id.get());
            }

            app.phase.set(Phase::Ready);
            app.notice.set(String::new());
            app.touch();
            if let Some(channel) = opened {
                load_channel_by_id(app, channel);
            }
        }
        Err(e) => {
            if after_auth {
                app.notice.set(message(e));
            }
            app.phase.set(Phase::SignedOut);
        }
    }
}

/// Re-fetch the sidebar without disturbing what is on screen.
pub fn refresh_shell(app: App) {
    spawn_async(async move {
        if let Ok(data) = api::bootstrap().await {
            app.spaces.set(data.spaces);
            app.dms.set(data.dms);
            app.unread.set(data.unread);
            app.touch();
        }
    });
}

// ===========================================================================
// Authentication
// ===========================================================================

pub fn claim(app: App) {
    let claim = api::Claim {
        token: app.form_claim_token.get().trim().to_string(),
        handle: app.form_handle.get().trim().to_lowercase(),
        display_name: app.form_display_name.get().trim().to_string(),
        password: app.form_password.get(),
        email: None,
    };
    if claim.token.is_empty() || claim.handle.is_empty() || claim.display_name.is_empty() {
        app.notice.set("Fill in the claim link, a handle and a name.".into());
        return;
    }
    submit(app, async move { api::claim_instance(claim).await });
}

pub fn sign_in(app: App) {
    let creds = api::Credentials {
        identifier: app.form_identifier.get().trim().to_string(),
        password: app.form_password.get(),
    };
    if creds.identifier.is_empty() || creds.password.is_empty() {
        app.notice.set("Enter your handle and password.".into());
        return;
    }
    submit(app, async move { api::login(creds).await });
}

pub fn register(app: App) {
    let reg = api::Registration {
        invite_code: app.form_invite.get().trim().to_string(),
        handle: app.form_handle.get().trim().to_lowercase(),
        display_name: app.form_display_name.get().trim().to_string(),
        password: app.form_password.get(),
        email: None,
    };
    if reg.invite_code.is_empty() || reg.handle.is_empty() || reg.display_name.is_empty() {
        app.notice.set("Fill in the invite code, a handle and a name.".into());
        return;
    }
    submit(app, async move { api::register(reg).await });
}

/// Run an auth request, then load the shell if it worked.
///
/// One place, so every auth path clears the password on success and reports
/// failure the same way. The password is dropped whatever happens: leaving it
/// in a signal keeps it in memory for the life of the tab.
fn submit(
    app: App,
    request: impl std::future::Future<Output = Result<api::LoginOk, ServerError>> + 'static,
) {
    if app.busy.get() {
        return;
    }
    app.busy.set(true);
    app.notice.set(String::new());
    spawn_async(async move {
        let outcome = request.await;
        app.form_password.set(String::new());
        app.busy.set(false);
        match outcome {
            Ok(ok) => {
                remember_token(&ok.token);
                app.me.set(Some(ok.user));
                load_shell(app, true).await;
            }
            Err(e) => app.notice.set(message(e)),
        }
    });
}

pub fn sign_out(app: App) {
    spawn_async(async move {
        let _ = api::logout().await;
        forget_token();
        // Clear loaded data as well as the session — leaving a signed-out
        // client holding the last account's spaces is how the next person to
        // sign in sees a flash of somebody else's sidebar.
        app.me.set(None);
        app.spaces.set(Vec::new());
        app.dms.set(Vec::new());
        app.members.set(Vec::new());
        app.messages.set(Vec::new());
        app.unread.set(Vec::new());
        app.channel_id.set(String::new());
        app.space_id.set(String::new());
        app.settings_open.set(false);
        app.auth_tab.set(AuthTab::SignIn);
        app.phase.set(Phase::SignedOut);
        app.touch();
    });
}

// ===========================================================================
// Channels and messages
// ===========================================================================

/// Load the selected channel's newest page, and mark it read.
pub fn load_channel(app: App) {
    load_channel_by_id(app, app.channel_id.get());
}

/// The same, for a channel id the caller already holds.
///
/// Every navigation uses this rather than [`load_channel`], because a caller
/// that has just `set` the selection cannot read it back — `set` stages, so the
/// read returns the channel we navigated *away* from.
pub fn load_channel_by_id(app: App, channel: String) {
    if channel.is_empty() {
        app.messages.set(Vec::new());
        return;
    }
    spawn_async(async move {
        match api::channel_page(channel.clone(), None).await {
            Ok(page) => {
                // Guard against a slow response for a channel we have since
                // navigated away from — otherwise clicking quickly through a
                // list leaves the last response to win.
                if app.channel_id.get() != channel {
                    return;
                }
                let newest = page.messages.last().map(|m| m.id.clone());
                app.messages.set(page.messages);
                app.touch();
                if let Some(upto) = newest {
                    let _ = api::mark_read(channel.clone(), upto).await;
                    app.unread.update(|u| {
                        u.iter().filter(|c| **c != channel).cloned().collect()
                    });
                    app.touch();
                }
            }
            Err(e) => app.notice.set(message(e)),
        }
    });
}

/// Load the roster for the current space.
pub fn load_members(app: App) {
    load_members_of(app, &app.space_id.get());
}

/// The same, for a space id the caller already holds — see
/// [`load_channel_by_id`] for why every navigation passes one.
pub fn load_members_of(app: App, space_id: &str) {
    let space = space_id.to_string();
    if space.is_empty() {
        app.members.set(Vec::new());
        return;
    }
    spawn_async(async move {
        if let Ok(members) = api::space_members(space.clone()).await {
            if app.space_id.get() == space {
                app.members.set(members);
                app.touch();
            }
        }
    });
}

/// Select a channel and load it.
///
/// Clears the draft and any open thread: a half-typed line belongs to the
/// channel you left, and a thread panel showing a conversation from a
/// different channel is worse than no panel.
pub fn go_channel(app: App, channel_id: &str) {
    let id = channel_id.to_string();
    let is_dm = app.dms.with(|d| d.iter().any(|d| d.channel_id == id));
    app.channel_id.set(id.clone());
    app.scope.set(if is_dm { Scope::Dms } else { Scope::Space });
    if is_dm {
        app.last_dm.set(id.clone());
    }
    app.last_chat.set(id.clone());
    app.settings_open.set(false);
    // Reading something is a request to see it. The call view occupies the same
    // column, so it steps aside — this does not leave the room, which is the
    // whole point of keeping connection and view separate.
    app.stage.set(String::new());
    // On a phone the room list IS the screen, so picking a channel has to get
    // out of the way. Pinned, there is nothing to close.
    if !idea_ui_nav::sidebar_pinned(runtime_core::Breakpoint::Lg) {
        app.nav_open.set(false);
    }
    app.thread_root.set(String::new());
    app.search_open.set(false);
    app.draft.set(String::new());
    // Tell the live connection what we are reading. It answers with the page,
    // so the fetch below is only what happens while the socket is down.
    #[cfg(target_arch = "wasm32")]
    let live = crate::livewire::watch(&id);
    #[cfg(not(target_arch = "wasm32"))]
    let live = false;

    if !live {
        // By id, not by reading the signal back — see `load_channel_by_id`.
        load_channel_by_id(app, id);
    }
}

pub fn go_space(app: App, space_id: &str) {
    // Look the space up BEFORE writing the selection: reading `app.space()`
    // afterwards would resolve against the id we are replacing.
    let first = app.spaces.with(|spaces| {
        spaces
            .iter()
            .find(|s| s.id == space_id)
            .and_then(|s| s.channels.iter().find(|c| c.kind == "text").map(|c| c.id.clone()))
    });
    app.space_id.set(space_id.to_string());
    app.scope.set(Scope::Space);
    app.settings_open.set(false);
    app.thread_root.set(String::new());
    load_members_of(app, space_id);
    if let Some(channel) = first {
        go_channel(app, &channel);
    }
}

pub fn go_dms(app: App) {
    app.scope.set(Scope::Dms);
    app.settings_open.set(false);
    let last = app.last_dm.get();
    let target = if last.is_empty() {
        app.dms.with(|d| d.first().map(|d| d.channel_id.clone()))
    } else {
        Some(last)
    };
    if let Some(channel) = target {
        go_channel(app, &channel);
    }
}

/// Post the draft.
///
/// The id is minted here, before the request, which is what makes a retry
/// idempotent — and it means the optimistic row already carries the id the
/// server will store, so nothing has to be reconciled when the reply arrives.
pub fn send(app: App) {
    let body = app.draft.get().trim().to_string();
    let channel = app.channel_id.get();
    if body.is_empty() || channel.is_empty() {
        return;
    }
    let id = new_id();
    app.draft.set(String::new());

    // Show it immediately. A message that appears only after a round trip
    // makes a fast connection feel slow and a slow one feel broken.
    let optimistic = api::MessageDto {
        id: id.clone(),
        channel_id: channel.clone(),
        author_name: app.my_name(),
        author_id: Some(app.my_id()),
        thread_root_id: None,
        body: Some(body.clone()),
        reply_count: 0,
        created_at: now_rfc3339(),
        edited_at: None,
    };
    app.messages.update(|prev| {
        let mut next = prev.clone();
        next.push(optimistic.clone());
        next
    });
    app.touch();

    // Over the live connection when there is one: the server answers with a
    // `MessagePosted` event that everyone in the channel gets, this client
    // included, and the optimistic row is corrected when it arrives (same id).
    // The HTTP path stays for the moment the socket is reconnecting — a
    // message typed then should still be sent, not silently dropped.
    #[cfg(target_arch = "wasm32")]
    if crate::livewire::connected() {
        crate::livewire::send(crate::live::Action::Post {
            id,
            channel_id: channel,
            body,
            thread_root_id: None,
        });
        return;
    }

    spawn_async(async move {
        match api::send_message(id.clone(), channel, body, None).await {
            Ok(stored) => {
                // Replace the optimistic row in place. Same id, so this is a
                // correction of what we guessed (the timestamp), not an
                // insertion.
                app.messages.update(|prev| {
                    prev.iter()
                        .map(|m| if m.id == stored.id { stored.clone() } else { m.clone() })
                        .collect()
                });
                app.touch();
            }
            Err(e) => {
                // Take the row back out rather than leaving a message on
                // screen that no one else can see.
                app.messages.update(|prev| prev.iter().filter(|m| m.id != id).cloned().collect());
                app.notice.set(message(e));
                app.touch();
            }
        }
    });
}

/// Open a thread and load it.
pub fn open_thread(app: App, root_id: &str) {
    let root = root_id.to_string();
    app.thread_root.set(root.clone());
    app.thread_messages.set(Vec::new());
    spawn_async(async move {
        if let Ok(messages) = api::thread(root.clone()).await {
            if app.thread_root.get() == root {
                app.thread_messages.set(messages);
                app.touch();
            }
        }
    });
}

pub fn send_reply(app: App) {
    let body = app.thread_draft.get().trim().to_string();
    let root = app.thread_root.get();
    let channel = app.channel_id.get();
    if body.is_empty() || root.is_empty() {
        return;
    }
    app.thread_draft.set(String::new());
    spawn_async(async move {
        if let Ok(stored) = api::send_message(new_id(), channel, body, Some(root.clone())).await {
            app.thread_messages.update(|prev| {
                let mut next = prev.clone();
                next.push(stored.clone());
                next
            });
            // The root's reply count changed, so the badge in the channel
            // behind the panel is now wrong.
            app.touch();
            if let Ok(page) = api::channel_page(app.channel_id.get(), None).await {
                app.messages.set(page.messages);
                app.touch();
            }
        }
    });
}

pub fn delete_message(app: App, id: &str) {
    let id = id.to_string();
    let channel = app.channel_id.get();
    spawn_async(async move {
        if api::delete_message(id, channel.clone()).await.is_ok() {
            if let Ok(page) = api::channel_page(channel, None).await {
                app.messages.set(page.messages);
                app.touch();
            }
        }
    });
}

/// Take someone out of a voice room, as a moderator. The server revokes
/// their grant at the relay — their call ends at once — and takes them out
/// of the room's presence, which everyone sees through the live socket.
pub fn remove_from_call(app: App, channel_id: &str, user_id: &str) {
    let (channel, user) = (channel_id.to_string(), user_id.to_string());
    spawn_async(async move {
        if let Err(e) = api::voice_remove(channel, user).await {
            app.notice.set(message(e));
        }
    });
}

// ===========================================================================
// Spaces and invites
// ===========================================================================

pub fn create_space(app: App) {
    let name = app.join_value.get().trim().to_string();
    if name.is_empty() {
        app.notice.set("Give the space a name.".into());
        return;
    }
    let slug = slugify(&name);
    spawn_async(async move {
        match api::create_space(slug, name).await {
            Ok(space) => {
                app.join_open.set(false);
                app.join_value.set(String::new());
                open_summary(app, space);
            }
            Err(e) => app.notice.set(message(e)),
        }
    });
}

pub fn join_space(app: App) {
    let code = invite_code_of(&app.join_value.get());
    if code.is_empty() {
        app.notice.set("Paste an invite link or code.".into());
        return;
    }
    spawn_async(async move {
        match api::join_space(code).await {
            Ok(space) => {
                app.join_open.set(false);
                app.join_value.set(String::new());
                open_summary(app, space);
            }
            Err(e) => app.notice.set(message(e)),
        }
    });
}

/// Put a space we just received into the sidebar and open it.
///
/// From the response, not from a re-read: `refresh_shell` has not committed
/// yet, so looking the new space up in `app.spaces` would not find it.
fn open_summary(app: App, space: crate::api::SpaceSummary) {
    let channel = space.channels.iter().find(|c| c.kind == "text").map(|c| c.id.clone());
    let space_id = space.id.clone();
    app.spaces.update(|prev| {
        let mut next: Vec<_> = prev.iter().filter(|s| s.id != space.id).cloned().collect();
        next.push(space.clone());
        next
    });
    app.space_id.set(space_id.clone());
    app.scope.set(Scope::Space);
    app.touch();
    load_members_of(app, &space_id);
    if let Some(channel) = channel {
        app.channel_id.set(channel.clone());
        app.last_chat.set(channel.clone());
        load_channel_by_id(app, channel);
    }
    refresh_shell(app);
}

/// Mint an invite for the current space and show it to be copied.
pub fn mint_invite(app: App) {
    let space = app.space_id.get();
    if space.is_empty() {
        return;
    }
    app.minted_invite.set(String::new());
    spawn_async(async move {
        match api::create_invite(space, 1).await {
            Ok(code) => app.minted_invite.set(code),
            Err(e) => app.notice.set(message(e)),
        }
    });
}

/// Open (or find) the conversation with someone, and go to it.
pub fn open_dm(app: App, user_id: &str) {
    let user = user_id.to_string();
    app.profile.set(String::new());
    app.close_menu();
    spawn_async(async move {
        if let Ok(channel) = api::open_dm(user).await {
            // Insert the row locally as well as refreshing, so it exists by
            // the time we navigate into it.
            let id = channel.id.clone();
            app.dms.update(|prev| {
                let mut next: Vec<_> =
                    prev.iter().filter(|d| d.channel_id != id).cloned().collect();
                next.insert(
                    0,
                    crate::api::DmDto {
                        channel_id: channel.id.clone(),
                        other_id: None,
                        other_name: channel.name.clone(),
                        preview: "Say hello".to_string(),
                    },
                );
                next
            });
            app.scope.set(Scope::Dms);
            app.channel_id.set(channel.id.clone());
            app.last_dm.set(channel.id.clone());
            app.touch();
            load_channel_by_id(app, channel.id);
            refresh_shell(app);
        }
    });
}

// ===========================================================================
// Small helpers
// ===========================================================================

/// A UUIDv7 for a message the client is about to send.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Current time as RFC 3339, for an optimistic row's timestamp.
///
/// The server's value replaces this the moment the real message comes back, so
/// a few milliseconds of drift never survives.
fn now_rfc3339() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let ms = js_sys::Date::now();
        let secs = (ms / 1000.0) as i64;
        let h = (secs / 3600) % 24;
        let m = (secs / 60) % 60;
        let s = secs % 60;
        format!("1970-01-01T{h:02}:{m:02}:{s:02}Z")
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        "1970-01-01T00:00:00Z".to_string()
    }
}

/// Turn a display name into a URL-safe slug the schema will accept.
fn slugify(name: &str) -> String {
    let mut out: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let out = out.trim_matches('-').to_string();
    // The schema requires 2–48 of `[a-z0-9-]`. A name of pure punctuation
    // would otherwise produce an empty slug and a confusing server error.
    if out.len() < 2 {
        format!("space-{}", &new_id()[..8])
    } else {
        out.chars().take(48).collect()
    }
}

/// Accept either a bare code or a full invite URL.
///
/// People paste whichever they were given, and refusing one of them is a
/// self-inflicted support burden.
pub fn invite_code_of(input: &str) -> String {
    input.trim().rsplit('/').next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_link_or_a_bare_code_both_work() {
        assert_eq!(invite_code_of("quiet-porch-42"), "quiet-porch-42");
        assert_eq!(invite_code_of("https://hearth.chat/j/quiet-porch-42"), "quiet-porch-42");
        assert_eq!(invite_code_of("  quiet-porch-42  "), "quiet-porch-42");
    }

    #[test]
    fn a_name_becomes_a_slug_the_schema_accepts() {
        assert_eq!(slugify("The Cabin"), "the-cabin");
        assert_eq!(slugify("Mercier  Family!"), "mercier-family");
        // The schema requires at least two characters of [a-z0-9-].
        assert!(slugify("!!!").len() >= 2);
        assert!(slugify("x").len() >= 2);
    }

    #[test]
    fn client_minted_ids_are_v7_and_ordered() {
        let a = new_id();
        let b = new_id();
        assert_eq!(uuid::Uuid::parse_str(&a).unwrap().get_version_num(), 7);
        assert!(a < b, "ids must sort in mint order or paging breaks");
    }
}
