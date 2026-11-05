//! The connection indicator in the control bar: a coloured dot and a word for
//! how the call's link is doing — with a lock when the call is end-to-end
//! encrypted — and on hover (or a tap, on touch) a card of the numbers behind
//! it: round trip, estimated latency, bitrates, loss, encryption.
//!
//! The page measures; this grades and draws. Platform-free, so the grading is
//! unit-tested natively.

use icons_lucide::LOCK;
use idea_ui::{tone, typography_kind, Card, Icon, IdeaThemeRef, Typography};
use runtime_core::primitives::overlay::BackdropMode;
use runtime_core::primitives::portal::{AnchorTarget, ElementAlign, ElementSide};
use runtime_core::{
    pressable, rx, signal, stylesheet, switch, ui, AlignItems, Cursor, Element, FlexDirection, JustifyContent, Ref,
    Signal, StyleApplication, ViewHandle,
};

/// How the link is doing, as the dot's colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    Good,
    Fair,
    Poor,
    /// Not connected, or not yet.
    #[default]
    Off,
}

impl Quality {
    fn variant(self) -> &'static str {
        match self {
            Quality::Good => "good",
            Quality::Fair => "fair",
            Quality::Poor => "poor",
            Quality::Off => "off",
        }
    }
}

/// Grade a link from its round trip to the relay (ms) and the share of audio
/// frames lost in the last window (0–1). Round trip counts twice in what
/// people hear — there and back again through someone else's link — so it is
/// held to a stricter line than loss, which the jitter buffer partly hides.
pub fn grade(rtt_ms: Option<f64>, loss: f64) -> Quality {
    let rtt = rtt_ms.unwrap_or(0.0);
    if rtt <= 150.0 && loss < 0.02 {
        Quality::Good
    } else if rtt <= 300.0 && loss < 0.08 {
        Quality::Fair
    } else {
        Quality::Poor
    }
}

/// What the indicator shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Health {
    pub label: String,
    pub quality: Quality,
    /// The card's rows: what, and how much.
    pub rows: Vec<(String, String)>,
    /// Whether the call is end-to-end encrypted, which earns the lock.
    pub encrypted: bool,
}

/// Kilobits a second, from bytes over `secs`.
pub fn kbps(bytes: u64, secs: f64) -> f64 {
    if secs <= 0.0 { 0.0 } else { bytes as f64 * 8.0 / 1000.0 / secs }
}

/// A rate for the card: whole kb/s under a megabit, one decimal of Mb/s over.
pub fn rate(kbps: f64) -> String {
    if kbps >= 1000.0 { format!("{:.1} Mb/s", kbps / 1000.0) } else { format!("{kbps:.0} kb/s") }
}

stylesheet! {
    /// The dot and its word; highlighted under the pointer, like the bar's
    /// buttons.
    Indicator<IdeaThemeRef> {
        base(t) {
            flex_direction: FlexDirection::Row,
            align_items: AlignItems::Center,
            gap: 8.0,
            padding_left: 10.0,
            padding_right: 12.0,
            padding_top: 6.0,
            padding_bottom: 6.0,
            border_radius: 999.0,
            cursor: Cursor::Default,
        }
        state hovered(t) {
            background: t.color.surface_alt(),
        }
        transitions { background: 150ms EaseOut }
    }
}

stylesheet! {
    Dot<IdeaThemeRef> {
        base(t) {
            width: 9.0,
            height: 9.0,
            border_radius: 999.0,
            background: t.color.text_muted(),
        }
        variant quality {
            good(t) { background: t.intent.success.fg() }
            fair(t) { background: t.intent.warning.fg() }
            poor(t) { background: t.intent.danger.fg() }
            #[default]
            off(t) { background: t.color.text_muted() }
        }
        transitions { background: 300ms EaseOut }
    }
}

stylesheet! {
    /// One row of the card: what on the left, how much on the right.
    Row<()> {
        base(_t) {
            flex_direction: FlexDirection::Row,
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            gap: 24.0,
        }
    }
}

stylesheet! {
    Rows<()> {
        base(_t) {
            flex_direction: FlexDirection::Column,
            gap: 6.0,
            min_width: 260.0,
        }
    }
}

/// The indicator, live on `health`.
pub fn indicator(health: Signal<Health>) -> Element {
    let open = signal(false);
    let anchor: Ref<ViewHandle> = Ref::new();
    let dot: Element = runtime_core::view(Vec::new())
        .with_style(move || StyleApplication::new(Dot::sheet()).with("quality", health.get().quality.variant()))
        .into();
    let label = ui! { Typography(content = rx!(health.get().label), kind = typography_kind::Caption) };
    let lock = switch(
        move || health.get().encrypted,
        |&on: &bool| {
            if !on {
                return runtime_core::fragment(Vec::new());
            }
            let icon = ui! { Icon(data = LOCK, size = 13.0, tone = Some(tone::Success.into())) };
            runtime_core::view(vec![icon])
                .a11y_role(runtime_core::accessibility::Role::Image)
                .a11y_label("End-to-end encrypted")
                .into()
        },
    );
    // A tap toggles it, for touch, where there is no hover.
    let face: Element = pressable(vec![dot, label, lock], move || open.set(!open.get()))
        .with_style(|| StyleApplication::new(Indicator::sheet()))
        .into();
    let trigger: Element = runtime_core::view(vec![face])
        .bind(anchor.clone())
        .on_hover(move |entering| open.set(entering))
        .a11y_label("Connection details")
        .into();
    // No backdrop: a hover card must not lay anything over the trigger, or
    // the pointer leaves it the moment the card opens.
    let card = switch(
        move || open.get(),
        move |&shown: &bool| {
            if !shown {
                return runtime_core::fragment(Vec::new());
            }
            let rows = switch(
                move || health.get(),
                |h: &Health| {
                    let rows: Vec<Element> = h
                        .rows
                        .iter()
                        .map(|(what, value)| {
                            ui! {
                                view(style = || StyleApplication::new(Row::sheet())) {
                                    Typography(content = what.clone(), kind = typography_kind::Caption, muted = true)
                                    Typography(content = value.clone(), kind = typography_kind::Caption)
                                }
                            }
                        })
                        .collect();
                    ui! { view(style = || StyleApplication::new(Rows::sheet())) { rows } }
                },
            );
            let body: Element = ui! { Card() { rows } };
            runtime_core::anchored_overlay(AnchorTarget::from(anchor.clone()), vec![body])
                .side(ElementSide::Above)
                .align(ElementAlign::Start)
                .offset(8.0)
                .backdrop(BackdropMode::None)
                .trap_focus(false)
                .into()
        },
    );
    runtime_core::fragment(vec![trigger, card])
}

/// What a throttling relay is holding back from us, in words; `None` for
/// nothing.
pub fn describe_held_back(level: u8) -> Option<&'static str> {
    match level {
        0 => None,
        1 => Some("video at half frame rate"),
        2 => Some("video at a quarter frame rate"),
        3 => Some("cameras paused, screens at a quarter"),
        _ => Some("video paused — voice only"),
    }
}

/// What we are sending at, and why, as two lines for the card and the
/// settings: `audio 40 kb/s (20 kHz) · video 600 kb/s`, and what bitrate
/// control last decided.
pub fn describe_rate(audio_bps: u32, fullband: bool, video_bps: u32, why: Option<call_rate::Why>) -> (String, String) {
    let band = if fullband { "20 kHz" } else { "8 kHz" };
    let mut sending = format!("audio {} ({band})", rate(audio_bps as f64 / 1000.0));
    if video_bps > 0 {
        sending.push_str(&format!(" · video {}", rate(video_bps as f64 / 1000.0)));
    }
    use call_rate::Why;
    let why = match why {
        None => "Auto — measuring".to_string(),
        Some(Why::Ceiling) => "Auto — clear link, full rate".to_string(),
        Some(Why::Growing) => "Auto — clear link, rising".to_string(),
        Some(Why::Holding) => "Auto — some loss, holding".to_string(),
        Some(Why::Loss(l)) => format!("Auto — {:.0}% lost, backing off", l * 100.0),
        Some(Why::Queue) => "Auto — send queue full, backing off".to_string(),
        Some(Why::Delay) => "Auto — delay rising, backing off".to_string(),
        Some(Why::Manual) => "Manual".to_string(),
    };
    (sending, why)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_held_back_is_described() {
        assert_eq!(describe_held_back(0), None);
        assert_eq!(describe_held_back(2), Some("video at a quarter frame rate"));
        assert_eq!(describe_held_back(9), Some("video paused — voice only"));
    }

    #[test]
    fn rates_are_described() {
        assert_eq!(
            describe_rate(40_000, true, 600_000, Some(call_rate::Why::Ceiling)),
            ("audio 40 kb/s (20 kHz) · video 600 kb/s".to_string(), "Auto — clear link, full rate".to_string())
        );
        assert_eq!(
            describe_rate(24_000, false, 0, Some(call_rate::Why::Loss(0.123))),
            ("audio 24 kb/s (8 kHz)".to_string(), "Auto — 12% lost, backing off".to_string())
        );
    }

    #[test]
    fn a_quick_clean_link_is_good() {
        assert_eq!(grade(Some(40.0), 0.0), Quality::Good);
        assert_eq!(grade(None, 0.0), Quality::Good, "no round trip yet is not a bad one");
    }

    #[test]
    fn delay_or_loss_each_bring_it_down() {
        assert_eq!(grade(Some(200.0), 0.0), Quality::Fair);
        assert_eq!(grade(Some(40.0), 0.05), Quality::Fair);
        assert_eq!(grade(Some(400.0), 0.0), Quality::Poor);
        assert_eq!(grade(Some(40.0), 0.2), Quality::Poor);
    }

    #[test]
    fn rates_read_naturally() {
        assert_eq!(kbps(3000, 1.0), 24.0);
        assert_eq!(kbps(3000, 0.0), 0.0);
        assert_eq!(rate(24.4), "24 kb/s");
        assert_eq!(rate(1520.0), "1.5 Mb/s");
    }
}
