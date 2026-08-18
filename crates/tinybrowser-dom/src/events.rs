use std::rc::Rc;

use crate::tree::{DomTree, NodeId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventListenerHandle(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventPhase {
    None,
    Capturing,
    AtTarget,
    Bubbling,
}

#[derive(Debug)]
pub struct Event {
    type_: String,
    bubbles: bool,
    cancelable: bool,
    target: Option<NodeId>,
    current_target: Option<NodeId>,
    event_phase: EventPhase,
    stop_propagation: bool,
    stop_immediate: bool,
    default_prevented: bool,
}

impl Event {
    pub fn new(type_: impl Into<String>, bubbles: bool, cancelable: bool) -> Self {
        Event {
            type_: type_.into(),
            bubbles,
            cancelable,
            target: None,
            current_target: None,
            event_phase: EventPhase::None,
            stop_propagation: false,
            stop_immediate: false,
            default_prevented: false,
        }
    }

    pub fn type_(&self) -> &str {
        &self.type_
    }

    pub fn bubbles(&self) -> bool {
        self.bubbles
    }

    pub fn cancelable(&self) -> bool {
        self.cancelable
    }

    pub fn target(&self) -> Option<NodeId> {
        self.target
    }

    pub fn current_target(&self) -> Option<NodeId> {
        self.current_target
    }

    pub fn event_phase(&self) -> EventPhase {
        self.event_phase
    }

    pub fn default_prevented(&self) -> bool {
        self.default_prevented
    }

    pub fn prevent_default(&mut self) {
        if self.cancelable {
            self.default_prevented = true;
        }
    }

    pub fn stop_propagation(&mut self) {
        self.stop_propagation = true;
    }

    pub fn stop_immediate_propagation(&mut self) {
        self.stop_propagation = true;
        self.stop_immediate = true;
    }
}

pub(crate) struct NativeListener {
    handle: EventListenerHandle,
    type_: String,
    capture: bool,
    once: bool,
    callback: Rc<dyn Fn(&mut Event)>,
}

impl DomTree {
    pub fn add_event_listener(
        &self,
        target: NodeId,
        type_: &str,
        capture: bool,
        once: bool,
        callback: impl Fn(&mut Event) + 'static,
    ) -> Option<EventListenerHandle> {
        let mut inner = self.inner.borrow_mut();
        if !inner.contains(target) {
            return None;
        }
        let handle = EventListenerHandle(inner.next_listener_id);
        inner.next_listener_id = inner.next_listener_id.saturating_add(1);
        inner
            .listeners
            .entry(target)
            .or_default()
            .push(NativeListener {
                handle,
                type_: type_.to_string(),
                capture,
                once,
                callback: Rc::new(callback),
            });
        Some(handle)
    }

    pub fn remove_event_listener(&self, target: NodeId, handle: EventListenerHandle) {
        let mut inner = self.inner.borrow_mut();
        if let Some(list) = inner.listeners.get_mut(&target) {
            list.retain(|listener| listener.handle != handle);
            if list.is_empty() {
                inner.listeners.remove(&target);
            }
        }
    }

    /// Dispatch `event` at `target`. Returns false when `preventDefault` ran.
    pub fn dispatch_event(&self, target: NodeId, event: &mut Event) -> bool {
        if self.get_node(target).is_none() {
            return true;
        }
        event.target = Some(target);
        event.stop_propagation = false;
        event.stop_immediate = false;
        event.default_prevented = false;

        let mut path = Vec::new();
        let mut current = Some(target);
        while let Some(node) = current {
            path.push(node);
            current = self.with_node(node, |n| n.parent).flatten();
        }
        path.reverse();

        event.event_phase = EventPhase::Capturing;
        for &node in path.iter().filter(|id| **id != target) {
            self.invoke_listeners(node, event, Some(true));
            if event.stop_propagation {
                event.event_phase = EventPhase::None;
                event.current_target = None;
                return !event.default_prevented;
            }
        }

        event.event_phase = EventPhase::AtTarget;
        self.invoke_listeners(target, event, None);
        if event.stop_propagation || !event.bubbles {
            event.event_phase = EventPhase::None;
            event.current_target = None;
            return !event.default_prevented;
        }

        event.event_phase = EventPhase::Bubbling;
        for &node in path.iter().rev().filter(|id| **id != target) {
            self.invoke_listeners(node, event, Some(false));
            if event.stop_propagation {
                break;
            }
        }

        event.event_phase = EventPhase::None;
        event.current_target = None;
        !event.default_prevented
    }

    fn invoke_listeners(&self, node: NodeId, event: &mut Event, capture_only: Option<bool>) {
        let snapshot: Vec<(EventListenerHandle, bool, Rc<dyn Fn(&mut Event)>)> = {
            let inner = self.inner.borrow();
            inner
                .listeners
                .get(&node)
                .into_iter()
                .flatten()
                .filter(|listener| listener.type_ == event.type_)
                .filter(|listener| capture_only.is_none_or(|capture| listener.capture == capture))
                .map(|listener| {
                    (
                        listener.handle,
                        listener.once,
                        Rc::clone(&listener.callback),
                    )
                })
                .collect()
        };

        event.current_target = Some(node);
        for (handle, once, callback) in snapshot {
            callback(event);
            if once {
                self.remove_event_listener(node, handle);
            }
            if event.stop_immediate {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{NodeData, NodeId};
    use html5ever::{LocalName, QualName};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn element(tree: &DomTree, local: &str) -> NodeId {
        tree.new_node(NodeData::Element {
            name: QualName::new(None, ns!(html), LocalName::from(local)),
            attrs: vec![],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
        })
    }

    #[test]
    fn dispatch_walks_capture_then_target_then_bubble() {
        let tree = DomTree::new();
        let doc = tree.document();
        let parent = element(&tree, "div");
        let child = element(&tree, "span");
        tree.append_child(doc, parent);
        tree.append_child(parent, child);

        let log = Rc::new(RefCell::new(Vec::<String>::new()));
        let push = |label: &'static str, log: Rc<RefCell<Vec<String>>>| {
            move |event: &mut Event| {
                log.borrow_mut()
                    .push(format!("{label}:{:?}", event.event_phase()));
            }
        };

        tree.add_event_listener(
            parent,
            "click",
            true,
            false,
            push("parent-cap", Rc::clone(&log)),
        );
        tree.add_event_listener(
            parent,
            "click",
            false,
            false,
            push("parent-bub", Rc::clone(&log)),
        );
        tree.add_event_listener(child, "click", false, false, push("child", Rc::clone(&log)));

        let mut event = Event::new("click", true, true);
        assert!(tree.dispatch_event(child, &mut event));
        assert_eq!(
            *log.borrow(),
            vec![
                "parent-cap:Capturing".to_string(),
                "child:AtTarget".to_string(),
                "parent-bub:Bubbling".to_string(),
            ]
        );
    }

    #[test]
    fn once_listener_runs_only_once() {
        let tree = DomTree::new();
        let doc = tree.document();
        let node = element(&tree, "button");
        tree.append_child(doc, node);

        let count = Rc::new(RefCell::new(0u32));
        let count_cb = Rc::clone(&count);
        tree.add_event_listener(node, "click", false, true, move |_| {
            *count_cb.borrow_mut() += 1;
        });

        let mut event = Event::new("click", true, true);
        tree.dispatch_event(node, &mut event);
        tree.dispatch_event(node, &mut event);
        assert_eq!(*count.borrow(), 1);
    }

    #[test]
    fn stop_propagation_skips_bubble_listeners() {
        let tree = DomTree::new();
        let doc = tree.document();
        let parent = element(&tree, "div");
        let child = element(&tree, "span");
        tree.append_child(doc, parent);
        tree.append_child(parent, child);

        let saw_parent = Rc::new(RefCell::new(false));
        let saw_parent_cb = Rc::clone(&saw_parent);
        tree.add_event_listener(child, "click", false, false, |event| {
            event.stop_propagation();
        });
        tree.add_event_listener(parent, "click", false, false, move |_| {
            *saw_parent_cb.borrow_mut() = true;
        });

        let mut event = Event::new("click", true, true);
        tree.dispatch_event(child, &mut event);
        assert!(!*saw_parent.borrow());
    }

    #[test]
    fn prevent_default_is_reported_by_dispatch() {
        let tree = DomTree::new();
        let node = element(&tree, "a");
        tree.add_event_listener(node, "click", false, false, |event| {
            event.prevent_default();
        });
        let mut event = Event::new("click", true, true);
        assert!(!tree.dispatch_event(node, &mut event));
        assert!(event.default_prevented());
    }

    #[test]
    fn listeners_die_with_removed_nodes() {
        let tree = DomTree::new();
        let doc = tree.document();
        let node = element(&tree, "div");
        tree.append_child(doc, node);
        let count = Rc::new(RefCell::new(0u32));
        let count_cb = Rc::clone(&count);
        tree.add_event_listener(node, "click", false, false, move |_| {
            *count_cb.borrow_mut() += 1;
        });
        tree.remove(node);
        let mut event = Event::new("click", true, true);
        tree.dispatch_event(node, &mut event);
        assert_eq!(*count.borrow(), 0);
    }
}
