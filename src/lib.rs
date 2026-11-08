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
//! Nothing reads the tree until assistive tech asks for it (VoiceOver, agent-desktop, an AT-SPI client), so an app
//! can skip building it until then: `is_active` says whether one is reading, and `take_asked` says, once, that one
//! has just asked. Its first query gets the last tree `update` gave, or an empty placeholder if there was none; the
//! app's next `update` then replaces it with the full tree. Poll `take_asked` (a timer, as for the actions) and
//! update at once: a window that's hidden draws no frames, so waiting for one could wait for ever.
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
    /// assistive tech is reading the tree: it asked for it, and (on Linux) hasn't let go since
    active: bool,
    /// it asked since the app last looked (`take_asked`)
    asked: bool,
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

    /// The adapter asks for the whole tree (its first query, or on Linux after AT-SPI comes back): what it gets now,
    /// None (it parks on a placeholder until the next `update`, which must then be the full tree) if there's none yet.
    fn activate(&mut self) -> Option<TreeUpdate> {
        self.active = true;
        self.asked = true;
        let full = self.full();
        self.initial_given = full.is_some();
        full
    }

    /// Nothing reads the tree any more (Linux: AT-SPI is off): forget it, as the app may stop sending it now, so a
    /// reader that comes back waits for the app's full tree rather than getting a stale one.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn deactivate(&mut self) {
        *self = Shared::default();
    }

    fn take_asked(&mut self) -> bool {
        std::mem::take(&mut self.asked)
    }
}

struct Activation(Arc<Mutex<Shared>>);

impl ActivationHandler for Activation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.0.lock().unwrap().activate()
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct Deactivation(Arc<Mutex<Shared>>);

#[cfg(target_os = "linux")]
impl accesskit::DeactivationHandler for Deactivation {
    fn deactivate_accessibility(&mut self) {
        self.0.lock().unwrap().deactivate();
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

/// Makes the view's window class answer AXFocusedUIElement with the view's own answer, once per class. Without it
/// AppKit answers with the window itself, so the focused node is never the one the app reports, and AX `focus` /
/// `set-value` from outside never reach it. (accesskit_macos's `add_focus_forwarder_to_window_class` asks the window's
/// contentView, but GPUI's NSView is a subview of that, so it would reach the wrong view: this keeps the view with the
/// window instead.)
///
/// # Safety
/// `view` must be a live NSView, and this must run on the main thread.
#[cfg(target_os = "macos")]
unsafe fn forward_focus(view: *mut std::ffi::c_void) {
    use objc2::ffi;
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    use objc2::sel;
    use std::ffi::c_void;
    unsafe extern "C" {
        fn class_getMethodImplementation(cls: *const c_void, sel: *const c_void) -> *const c_void;
    }
    static KEY: u8 = 0;
    static DONE: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    extern "C" fn forwarder(this: *mut AnyObject, cmd: Sel) -> *mut AnyObject {
        unsafe {
            let view = ffi::objc_getAssociatedObject(this as *mut _, &KEY as *const u8 as *const c_void) as *mut AnyObject;
            if !view.is_null() {
                return msg_send![view, accessibilityFocusedUIElement];
            }
            // a window of this class that isn't ours: what the class's superclass would have answered
            let mut class = (*this).class() as *const AnyClass;
            // the lock is dropped before the call below: the superclass's implementation may be another patched
            // class's forwarder, which takes it again
            {
                let done = DONE.lock().unwrap();
                while !class.is_null() && !done.contains(&(class as usize)) {
                    class = ffi::class_getSuperclass(class as *mut _) as *const AnyClass;
                }
            }
            let sup = if class.is_null() { std::ptr::null_mut() } else { ffi::class_getSuperclass(class as *mut _) };
            if sup.is_null() {
                return std::ptr::null_mut();
            }
            let f = class_getMethodImplementation(sup as *const c_void, cmd.as_ptr() as *const c_void);
            let f: extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject = std::mem::transmute(f);
            f(this, cmd)
        }
    }

    let view = view as *mut AnyObject;
    // SAFETY: `view` is a live NSView.
    let window: *mut AnyObject = unsafe { msg_send![view, window] };
    if window.is_null() {
        return; // not in a window yet: nothing to forward from
    }
    unsafe {
        // keeps the view alive with the window (the view only holds the window weakly)
        ffi::objc_setAssociatedObject(
            window as *mut _,
            &KEY as *const u8 as *const c_void,
            view as *mut _,
            ffi::OBJC_ASSOCIATION_RETAIN_NONATOMIC,
        );
        let class = (*window).class() as *const AnyClass;
        let mut done = DONE.lock().unwrap();
        if !done.contains(&(class as usize)) {
            let imp: extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject = forwarder;
            ffi::class_replaceMethod(
                class as *mut _,
                sel!(accessibilityFocusedUIElement).as_ptr() as _,
                Some(std::mem::transmute::<extern "C" fn(*mut AnyObject, Sel) -> *mut AnyObject, unsafe extern "C" fn()>(imp)),
                c"@@:".as_ptr(),
            );
            done.push(class as usize);
        }
    }
}

#[cfg(target_os = "macos")]
type Adapter = accesskit_macos::SubclassingAdapter;
#[cfg(target_os = "linux")]
type Adapter = accesskit_unix::Adapter;

pub struct A11y {
    /// None for a detached A11y (`detached`)
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    adapter: Option<RefCell<Adapter>>,
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
            unsafe { forward_focus(h.ns_view.as_ptr()) };
            let adapter = unsafe {
                accesskit_macos::SubclassingAdapter::new(
                    h.ns_view.as_ptr(),
                    Activation(shared.clone()),
                    Actions(tx),
                )
            };
            (A11y { adapter: Some(RefCell::new(adapter)), shared }, rx)
        }
        #[cfg(target_os = "linux")]
        {
            let _ = window;
            let adapter = accesskit_unix::Adapter::new(
                Activation(shared.clone()),
                Actions(tx),
                Deactivation(shared.clone()),
            );
            (A11y { adapter: Some(RefCell::new(adapter)), shared }, rx)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (window, tx);
            (A11y { shared, _p: RefCell::new(()) }, rx)
        }
    }

    /// An A11y with no window and no adapter, for tests and benches (GPUI's test platform has no view to attach to):
    /// `update` keeps the tree as `attach`'s does while nothing reads it, nothing is handed on, and no actions come.
    /// `ask` stands in for a reader.
    pub fn detached() -> (A11y, Receiver<ActionRequest>) {
        let (_, rx) = channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let a11y = A11y { adapter: None, shared };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let a11y = A11y { shared, _p: RefCell::new(()) };
        (a11y, rx)
    }

    /// A reader asking for the whole tree, as the adapter does (`request_initial_tree`): what it gets now, None for
    /// the placeholder. From then on `is_active`, and `take_asked` says so once. For a detached A11y (tests): an
    /// attached one's adapter asks by itself.
    pub fn ask(&self) -> Option<TreeUpdate> {
        self.shared.lock().unwrap().activate()
    }

    /// The tree as kept, merged from every `update`: what a reader that asked now would get.
    pub fn tree(&self) -> Option<TreeUpdate> {
        self.shared.lock().unwrap().full()
    }

    /// Whole or partial tree, after each render; bounds in window points.
    /// The first update must carry `tree: Some(..)`.
    pub fn update(&self, update: TreeUpdate) {
        // What to hand the adapter. Once it holds a full tree, the update as it is. Until then, our
        // whole merged tree: the adapter may be parked on an empty placeholder (its first query came
        // before our first update, so `request_initial_tree` had nothing to give), and only a full
        // tree replaces that. If there is no complete tree yet, send nothing.
        let (send, was_given, active) = {
            let mut s = self.shared.lock().unwrap();
            s.merge(&update);
            let active = s.active;
            if s.initial_given { (Some(update), true, active) } else { (s.full(), false, active) }
        };
        let Some(send) = send else { return };
        // detached: nothing reads it
        #[cfg(target_os = "macos")]
        if let Some(adapter) = &self.adapter {
            // `None` while the adapter is still inactive (nothing has queried this window yet): then
            // its first query calls `request_initial_tree`, which hands over the merged tree.
            let events = adapter.borrow_mut().update_if_active(|| send);
            if let Some(events) = events {
                if !was_given {
                    self.shared.lock().unwrap().initial_given = true;
                }
                events.raise();
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(adapter) = &self.adapter {
            // a pending unix adapter wants a full tree from its first update; `update_if_active` says
            // nothing, so count the full tree as given once sent to an active one (an inactive adapter asks for it
            // itself, and `activate` says again whether it got one)
            if !was_given && active {
                self.shared.lock().unwrap().initial_given = true;
            }
            adapter.borrow_mut().update_if_active(|| send);
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = (send, was_given);
        #[cfg(not(target_os = "linux"))]
        let _ = active;
    }

    /// Whether assistive tech is reading this window's tree: it has asked for it, and (on Linux) hasn't let go since.
    /// macOS's adapter never lets go, so there it stays true once asked. While it's false, `update` only keeps the
    /// tree for a reader to come, and an app can skip building it: see `take_asked`.
    pub fn is_active(&self) -> bool {
        self.shared.lock().unwrap().active
    }

    /// True once each time assistive tech asks for the whole tree: its first query (macOS), or AT-SPI coming on
    /// (Linux). It got the last tree `update` gave, or a placeholder if there was none, so send the full tree now
    /// (`update`), from outside the adapter's call (a timer that polls this, say). Then false until it asks again.
    pub fn take_asked(&self) -> bool {
        self.shared.lock().unwrap().take_asked()
    }

    /// Tell the adapter whether the window is key (VoiceOver follows focus only in an active window).
    pub fn set_window_focused(&self, focused: bool) {
        #[cfg(target_os = "macos")]
        if let Some(events) = self.adapter.as_ref().and_then(|a| a.borrow_mut().update_view_focus_state(focused)) {
            events.raise();
        }
        #[cfg(target_os = "linux")]
        if let Some(adapter) = &self.adapter {
            adapter.borrow_mut().update_window_focus_state(focused);
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = focused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use accesskit::{Role, TreeId, TreeInfo};

    /// a window (0) with a button (1) that has the focus
    fn tree() -> TreeUpdate {
        let mut root = Node::new(Role::Window);
        root.set_children(vec![NodeId(1)]);
        TreeUpdate {
            nodes: vec![(NodeId(0), root), (NodeId(1), Node::new(Role::Button))],
            tree: Some(TreeInfo::new(NodeId(0))),
            tree_id: TreeId::ROOT,
            focus: NodeId(1),
        }
    }

    /// AT-SPI going off (Linux's deactivation handler; elsewhere the same call, which macOS's adapter never makes)
    fn deactivate(shared: &Arc<Mutex<Shared>>) {
        #[cfg(target_os = "linux")]
        accesskit::DeactivationHandler::deactivate_accessibility(&mut Deactivation(shared.clone()));
        #[cfg(not(target_os = "linux"))]
        shared.lock().unwrap().deactivate();
    }

    #[test]
    fn a_reader_that_asks_before_any_update_parks_and_the_app_hears_it_once() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        assert!(!shared.lock().unwrap().active, "nothing reads the tree before a reader asks");
        assert!(!shared.lock().unwrap().take_asked());
        // its first query comes before the app sent anything: a placeholder, so the next update must be the full tree
        assert!(Activation(shared.clone()).request_initial_tree().is_none());
        let mut s = shared.lock().unwrap();
        assert!(s.active, "a reader is reading");
        assert!(!s.initial_given, "the next update is sent whole");
        assert!(s.take_asked(), "the app hears that it asked");
        assert!(!s.take_asked(), "once");
        assert!(s.active, "and it stays active");
    }

    #[test]
    fn a_reader_after_an_update_gets_that_tree_and_one_that_comes_back_waits_for_a_new_one() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        shared.lock().unwrap().merge(&tree());
        let first = Activation(shared.clone()).request_initial_tree().expect("the tree the app last sent");
        assert_eq!((first.nodes.len(), first.focus), (2, NodeId(1)));
        {
            let mut s = shared.lock().unwrap();
            assert!(s.active && s.initial_given && s.take_asked());
        }

        deactivate(&shared);
        {
            let mut s = shared.lock().unwrap();
            assert!(!s.active, "nobody reads it now");
            assert!(!s.take_asked() && !s.initial_given);
            assert!(s.full().is_none(), "the tree is forgotten: the app stops sending it");
        }

        // back on: no stale tree, it parks until the app's full one, and the app hears it asked
        assert!(Activation(shared.clone()).request_initial_tree().is_none());
        let mut s = shared.lock().unwrap();
        assert!(s.active && !s.initial_given && s.take_asked());
    }

    #[test]
    fn a_detached_a11y_is_read_only_once_asked() {
        let (a11y, actions) = A11y::detached();
        assert!(!a11y.is_active() && !a11y.take_asked(), "nothing reads it yet");
        a11y.set_window_focused(true);
        assert!(a11y.ask().is_none(), "nothing sent yet: the placeholder");
        assert!(a11y.is_active());
        assert!(a11y.take_asked() && !a11y.take_asked(), "the app hears it once");
        a11y.update(tree());
        let kept = a11y.tree().expect("the tree it was sent");
        assert_eq!((kept.nodes.len(), kept.focus), (2, NodeId(1)));
        assert!(actions.try_recv().is_err(), "no actions come");
    }
}
