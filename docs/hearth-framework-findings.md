# Framework findings from building Hearth

Everything here came out of implementing `Hearth.dc.html` on idealyst 1.3.16 —
a ~3,000-line app crate, no framework changes, `idea-ui` for every control.
Each item is something that cost debugging time, produced a wrong result, or
forced a documented departure from the design. They're ordered by how much they
cost, not by severity to the framework.

Nothing here blocked the app. All of it is now worked around in `crates/hearth`,
and the workaround is commented at the site.

---

## 1. Nothing tells you a component body is not a reactive scope

**Cost: the largest single block of debugging in the build.**

The natural way to write a view is to read state at the top and build a tree
from it:

```rust
#[component]
fn RoomList(app: App) -> Element {
    let selected = app.room_id.get();          // ← reads once, forever
    ui! { /* … built from `selected` … */ }
}
```

This compiles, lints clean, renders correctly on first paint, and then never
updates again. Handlers fire, signals commit, and nothing moves. The app looked
completely inert while being, internally, completely correct.

Fine-grained reactivity is the right model and `switch`/`when`/`rx!` are the
right tools — the issue is purely that nothing points at the mistake. The
symptom (a dead UI) points at event wiring, which is the one thing that was
working. I went as far as verifying `onclick` was attached on the DOM node and
firing before the real cause occurred to me.

Two things would have collapsed this to minutes:

- **A lint.** `signal.get()` in a `#[component]` body whose result is not routed
  into a reactive sink is nearly always this bug. `idealyst lint` already has
  the AST access to spot it, and already carries rules of comparable subtlety
  (`prefer-keyed-list`).
- **A named entry in the docs.** The component-authoring guidance in `CLAUDE.md`
  §9 covers `#[component]`, `ui!`, keyed lists, and prop reactivity in detail,
  and never states that a component body runs once. §9.1 says the macro
  "generates the props struct's `BuildElement` impl" — true, and not the thing
  a newcomer needs to know first.

`voice-client` doesn't hit this because it binds signals directly into
primitives (`text(move || …)`). Any app that renders *structure* from state
hits it immediately.

## 2. A structural `switch` key that includes focus state destroys the field

The direct sequel to #1, and worth its own entry because the fix for #1 causes
it. Having learned to wrap regions in `switch`, the obvious key for a composer
is "what changes how it's painted" — which includes whether it has focus:

```rust
switch(
    move || (app.draft.get().is_empty(), app.composer_focused.get()),
    move |_| composer_body(app),
)
```

Focusing the input fires `on_focus` → sets `composer_focused` → changes the key
→ rebuilds the subtree → destroys the `<input>` that just took focus. The field
cannot be typed into at all, and `document.activeElement` snaps back to `BODY`
about 200ms after every click.

The correct shape is that per-keystroke and per-focus state belongs in reactive
*sinks* (a style closure that reads the signal inside, `rx!` props), never in a
structural key. That's a sharp, teachable rule and I found it by bisecting.
`crates/hearth/src/views/chat.rs::Composer` carries the explanation.

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

## 11. Small things

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
