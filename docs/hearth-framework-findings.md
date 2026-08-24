# Framework findings from building Hearth

Everything here came out of implementing `Hearth.dc.html` on idealyst 1.3.16 —
a ~3,000-line app crate, no framework changes, `idea-ui` for every control.
Each item is something that cost debugging time, produced a wrong result, or
forced a documented departure from the design. They're ordered by how much they
cost, not by severity to the framework.

Nothing here blocked the app. All of it is now handled in `crates/hearth`, and
the reason is commented at the site.

**The first two are corrections to my own misuse of the framework, not defects
in it.** I originally filed #1 as a framework bug — that a `#[component]` body
isn't a reactive scope. That claim was wrong, and the experiment that disproves
it is in the section. It's kept here because the *misdiagnosis* is the useful
artifact: it's the mistake a newcomer from a re-render framework will make, and
the symptom points somewhere else entirely.

---

## 1. Reading signals above the sink instead of inside it (my bug, not the framework's)

**Cost: the largest block of debugging in the build — and the diagnosis I got
wrong.**

The symptom: the app rendered correctly on first paint and then went inert.
Handlers fired, signals committed, nothing moved on screen. I concluded that a
`#[component]` body was not a reactive scope and called it a framework defect.

That was wrong. idealyst is fine-grained reactive, and a component body running
once is the *correct* design — the same contract Solid and Leptos have. Updates
flow through reactive **sinks**: a style closure that reads a signal inside
itself, `rx!` on a prop, `text(move || …)`, `when`, `switch`. My code read state
at the top of the body and baked the result into the tree:

```rust
// what I wrote — `active` is a bool, decided once and frozen
let active = app.space_id.get() == space.id;
press(children,
      move || StyleApplication::new(RailButton::sheet())
                  .with("active", if active { "on" } else { "off" }),
      on_click)
```

```rust
// what it should have been — the sink reads the signal, so it re-tints in place
press(children,
      move || {
          let active = app.space_id.get() == space.id;
          StyleApplication::new(RailButton::sheet())
              .with("active", if active { "on" } else { "off" })
      },
      on_click)
```

The one-word difference is where the `.get()` sits relative to the closure.

**Verified both directions.** Stripping the `switch` off `Rail` and leaving the
build-time read froze the highlight while the (still-switched) room list
updated. Moving the same reads inside the style closure — with no `switch`
anywhere — made it correct:

```
rail before : TC=rgb(79,70,229)  ST=rgb(38,37,45)   ← Cabin selected
rail after  : TC=rgb(38,37,45)   ST=rgb(79,70,229)  ← Studio selected
```

`voice-client` never hits this because it binds signals into sinks throughout,
which is the idiom. Any app that builds *structure* from a build-time read hits
it immediately.

**What's actually left as a framework request:** a lint. `signal.get()` in a
`#[component]` body whose result is not routed into a sink is nearly always
this mistake, and the symptom points at event wiring — the one thing that is
working. `idealyst lint` already carries rules of comparable subtlety
(`prefer-keyed-list`). One diagnostic here would have saved hours, and would
save them for anyone arriving from a re-render framework, where reading state
at the top of a component is exactly right.

## 2. `switch` is for shape, never for paint

A corollary worth stating separately, because reaching for `switch` is the
natural overcorrection once you understand #1 — and I made it. Wrapping every
region in a `switch` keyed on the state it reads *works*, and it is wrong twice
over.

Wrong on cost: navigating between channels rebuilt forty rows to recolour two.

Wrong on correctness, sharply, for anything focus-related. The obvious key for a
composer includes whether it has focus:

```rust
switch(move || (app.draft.get().is_empty(), app.composer_focused.get()),
       move |_| composer_body(app))
```

Focusing the input fires `on_focus` → sets `composer_focused` → changes the key
→ rebuilds the subtree → destroys the `<input>` that just took focus. The field
cannot be typed into at all, and `activeElement` snaps back to `BODY` about
200ms after every click.

The rule Hearth now follows: **`switch` only where the subtree's shape differs**
— which view is showing, which room's messages, whether an overlay is mounted.
Everything presentational — selection highlights, tones, hover reveals, unread
pips, the focus ring — is a sink. `views/rail.rs` carries no `switch` at all and
is the reference for the pattern; `views/rooms.rs` shows the split, with a key
covering only what changes the list's shape and per-row selection read inside
each row's own style closure.

## 3. `ui!` silently reinterprets a braced child as the previous tag's children

```rust
ui! {
    view(style = pip_style)      // childless tag
    { button }                   // meant as a sibling
}
```

The `{ button }` is consumed as the *pip's* children block. No error — the tree
just comes out wrong. In the spaces rail the pip is `position: absolute` with
zero height, so the button nested inside it and the row collapsed to its
padding; three space buttons stacked on top of each other at the top of the
rail.

It's the same ambiguity for a bare-identifier splat:

```rust
view(...) {
    head          // a Vec<Element>
    { body }      // parsed as `head`'s children
}
```

Both are resolvable at parse time — a splat identifier and a childless tag are
distinguishable from a tag opening a block — and both currently fail silently.
Related: a nested `ui!` inside a braced child (`{ ui! { … } }`) *does* error, but
with `missing angle brackets in associated item path`, which points nowhere near
the cause.

## 4. `idealyst lint` errors on the entry point the CLI requires

```
error[component-pascal-case]: component `app` should be PascalCase
  --> ./src/lib.rs:56:8
  help: rename to `App`
```

The CLI's web/Android wrappers call `<lib>::app()`. Renaming as instructed
breaks the build. This is not specific to Hearth — the framework's own
`examples/nav-showcase` produces the identical error:

```
$ cd examples/nav-showcase && idealyst lint
error[component-pascal-case]: component `app` should be PascalCase
  --> ./src/lib.rs:64:8
✖ 2 problem(s) — 1 error(s), 1 warning(s)
```

Every idealyst app is one lint error from clean, forever. The rule should
exempt the entry point.

## 5. Spurious `unused variable` on a keyed `for` over a destructured item

```rust
for (line, heads) in rows, key = line.key.clone() {
    message_row(app, line, heads, compact)      // `heads` IS used
}
```

→ `warning: unused variable: heads`.

The macro appears to expand the pattern once for the key and once for the body;
bindings the key doesn't mention are reported unused against the key's copy. It
fires for every binding after the first, and it fired in three separate places
here. The workaround is a named struct with one binding — which is what
`chat::Row` and `mobile::Tab` are, and both carry a comment saying so.

The warning is noise, but it's the *wrong* kind: it accuses correct code of a
bug, so the honest response is to go and check that grouping still works.

## 6. `Modal` ignores `width` above ~520dp

`ModalProps::width` is documented as "desired width on a roomy viewport, capped
to the viewport width". On a 1440×900 viewport:

| requested | rendered surface |
| --- | --- |
| 352 | 350 ✓ |
| 540 | ~520 |
| 620 | ~520 |
| 790 | ~520 |

`effective_modal_width` is `desired.min((viewport_width - 32).max(280))`, which
on a 1440px viewport should cap at 1408 — so the viewport width the modal reads
does not look like the real one. Everything at or below `DEFAULT_MODAL_WIDTH`
(520) is unaffected, which is why the four dialogs here are fine and only the
device preview isn't.

Hearth's mobile preview needs 680 (a 372dp phone beside a 280dp note), so it is
the one overlay built on a bare scrim instead of `Modal` —
`views/mobile.rs::MobilePreview` explains why at the call site.

## 7. Autofocus inside a `Modal` is unreachable from app code

```rust
let focus: Ref<TextInputHandle> = Ref::new();
// … .bind(focus.clone()) on the text_input …
after_ms(250, move || { if let Some(h) = focus.get() { h.focus(); } });
```

The `Ref` fills and `focus()` runs (verified — replacing the call with a signal
write proved the branch is taken), but `Modal` moves focus to its own card layer
afterwards, and there's no prop to opt out. A search palette that opens
unfocused is a real defect in a chat client; every delay I tried lost the race.
`Modal` wants either a `initial_focus: Option<Ref<…>>` or a
`focus_on_open: bool`.

## 8. `StyleRules` has no `border_style`

No dashed or dotted borders. The design uses dashed outlines for both of its
"add something here" affordances — the new-space button in the rail and the
invite tile in a room — precisely because dashed reads as *placeholder* rather
than as a control. Rendered solid, they read as ordinary outlined buttons.

The workaround is one line in `styles.rs` and the affordance survives, but this
is a genuinely missing primitive rather than a stylistic nicety: "dashed" is the
standard visual language for a drop target or an empty slot, and there is no way
to express it.

## 9. No secondary click, and press handlers carry no event

`pressable(children, on_click)` takes `Fn()` — no event, no button, no
coordinates, no modifiers. There is no context-menu channel anywhere in the
vocabulary.

The design's person menu is a right-click at the pointer. Hearth opens the same
menu on a normal press and anchors it with `idea_ui::Menu` instead, with **View
profile** promoted to the first item so the design's primary action stays one
click away.

Worth saying: **the anchored version is better**, and I'd keep it even if
right-click existed. A menu pinned to its row lands correctly on every backend,
whereas viewport coordinates are a web idea that has to be re-derived on iOS and
Android. But "the app cannot distinguish a left click from a right click"
is a hard limit that desktop-class apps will keep hitting — Hearth alone wants
it for messages, rooms, and channels as well as people.

## 10. `Menu` doesn't height-cap or flip near the viewport edge

`menu.rs` says its panel "delegates to the family-wide scrolling panel so a long
item list scrolls within a height-capped panel instead of running off the bottom
of the viewport". It doesn't. A 7-item person menu opened on a row near the
bottom of the room list runs past the viewport and the last item —
**Remove from space…**, the destructive one — is clipped.

`side`/`align` exist as props but are static and there's no measurement
available to app code, so the app can't choose the right side itself.

## 10b. An anchored `Menu` does not survive its anchor being rebuilt

`Menu` anchors to a `Ref<ViewHandle>` bound to the row that opened it. If the
region containing that row re-renders, the handle is a new one, and the open
menu goes with it — silently, with no error and nothing in the console.

That is defensible on its own. What makes it a trap is how easy it is to key a
region on something that changes several times a second without noticing:
Hearth's room list was keyed on who is talking, because the speaking highlight
is built inside it, so during a call the whole list — and any menu open on it —
was rebuilt on every VAD event. The symptom is a menu that "sometimes doesn't
open", which reads as a flaky click rather than as a rebuild.

Two things would help, neither of them large:

- `Menu` could **re-anchor** rather than unmount when its target's handle is
  replaced, which is what a person would expect an anchored overlay to do.
- `Slider`'s module docs already warn that a host must not re-key it mid-drag
  ("that destroys the pointer-capturing view and the drag stalls after the
  first move"). The same warning belongs on `Menu`, and on anything else built
  on `anchored_overlay` — it is the same failure with a different symptom.

The fix on the app side is the framework's own idiom and worth writing down,
because it is not obvious in this direction: **a style closure is a reactive
sink**. Reading a fast-changing signal *inside* one repaints without a rebuild,
so the enclosing `switch` does not have to carry it in its key at all. The
guidance that state read at build time needs a `switch`/`rx!` sink has an
inverse — state read at build time *forces* a rebuild, and putting it in a sink
is how you stop paying for one.

## 11. `AppShell` and `Drawer` wrap content in a block, not a flex container

`idea_ui_nav::AppShell` documents its content wrapper as "Main content: fills
the shell", and it does set `height: 100%`, `min-height: 0`, and the
pinned-sidebar margin. What it does not set is `display: flex`.

That is invisible until you put a `flex_grow: 1.0` child in it — which is the
natural thing to do, because every other container in the framework is a flex
box. The child has no flex context, falls back to content height, and the
layout collapses in a way that looks like a bug in your own column:

```
margin-left: 316px   height 900   display: block   <- AppShell content wrapper
  flex-grow: 1       height  56   display: flex    <- our main column, collapsed
    header           height  56
    body             height   0                    <- everything below it gone
```

The composer ends up under the header and the transcript disappears. `Drawer`'s
container has the same shape (`position: relative; width/height: 100%`, no
`display`), so wrapping a flex column in a `Drawer` fails the same way.

The workaround is one line per affected root — an explicit
`height: Length::pct(100.0)` instead of relying on `flex_grow`:

```rust
pub Main<()> {
    base(_t) {
        flex_grow: 1.0,        // still right when the parent IS a flex box
        height: Length::pct(100.0),  // ...and this is what actually applies here
        min_height: 0.0,
        flex_direction: FlexDirection::Column,
    }
}
```

Both wrappers exist to hold exactly one subtree of app layout, and that subtree
is almost always a flex column. Adding `display: flex` +
`flex_direction: column` to those two sheets would make the documented
behaviour ("fills the shell") true without changing anything for content that
does not use flex.

Worth noting the failure is silent and viewport-dependent: at desktop widths
the collapse looked like a styling mistake in the chat column, and it only
became obviously structural when the same subtree broke inside a `Drawer` at
phone width.

## 12. A refused WebSocket handshake throws out of the SDK

Opening a `Socket` whose upgrade is rejected — the ordinary case for a client
that connects before it has a session — raises an uncaught

```
Error: closure invoked recursively or after being dropped
```

from `wasm-bindgen`, once per attempt. `Socket::connect` returns its `Err` as
well, so the app's own error handling runs; the throw is extra, escapes the
future, and lands as a page error the app cannot catch.

It is easy to hit and hard to attribute. Anything with a reconnect loop
retries, so one mistimed connection becomes a stream of identical uncaught
errors with no stack pointing anywhere in the app. Ours surfaced as a failing
"no runtime errors" assertion in the browser suite, four occurrences per run,
long after the code that caused it was written.

Reproduction is one line — `Socket::connect` at an endpoint whose extractor
refuses, with no cookie:

```
WebSocket connection to 'ws://…/_srv/_ws/live' failed:
  HTTP Authentication failed; no valid credentials available
Uncaught Error: closure invoked recursively or after being dropped
```

The workaround is to not connect until the connection can succeed, which Hearth
now does — it waits for `Phase::Ready`. That is the right behaviour anyway, so
the cost here is only the time spent finding out why. A failed handshake should
resolve the future with `Err` and drop its callbacks without invoking them.

## 13. `denoise` on native is unusable from a server without a thread of your own

The SDK is an `AudioStream -> AudioStream` transformer, which is exactly the
right shape for the client it was written for. On a server it collides with
`tokio`: `AudioStream` is `!Send` (it holds a main-thread native source on the
platform it was designed for), so neither the input nor the output handle can
live in a `tokio::spawn` future — and every connection a relay serves is one.

The `Send` half, `AudioWriter`, has no such problem. So the shape that works is
one dedicated OS thread per process that owns every `!Send` handle, with
connections holding only writers and pushing into them
(`voice-server/src/suppress.rs`). That is about eighty lines of plumbing to use
an SDK whose actual inference already runs on its own worker thread.

Two things would remove it:

- **Make the handles `Send` on non-web targets.** The `!Send`-ness is a web
  constraint (`Rc`, a main-thread `MediaStreamTrack`) that the native path
  inherits without needing.
- **Expose the framing engine directly** — a `feed(&[f32], rate, channels)`
  that emits blocks through a callback, with no stream on either side. That is
  what `Engine` already is internally, and it is what a server, a file
  transcoder, and a test all actually want.

Neither is urgent: the workaround is contained, and the SDK's *audio* work is
excellent — one implementation, no per-OS code, and 0.11 ms of CPU per 10 ms
hop on a Ryzen 7 7700X. It is only the ownership model that assumes a client.

Second, smaller note on the same crate: `Denoiser::new()` embeds the model on
native but there is no equivalent constant for the model's **bytes**, so a
server that wants to hand the same weights to something else has no handle on
them. And `DfParams::from_bytes` requiring `&'static [u8]` propagates a
one-time leak into every web caller — worth wrapping in the SDK (it copies the
bytes internally anyway) rather than making each app write `Box::leak`.

## 14. Small things

- **A bare `view` is `display: block`, not a flex column.** idea-ui's own
  documentation says containers are "flex-column by default", which is true of
  `Stack`/`Surface` and not of the `view` primitive. Two `text` children of a
  bare `view` flow inline and run together — that shipped a settings page
  reading `SettingsSeven of them. That's the whole list.` and a roster reading
  `Ana RuizOn The Porch`. A one-line note in the layout docs would cover it.
- **`flex_basis: 0` on a scroll view inside a content-sized parent collapses it
  to nothing.** The search results vanished entirely — empty state and all —
  until the container was given a `max_height` instead. Correct flex behaviour,
  but a zero-height scroll region is invisible rather than obviously broken.
- **`idealyst build --web` emits no favicon**, so every page load logs a 404.
  Harmless, but it's the only console error in a clean run, which trains people
  to ignore console errors.
- **`prefer-ui-macro` false-positives on `http::Response::builder()`** in
  `voice-server` — it matches any `builder` identifier, not element builders.

---

## What worked, and is worth saying

Not everything is a complaint, and the good parts here are load-bearing:

- **The token vocabulary is excellent.** Hearth's light/dark schemes and its
  four accents are one `install_idea_theme_reactive` closure and a set of token
  values. Flipping either re-tints every `idea-ui` component *and* every one of
  the ~60 app-level `stylesheet!` blocks, with no per-component work, because
  both read the same token names. `t.spacing.md()` failing to compile when the
  token doesn't exist caught two typos before they rendered.
- **`stylesheet!`'s variant axes are the right shape for app chrome.** Every
  piece of state-dependent styling in Hearth — active rows, presence dots,
  speaking rings, hover reveals, density — is a variant axis, resolved at build
  time with no runtime branching in the view code.
- **`idea-ui` covered the whole control surface.** Every control in the design
  is a stock component; the app-level styles are all *layout*. That's the
  outcome a design system is supposed to produce, and it's why the accent swap
  reaches everything.
- **`icons-lucide` per-`const` tree-shaking** meant using ~25 icons cost
  nothing, and the constants matched the design's SVGs one-for-one.
- **`idealyst lint`'s `prefer-keyed-list` found real bugs.** Every hand-built
  `Vec<Element>` it flagged was a list where per-row state would attach to the
  wrong row. The suppression comment for the genuinely-fixed-arity cases is the
  right escape hatch.
