# gpui-a11y

A macOS accessibility tree for [GPUI](https://www.gpui.rs) windows, built on
[AccessKit](https://github.com/AccessKit/accesskit) (`accesskit` + `accesskit_macos`).

GPUI 0.2.2 exposes no accessibility tree, so screen readers and UI automation see an empty
window. This crate attaches AccessKit's macOS adapter to a GPUI window's `NSView` (found through
`raw-window-handle`) and lets the app describe its UI as an AccessKit tree. No GPUI fork.

```rust
use gpui_a11y::{A11y, accesskit};

// once, after opening the window (main thread)
let a11y = A11y::attach(window, |req: accesskit::ActionRequest| {
    // Click, SetValue, Focus …: keep it short, hand the work to GPUI (e.g. a channel)
});

// after each render
a11y.update(tree_update);          // bounds in window points
a11y.set_window_focused(true);     // on window activation changes
```

`attach`, `update` and the action callback all run on the main thread.

## Example

```sh
cargo run --example demo
```

A window with a list of three rows and a text field. Clicking a row and setting the field's
value both work through the accessibility API (tested with
[agent-desktop](https://github.com/vercel-labs/agent-desktop)).

## Status

Early: about 150 lines, macOS only. Not yet done:

- keeping accessibility focus in step with GPUI focus (window focus is wired)
- scrolling lists: only visible rows are in the tree; no scroll actions or position
- partial updates and announcements (a full tree is sent each frame)
- text ranges: selection and caret for text fields and static text
- a VoiceOver pass

## License

MIT. See [LICENSE](LICENSE).
