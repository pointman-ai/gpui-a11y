//! A window with a list of 3 rows and a text field, with an accessibility tree.
//! Try: agent-desktop snapshot --app gpui-a11y-demo -i ; click a row ; set-value the field.
use accesskit::{Action, ActionData, ActionRequest, Node, NodeId, Rect, Role, TreeInfo, TreeUpdate};
use gpui::{prelude::*, *};
use gpui_a11y::A11y;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const ROOT: NodeId = NodeId(0);
const LIST: NodeId = NodeId(1);
const FIELD: NodeId = NodeId(10);
const W: f64 = 400.0;
const H: f64 = 300.0;
const ROW_H: f64 = 40.0;
const ROW_TOP: f64 = 20.0;
const FIELD_TOP: f64 = 180.0;
fn row_id(i: usize) -> NodeId {
    NodeId(2 + i as u64)
}

struct Demo {
    a11y: A11y,
    selected: usize,
    text: String,
    focus: FocusHandle,
}

impl Demo {
    fn tree(&self) -> TreeUpdate {
        let mut root = Node::new(Role::Window);
        root.set_label("gpui-a11y demo");
        root.set_bounds(Rect { x0: 0.0, y0: 0.0, x1: W, y1: H });
        root.set_children(vec![LIST, FIELD]);
        let mut list = Node::new(Role::List);
        list.set_label("Rows");
        list.set_bounds(Rect { x0: 20.0, y0: ROW_TOP, x1: W - 20.0, y1: ROW_TOP + 3.0 * ROW_H });
        list.set_children((0..3).map(row_id).collect::<Vec<_>>());
        let mut nodes = vec![(ROOT, root), (LIST, list)];
        for i in 0..3 {
            let mut n = Node::new(Role::ListItem);
            n.set_label(format!("Row {}", i + 1));
            n.set_selected(i == self.selected);
            n.add_action(Action::Click);
            let y = ROW_TOP + i as f64 * ROW_H;
            n.set_bounds(Rect { x0: 20.0, y0: y, x1: W - 20.0, y1: y + ROW_H });
            nodes.push((row_id(i), n));
        }
        let mut f = Node::new(Role::TextInput);
        f.set_label("Reply");
        f.set_value(self.text.clone());
        f.add_action(Action::SetValue);
        f.add_action(Action::Focus);
        f.set_bounds(Rect { x0: 20.0, y0: FIELD_TOP, x1: W - 20.0, y1: FIELD_TOP + 36.0 });
        nodes.push((FIELD, f));
        TreeUpdate {
            nodes,
            tree: Some(TreeInfo::new(ROOT)),
            tree_id: accesskit::TreeId::ROOT,
            focus: FIELD,
        }
    }

    fn handle(&mut self, req: ActionRequest) {
        eprintln!("action: {:?} on {:?}", req.action, req.target_node);
        match (req.action, req.target_node) {
            (Action::Click, id) if (2..5).contains(&id.0) => self.selected = (id.0 - 2) as usize,
            (Action::SetValue, FIELD) => {
                if let Some(ActionData::Value(v)) = req.data {
                    self.text = v.to_string();
                }
            }
            _ => {}
        }
    }
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.a11y.update(self.tree());
        let _ = cx;
        div()
            .size_full()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xffffff))
            .child(
                div().absolute().left(px(20.)).top(px(ROW_TOP as f32)).w(px((W - 40.0) as f32)).children(
                    (0..3).map(|i| {
                        div()
                            .h(px(ROW_H as f32))
                            .px_2()
                            .flex()
                            .items_center()
                            .bg(if i == self.selected { rgb(0x45475a) } else { rgb(0x313244) })
                            .child(format!("Row {}", i + 1))
                    }),
                ),
            )
            .child(
                div()
                    .track_focus(&self.focus)
                    .absolute()
                    .left(px(20.))
                    .top(px(FIELD_TOP as f32))
                    .w(px((W - 40.0) as f32))
                    .h(px(36.))
                    .px_2()
                    .flex()
                    .items_center()
                    .bg(rgb(0x11111b))
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _w, cx| {
                        if ev.keystroke.key == "backspace" {
                            this.text.pop();
                        } else if let Some(c) = &ev.keystroke.key_char {
                            this.text.push_str(c);
                        }
                        cx.notify();
                    }))
                    .child(self.text.clone()),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _e, w, _cx| {
                w.focus(&this.focus);
            }))
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let (tx, rx) = channel::<ActionRequest>();
        let bounds = Bounds::centered(None, size(px(W as f32), px(H as f32)), cx);
        cx.open_window(
            WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() },
            move |window, cx| {
                let a11y = A11y::attach(window, move |req| {
                    let _ = tx.send(req);
                });
                let view = cx.new(|cx| Demo { a11y, selected: 0, text: String::new(), focus: cx.focus_handle() });
                poll_actions(view.downgrade(), rx, cx);
                view
            },
        )
        .unwrap();
        cx.activate(true);
    });
}

fn poll_actions(view: WeakEntity<Demo>, rx: Receiver<ActionRequest>, cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_millis(30)).await;
            while let Ok(req) = rx.try_recv() {
                if view.update(cx, |d, cx| { d.handle(req); cx.notify(); }).is_err() {
                    return;
                }
            }
        }
    })
    .detach();
}
