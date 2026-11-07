//! An accessibility tree for GPUI windows, on accesskit: accesskit_macos on macOS (VoiceOver), accesskit_unix on Linux (AT-SPI).
//!
//! ```ignore
//! let (a11y, actions) = A11y::attach(window);
//! a11y.update(tree_update); // after each render; bounds in window points
//! // ActionRequest: Click, SetValue, Focus … : drain `actions` on GPUI's thread, e.g. from a
//! // task: `while let Ok(req) = actions.try_recv() { … }`
//! ```
//!
//! `attach` and `update` must be called on the main thread (GPUI's UI thread).
//!
//! Actions are queued, never run in place. AppKit calls the adapter's `do_action` from inside the
//! accessibility call, and the caller (VoiceOver, agent-desktop) waits for it to return: anything
//! slow there, or anything that re-enters GPUI, holds the caller up. So `do_action` only sends the
//! request to the channel `attach` returned, and returns at once; the app drains the channel
//! itself, on GPUI's thread, whenever it likes.

pub use accesskit;

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, Node, NodeId, TreeUpdate};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// What the adapter may ask for from its own callbacks: the last known tree, merged from every
/// `update`, so a screen reader that connects late still gets a full tree.
#[derive(Default)]
struct Shared {
    nodes: HashMap<NodeId, Node>,
    tree: Option<accesskit::TreeInfo>,
    focus: Option<NodeId>,
    /// set once the adapter has been handed a full tree
    initial_given: bool,
}

impl Shared {
    fn merge(&mut self, update: &TreeUpdate) {
        for (id, node) in &update.nodes {
            self.nodes.insert(*id, node.clone());
        }
        if let Some(tree) = &update.tree {
            self.tree = Some(tree.clone());
        }
        self.focus = Some(update.focus);
    }

    /// The whole known tree: only nodes reachable from the root (a partial update may have
    /// orphaned others, and accesskit rejects unreachable nodes).
    fn full(&self) -> Option<TreeUpdate> {
        let tree = self.tree.clone()?;
        let mut out = Vec::new();
        let mut stack = vec![tree.root];
        while let Some(id) = stack.pop() {
            let mut node = self.nodes.get(&id)?.clone();
            // a child the app never sent (or no longer sends) would make accesskit reject the tree
            if node.children().iter().any(|c| !self.nodes.contains_key(c)) {
                let kept: Vec<NodeId> =
                    node.children().iter().copied().filter(|c| self.nodes.contains_key(c)).collect();
                node.set_children(kept);
            }
            stack.extend(node.children().iter().copied());
            out.push((id, node));
        }
        let focus = self
            .focus
            .filter(|f| out.iter().any(|(id, _)| id == f))
            .unwrap_or(tree.root);
        Some(TreeUpdate { nodes: out, tree: Some(tree), tree_id: accesskit::TreeId::ROOT, focus })
    }
}

struct Activation(Arc<Mutex<Shared>>);

impl ActivationHandler for Activation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        let mut s = self.0.lock().unwrap();
        let full = s.full();
        if full.is_some() {
            s.initial_given = true;
        }
        full
    }
}

struct Deactivation(Arc<Mutex<Shared>>);

#[cfg(target_os = "linux")]
impl accesskit::DeactivationHandler for Deactivation {
    fn deactivate_accessibility(&mut self) {
        self.0.lock().unwrap().initial_given = false;
    }
}

/// Queues each request and returns: see the crate docs.
struct Actions(Sender<ActionRequest>);

impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        // a dropped receiver means the app no longer wants actions: nothing to do
        let _ = self.0.send(request);
    }
}

pub struct A11y {
    #[cfg(target_os = "macos")]
    adapter: RefCell<accesskit_macos::SubclassingAdapter>,
    #[cfg(target_os = "linux")]
    adapter: RefCell<accesskit_unix::Adapter>,
    shared: Arc<Mutex<Shared>>,
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    _p: RefCell<()>,
}

impl A11y {
    /// Finds the window's NSView (raw-window-handle) and installs accesskit_macos's adapter.
    /// Call it before the window is first shown or focused if you can (right after
    /// `open_window`, in the root view's constructor); VoiceOver picks it up on its next query.
    /// On Linux it registers an AT-SPI adapter; the window isn't needed, and the adapter's handlers run
    /// on its bus thread (fine: actions only go into the queue). On other platforms this is a no-op.
    ///
    /// Returns the adapter and the queue of actions that assistive tech asks for (`Click`,
    /// `SetValue`, `Focus` …). Drain it on GPUI's thread.
    pub fn attach(window: &gpui::Window) -> (A11y, Receiver<ActionRequest>) {
        let (tx, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        #[cfg(target_os = "macos")]
        {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let handle = HasWindowHandle::window_handle(window).expect("gpui window has no handle");
            let RawWindowHandle::AppKit(h) = handle.as_raw() else {
                panic!("gpui window is not an AppKit window");
            };
            // SAFETY: ns_view is GPUI's live NSView, and we are on the main thread.
            let adapter = unsafe {
                accesskit_macos::SubclassingAdapter::new(
                    h.ns_view.as_ptr(),
                    Activation(shared.clone()),
                    Actions(tx),
                )
            };
            (A11y { adapter: RefCell::new(adapter), shared }, rx)
        }
        #[cfg(target_os = "linux")]
        {
            let _ = window;
            let adapter = accesskit_unix::Adapter::new(
                Activation(shared.clone()),
                Actions(tx),
                Deactivation(shared.clone()),
            );
            (A11y { adapter: RefCell::new(adapter), shared }, rx)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (window, tx);
            (A11y { shared, _p: RefCell::new(()) }, rx)
        }
    }

    /// Whole or partial tree, after each render; bounds in window points.
    /// The first update must carry `tree: Some(..)`.
    pub fn update(&self, update: TreeUpdate) {
        // What to hand the adapter. Once it holds a full tree, the update as it is. Until then, our
        // whole merged tree: the adapter may be parked on an empty placeholder (its first query came
        // before our first update, so `request_initial_tree` had nothing to give), and only a full
        // tree replaces that. If there is no complete tree yet, send nothing.
        let (send, was_given) = {
            let mut s = self.shared.lock().unwrap();
            s.merge(&update);
            if s.initial_given { (Some(update), true) } else { (s.full(), false) }
        };
        let Some(send) = send else { return };
        #[cfg(target_os = "macos")]
        {
            // `None` while the adapter is still inactive (nothing has queried this window yet): then
            // its first query calls `request_initial_tree`, which hands over the merged tree.
            let events = self.adapter.borrow_mut().update_if_active(|| send);
            if let Some(events) = events {
                if !was_given {
                    self.shared.lock().unwrap().initial_given = true;
                }
                events.raise();
            }
        }
        #[cfg(target_os = "linux")]
        {
            // a pending unix adapter wants a full tree from its first update; `update_if_active` says
            // nothing, so count the full tree as given once sent (an inactive adapter asks for it itself)
            if !was_given {
                self.shared.lock().unwrap().initial_given = true;
            }
            self.adapter.borrow_mut().update_if_active(|| send);
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = (send, was_given);
    }

    /// Tell the adapter whether the window is key (VoiceOver follows focus only in an active window).
    pub fn set_window_focused(&self, focused: bool) {
        #[cfg(target_os = "macos")]
        if let Some(events) = self.adapter.borrow_mut().update_view_focus_state(focused) {
            events.raise();
        }
        #[cfg(target_os = "linux")]
        self.adapter.borrow_mut().update_window_focus_state(focused);
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = focused;
    }
}
