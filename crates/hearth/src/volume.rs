//! How loud everyone else is, on this machine.
//!
//! Two controls, and the difference between them is the whole design:
//!
//! - **The master** is one number for the whole room. It is what answers
//!   "everyone is too loud", and it is a *device* preference like every other
//!   one in [`crate::prefs`] — how loud a room should be depends on the
//!   headphones you have on, not on who you are.
//! - **Per person** is one number each, on top of the master. It is what
//!   answers "everyone is fine except this one", which the master cannot: the
//!   master moves the person you can barely hear at the same time as the one
//!   who is shouting.
//!
//! Neither travels. Nobody is told they have been turned down, nothing is sent
//! to the server, and the person on the other end sounds to everyone else
//! exactly as they did before. That is why this can be a slider with no
//! etiquette attached to it, unlike the speaker-side stages in `voice_dsp`
//! where one person's choice is everyone's result.
//!
//! # Why the master defaults below 100%
//!
//! Two reasons, and the second is the one that matters.
//!
//! The obvious one: the speaker-side levelling in `voice_dsp` deliberately
//! lands everybody at the same broadcast-ish level, so a room arrives *hot*
//! and consistent rather than quiet and uneven. Played at unity into a system
//! volume that was set for music, that is loud.
//!
//! The real one: gains multiply, and the ceiling is the ceiling. Turning one
//! person **up** is only possible if there is room above where they already
//! are — so the master is not merely a volume, it is the headroom that makes
//! the per-person control able to go both ways. At a master of 100% the only
//! available direction is down.

use crate::prefs;

/// Master position, `0.0..=1.0`, where `1.0` is "as it arrived".
///
/// 70% of position is about -6 dB of gain once [`voice_dsp::taper`] has been
/// applied — quieter than sent, with enough room left over that a person can
/// be pushed back past unity when they need to be.
pub const MASTER_DEFAULT: f32 = 0.7;

/// Per-person position. `1.0` is "no opinion", which is what almost everybody
/// will always be.
pub const PERSON_DEFAULT: f32 = 1.0;

/// The loudest a single person may be set to, as a position. Tapered, 200% is
/// four times the amplitude — the far end of what is useful before a
/// suppressor's residue comes up with the voice.
pub const PERSON_MAX: f32 = 2.0;

/// Where per-person volumes live in the preference store. Public because the
/// applier enumerates the whole prefix — see [`everyone`].
pub const PERSON_PREFIX: &str = "voice.volume.";

const MASTER_KEY: &str = "voice.volume";

/// Who is silenced, as one comma-joined list. Account ids are UUIDs, so a
/// comma cannot appear inside one.
const MUTED_KEY: &str = "voice.muted";

/// The stored master position.
pub fn master() -> f32 {
    prefs::number(MASTER_KEY, MASTER_DEFAULT).clamp(0.0, 1.0)
}

/// Remember the master position.
pub fn set_master(position: f32) {
    prefs::set_number(MASTER_KEY, position.clamp(0.0, 1.0));
}

/// The stored position for one person.
pub fn person(user_id: &str) -> f32 {
    prefs::number(&key(user_id), PERSON_DEFAULT).clamp(0.0, PERSON_MAX)
}

/// Remember one person's position — or forget it, if it is back at the
/// default. A row per person the user once nudged and put back would be a
/// store that only ever grows.
pub fn set_person(user_id: &str, position: f32) {
    let position = position.clamp(0.0, PERSON_MAX);
    if (position - PERSON_DEFAULT).abs() < 0.001 {
        prefs::clear(&key(user_id));
    } else {
        prefs::set_number(&key(user_id), position);
    }
}

/// Everyone this device has an opinion about, as `(user id, position)`.
///
/// Read at join, because a volume set for somebody who is not talking has to
/// be in force the instant they start.
pub fn everyone() -> Vec<(String, f32)> {
    prefs::with_prefix(PERSON_PREFIX)
        .into_iter()
        .filter_map(|(user_id, raw)| {
            let position: f32 = raw.parse().ok()?;
            position.is_finite().then(|| (user_id, position.clamp(0.0, PERSON_MAX)))
        })
        .collect()
}

fn key(user_id: &str) -> String {
    format!("{PERSON_PREFIX}{user_id}")
}

/// Everyone silenced on this device.
///
/// Stored, like the volumes, and for the same reason: you mute somebody
/// because of something that is still true after a refresh — a bad microphone,
/// a noisy room, a person you would rather read than hear. Forgetting it on
/// reload would be a setting that quietly undoes itself, and the un-muting
/// happens without a sound, so there is nothing to notice.
pub fn muted() -> Vec<String> {
    prefs::get(MUTED_KEY)
        .unwrap_or_default()
        .split(',')
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect()
}

/// Remember who is silenced.
pub fn set_muted(ids: &[String]) {
    if ids.is_empty() {
        prefs::clear(MUTED_KEY);
    } else {
        prefs::set(MUTED_KEY, &ids.join(","));
    }
}

/// What the audio path should actually apply for one person.
///
/// Mute and volume are separate STATE and meet only here, in the one number
/// the audio path gets. That separation is the point of having both: mute is a
/// lease over the level, not a value written into it, so unmuting returns
/// somebody to exactly where they were rather than to a default. Writing zero
/// into the slider instead would work once and lose the level the first time
/// anyone used it.
pub fn person_gain(position: f32, muted: bool) -> f32 {
    if muted {
        0.0
    } else {
        voice_dsp::taper(position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mute wins over any position, including a boosted one.
    #[test]
    fn mute_is_zero_whatever_the_slider_says() {
        assert_eq!(person_gain(2.0, true), 0.0);
        assert_eq!(person_gain(1.0, true), 0.0);
        assert_eq!(person_gain(0.0, false), 0.0);
    }

    /// The default position has to be exactly unity, or every unmuted person
    /// is quietly recoloured by a control nobody touched.
    #[test]
    fn the_default_person_is_untouched() {
        assert_eq!(person_gain(PERSON_DEFAULT, false), 1.0);
    }

    /// Mute is a lease over the level, not a value written into it: whatever
    /// the slider said before the mute is what comes back after it. This is
    /// the whole reason the two are separate state.
    #[test]
    fn unmuting_returns_the_level_that_was_there() {
        for position in [0.4_f32, 1.0, 1.7] {
            assert_eq!(person_gain(position, true), 0.0);
            assert_eq!(person_gain(position, false), voice_dsp::taper(position));
        }
    }

    /// The master is headroom: boosting somebody past unity has to be
    /// reachable at the shipped default, or the per-person control only ever
    /// goes one way.
    #[test]
    fn the_default_master_leaves_room_to_boost() {
        let master = voice_dsp::taper(MASTER_DEFAULT);
        assert!(master < 1.0, "master default is not below unity");
        assert!(
            master * person_gain(PERSON_MAX, false) > 1.0,
            "nobody can be pushed above unity at the default master",
        );
    }
}
