//! What a URL is asking the app to do.
//!
//! The app is a single page, so the path never reaches a router on the server —
//! it arrives here, in the browser, as a string to interpret. Two paths mean
//! something:
//!
//! - `/j/<code>` — an invitation to a space.
//! - `/claim/<token>` — the one-time link that claims a fresh instance.
//!
//! Both used to be *pasted*. The server printed a claim URL at startup that
//! could only be copied out of and typed into a form, and an invite was a bare
//! code with no link at all. Neither was a link anyone could click, and the
//! claim one 404'd if you tried.
//!
//! The parsing is pure and lives here so it can be tested on a host, away from
//! the browser it runs in.

/// What the address bar was pointing at when the app loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landing {
    /// An invitation to a space.
    Invite(String),
    /// The one-time instance claim.
    Claim(String),
    /// An ordinary visit.
    Nothing,
}

/// Read a path.
///
/// Tolerant of the shapes a link acquires in the wild: a trailing slash from an
/// autolinker, a `?utm_...` a chat client appended, a `#` a browser kept.
/// Intolerant of an empty code, which is a malformed link rather than a request
/// to join nothing.
pub fn landing_of(path: &str) -> Landing {
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    match (parts.next(), parts.next()) {
        (Some("j"), Some(code)) => Landing::Invite(code.to_string()),
        (Some("claim"), Some(token)) => Landing::Claim(token.to_string()),
        _ => Landing::Nothing,
    }
}

/// The URL to hand someone for an invite code.
///
/// A whole link, not a code. "Paste this into the second field of the sign-in
/// screen" is an instruction; a link is not.
pub fn invite_url(origin: &str, code: &str) -> String {
    format!("{}/j/{}", origin.trim_end_matches('/'), code)
}

/// Where this page is served from, for building a shareable link.
///
/// Falls back to a relative path rather than to a hardcoded host: a relative
/// link is useless to share but harmless, where the wrong absolute host is a
/// link that silently sends people somewhere else.
pub fn origin() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    String::new()
}

/// What the address bar says right now.
pub fn landing() -> Landing {
    #[cfg(target_arch = "wasm32")]
    {
        let path = web_sys::window()
            .and_then(|w| w.location().pathname().ok())
            .unwrap_or_default();
        landing_of(&path)
    }
    #[cfg(not(target_arch = "wasm32"))]
    Landing::Nothing
}

/// Put the address bar back to `/` without reloading.
///
/// Two reasons, and the second is the one that matters. A claim token is a
/// credential and should not sit in the address bar, in history, or in the
/// referrer of the next request. And an invite that stayed in the URL would be
/// re-applied on every refresh, so a person who joined and reloaded would be
/// asked to join again.
pub fn clear_path() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(history) = web_sys::window().and_then(|w| w.history().ok()) {
            let _ = history.replace_state_with_url(
                &wasm_bindgen::JsValue::NULL,
                "",
                Some("/"),
            );
        }
    }
}

/// Put `text` on the clipboard, best effort.
///
/// Best effort because the API is permission-gated and absent on insecure
/// origins, and because there is nothing useful to do about a refusal: the
/// link is on screen either way. The caller says it worked only if this
/// returns true.
pub fn copy(text: &str) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        let Some(clipboard) = web_sys::window().map(|w| w.navigator().clipboard()) else {
            return false;
        };
        // Returns a promise. Nothing awaits it: a failure surfaces as the
        // clipboard simply not having changed, which the person discovers by
        // pasting, and no message we could show would help.
        let _ = clipboard.write_text(text);
        true
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_link_carries_its_code() {
        assert_eq!(landing_of("/j/quiet-porch-42"), Landing::Invite("quiet-porch-42".into()));
    }

    #[test]
    fn a_claim_link_carries_its_token() {
        assert_eq!(landing_of("/claim/abc123"), Landing::Claim("abc123".into()));
    }

    #[test]
    fn an_ordinary_visit_asks_for_nothing() {
        assert_eq!(landing_of("/"), Landing::Nothing);
        assert_eq!(landing_of(""), Landing::Nothing);
    }

    #[test]
    fn a_link_survives_what_the_world_does_to_links() {
        // A trailing slash from an autolinker, tracking a chat client appended,
        // a fragment the browser kept. All of these arrive in real messages and
        // none of them changes which space you were invited to.
        assert_eq!(landing_of("/j/porch/"), Landing::Invite("porch".into()));
        assert_eq!(landing_of("/j/porch?utm_source=slack"), Landing::Invite("porch".into()));
        assert_eq!(landing_of("/j/porch#"), Landing::Invite("porch".into()));
    }

    #[test]
    fn a_link_with_no_code_is_not_an_invitation() {
        // Rather than an invitation to join nothing, which would show a form
        // pre-filled with an empty code and fail on submit for no stated reason.
        assert_eq!(landing_of("/j"), Landing::Nothing);
        assert_eq!(landing_of("/j/"), Landing::Nothing);
        assert_eq!(landing_of("/claim/"), Landing::Nothing);
    }

    #[test]
    fn any_other_path_is_the_app_being_opened_normally() {
        assert_eq!(landing_of("/spaces/porch"), Landing::Nothing);
        assert_eq!(landing_of("/jj/porch"), Landing::Nothing, "a prefix is not a match");
    }

    #[test]
    fn a_share_url_is_a_whole_link() {
        assert_eq!(invite_url("https://chat.idealyst.io", "porch"), "https://chat.idealyst.io/j/porch");
        assert_eq!(invite_url("https://chat.idealyst.io/", "porch"), "https://chat.idealyst.io/j/porch");
    }

    #[test]
    fn a_share_url_round_trips_through_the_parser() {
        // The two halves have to agree, and they are written in different
        // places by different people at different times.
        let url = invite_url("https://chat.idealyst.io", "quiet-porch-42");
        let path = url.trim_start_matches("https://chat.idealyst.io");
        assert_eq!(landing_of(path), Landing::Invite("quiet-porch-42".into()));
    }
}
