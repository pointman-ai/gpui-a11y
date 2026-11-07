//! Two windows in one process, each with its own accessibility tree.
//! Try: agent-desktop snapshot --app two_windows -i  (both windows' rows should show)
use accesskit::{Action, ActionRequest, Node, NodeId, Rect, Role, TreeInfo, TreeUpdate};
use gpui::{prelude::*, *};
use gpui_a11y::A11y;
use std::sync::mpsc::Receiver;
use std::time::Duration;

const ROOT: NodeId = NodeId(0);
const W: f64 = 300.0;
const H: f64 = 160.0;
const ROW_H: f64 = 40.0;

struct Win {
    name: &'static str,
    a11y: std::rc::Rc<A11y>,
    clicks: usize,
    /// no tree before this: models a window whose first frame comes after its first AX query
    tree_from: std::time::Instant,
}

impl Win {
    fn tree(&self) -> TreeUpdate {
        let mut root = Node::new(Role::Window);
        root.set_label(self.name);
        root.set_bounds(Rect { x0: 0.0, y0: 0.0, x1: W, y1: H });
        root.set_children(vec![NodeId(1), NodeId(2)]);
        let mut nodes = vec![(ROOT, root)];
        for i in 0..2u64 {
            let mut n = Node::new(Role::Button);
            n.set_label(if i == 0 {
                format!("{} button (clicked {})", self.name, self.clicks)
            } else {
                format!("{} other", self.name)
            });
            n.add_action(Action::Click);
            let y = 20.0 + i as f64 * ROW_H;
            n.set_bounds(Rect { x0: 20.0, y0: y, x1: W - 20.0, y1: y + ROW_H });
            nodes.push((NodeId(1 + i), n));
        }
        TreeUpdate { nodes, tree: Some(TreeInfo::new(ROOT)), tree_id: accesskit::TreeId::ROOT, focus: ROOT }
    }
}

impl Render for Win {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // like Pointman Next: tree sent on the next frame, with the window's focus state first
        if std::time::Instant::now() < self.tree_from {
            return div().size_full().bg(rgb(0x1e1e2e));
        }
        let tree = self.tree();
        let a11y = self.a11y.clone();
        window.on_next_frame(move |window, _cx| {
            a11y.set_window_focused(window.is_window_active());
            a11y.update(tree);
        });
        div().size_full().bg(rgb(0x1e1e2e)).text_color(rgb(0xffffff)).child(format!("{} ({} clicks)", self.name, self.clicks))
    }
}

fn open(cx: &mut App, name: &'static str, x: f32) {
    let bounds = Bounds::new(point(px(x), px(200.)), size(px(W as f32), px(H as f32)));
    cx.open_window(
        WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
        move |window, cx| {
            let (a11y, rx) = A11y::attach(window);
            // TWO_WINDOWS_LATE_TREE_MS=4000: window B sends its first tree this long after it opens
            let late = if name == "Window B" { std::env::var("TWO_WINDOWS_LATE_TREE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0) } else { 0 };
            let tree_from = std::time::Instant::now() + Duration::from_millis(late);
            let view = cx.new(|_| Win { name, a11y: std::rc::Rc::new(a11y), clicks: 0, tree_from });
            drain(view.downgrade(), rx, cx);
            if late > 0 {
                let weak = view.downgrade();
                cx.spawn(async move |cx| {
                    cx.background_executor().timer(Duration::from_millis(late + 100)).await;
                    weak.update(cx, |_, cx| cx.notify()).ok();
                })
                .detach();
            }
            view
        },
    )
    .unwrap();
}

fn main() {
    Application::new().run(|cx: &mut App| {
        open(cx, "Window A", 100.0);
        cx.activate(true);
        // TWO_WINDOWS_DELAY_MS=3000: open B later, once A is shown, focused and queried.
        match std::env::var("TWO_WINDOWS_DELAY_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            Some(0) => cx
                .spawn(async move |cx| {
                    cx.background_executor().timer(Duration::from_millis(3000)).await;
                    // open B from inside A's own update, as Pointman Next opens Settings
                    cx.update(|cx| {
                        let a = cx.windows()[0].downcast::<Win>().unwrap();
                        a.update(cx, |_w, window, cx| {
                            open(cx, "Window B", 500.0);
                            window.activate_window();
                        })
                        .ok();
                    })
                    .ok();
                })
                .detach(),
            Some(ms) => cx
                .spawn(async move |cx| {
                    cx.background_executor().timer(Duration::from_millis(ms)).await;
                    cx.update(|cx| {
                        open(cx, "Window B", 500.0);
                        cx.activate(true);
                    })
                    .ok();
                })
                .detach(),
            None => open(cx, "Window B", 500.0),
        }
    });
}

fn drain(view: WeakEntity<Win>, rx: Receiver<ActionRequest>, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_millis(30)).await;
            while let Ok(req) = rx.try_recv() {
                eprintln!("action: {:?} on {:?}", req.action, req.target_node);
                if view.update(cx, |w, cx| { w.clicks += 1; cx.notify(); }).is_err() {
                    return;
                }
            }
        }
    })
    .detach();
}
