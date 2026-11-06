//! A macOS accessibility tree for GPUI windows, on accesskit and accesskit_macos.
//!
//! ```ignore
//! let a11y = A11y::attach(window, |req| { /* ActionRequest: Click, SetValue, Focus … */ });
//! a11y.update(tree_update); // after each render; bounds in window points
//! ```
//!
//! `attach` and `update` must be called on the main thread (GPUI's UI thread). `on_action` is
//! called on the main thread too, from inside the AppKit accessibility call: keep it short and
//! hand the work to GPUI (e.g. send it to a channel that an `AsyncApp` task drains).

pub use accesskit;

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, Node, NodeId, TreeUpdate};
use std::cell::RefCell;
use std::collections::HashMap;
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
            let node = self.nodes.get(&id)?;
            stack.extend(node.children().iter().copied());
            out.push((id, node.clone()));
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

struct Actions<F>(F);

impl<F: Fn(ActionRequest) + Send + 'static> ActionHandler for Actions<F> {
    fn do_action(&mut self, request: ActionRequest) {
        (self.0)(request)
    }
}

pub struct A11y {
    #[cfg(target_os = "macos")]
    adapter: RefCell<accesskit_macos::SubclassingAdapter>,
    shared: Arc<Mutex<Shared>>,
    #[cfg(not(target_os = "macos"))]
    _p: RefCell<()>,
}

impl A11y {
    /// Finds the window's NSView (raw-window-handle) and installs accesskit_macos's adapter.
    /// Call it before the window is first shown or focused if you can (right after
    /// `open_window`, in the root view's constructor); VoiceOver picks it up on its next query.
    /// On other platforms this is a no-op.
    pub fn attach(
        window: &gpui::Window,
        on_action: impl Fn(ActionRequest) + Send + 'static,
    ) -> A11y {
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
                    Actions(on_action),
                )
            };
            A11y { adapter: RefCell::new(adapter), shared }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (window, on_action);
            A11y { shared, _p: RefCell::new(()) }
        }
    }

    /// Whole or partial tree, after each render; bounds in window points.
    /// The first update must carry `tree: Some(..)`.
    pub fn update(&self, update: TreeUpdate) {
        let partial = {
            let mut s = self.shared.lock().unwrap();
            s.merge(&update);
            // before the adapter has been given a tree, it will ask for ours: nothing to send
            s.initial_given
        };
        #[cfg(target_os = "macos")]
        if partial {
            if let Some(events) = self.adapter.borrow_mut().update_if_active(|| update) {
                events.raise();
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = partial;
    }

    /// Tell the adapter whether the window is key (VoiceOver follows focus only in an active window).
    pub fn set_window_focused(&self, focused: bool) {
        #[cfg(target_os = "macos")]
        if let Some(events) = self.adapter.borrow_mut().update_view_focus_state(focused) {
            events.raise();
        }
        #[cfg(not(target_os = "macos"))]
        let _ = focused;
    }
}
