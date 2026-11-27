//! Programmatic permission decisions for the CEF backend.
//!
//! Two things an embedding app can drive from outside the CEF callbacks:
//!
//! * an **auto-decision policy**: `(origin, permission) -> allow`, consulted
//!   before the persisted store and before any native dialog, so automation
//!   (tests, kiosk, the control API) gets a deterministic answer with no
//!   prompt on screen. Either side of the pair may be `"*"`.
//! * a **directory of pending requests**: every prompt/media request the
//!   permission handler registered is listed here with a stable id, so an
//!   already-showing prompt can be answered programmatically. Settling goes
//!   through the handler's own one-shot registry, so a decision can never
//!   race the dialog task into calling a CEF callback twice.
//!
//! All state is thread-local: CEF callbacks and `ui.call_sync` host functions
//! both run on the main thread, and the registries hold `Rc`s.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// What an auto-decision policy entry matches: origin and permission, either
/// possibly `"*"`.
type PolicyKey = (String, String);

#[derive(Default)]
struct AutoPolicy {
    entries: HashMap<PolicyKey, bool>,
}

thread_local! {
    static AUTO: RefCell<AutoPolicy> = RefCell::new(AutoPolicy::default());
    static PENDING: RefCell<HashMap<u64, PendingRecord>> = RefCell::new(HashMap::new());
    static NEXT_PENDING_ID: Cell<u64> = const { Cell::new(1) };
}

/// Sets an auto-decision: requests for `permission` from `origin` are answered
/// with `allow` without consulting the store or showing a dialog. Use `"*"`
/// on either side to match anything. An explicit programmatic policy wins
/// over persisted decisions, so automation stays deterministic.
pub fn set_auto_permission(origin: &str, permission: &str, allow: bool) {
    let key = (normalize_match(origin), normalize_match(permission));
    AUTO.with(|auto| {
        auto.borrow_mut().entries.insert(key, allow);
    });
}

/// Removes every auto-decision entry.
pub fn clear_auto_permissions() {
    AUTO.with(|auto| auto.borrow_mut().entries.clear());
}

/// The policy's decision for `(origin, permission)`, most specific key first:
/// exact, then `(origin, "*")`, then `("*", permission)`, then `("*", "*")`.
pub fn auto_decision(origin: &str, permission: &str) -> Option<bool> {
    let origin = normalize_match(origin);
    let permission = normalize_match(permission);
    AUTO.with(|auto| {
        let entries = &auto.borrow().entries;
        entries
            .get(&(origin.clone(), permission.clone()))
            .or_else(|| entries.get(&(origin.clone(), "*".to_string())))
            .or_else(|| entries.get(&("*".to_string(), permission.clone())))
            .or_else(|| entries.get(&("*".to_string(), "*".to_string())))
            .copied()
    })
}

/// Matching keys are case-insensitive and ignore a trailing slash, so
/// `HTTPS://Example.site/` and `https://example.site` are the same origin.
fn normalize_match(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    trimmed.to_lowercase()
}

/// A pending permission request as seen from outside the CEF callbacks.
#[derive(Clone, Debug)]
pub struct PendingPromptInfo {
    /// Stable id for [`decide_pending`].
    pub id: u64,
    /// `"prompt"` (on_show_permission_prompt) or `"media"` (media access).
    pub kind: String,
    /// The requesting origin, e.g. `https://example.site`.
    pub origin: String,
    /// The permission label(s), e.g. `notifications` or `camera, microphone`.
    pub permission: String,
    /// The browser the request came from, if known.
    pub browser_id: Option<i32>,
}

struct PendingRecord {
    info: PendingPromptInfo,
    /// Whether the handler's registry still holds the entry unsettled.
    is_pending: Box<dyn Fn() -> bool>,
    /// Settles the request with `allow`; false when no longer pending.
    settle: Box<dyn Fn(bool) -> bool>,
}

/// Registers a pending request in the directory and returns its id. The
/// closures bridge to the handler's own one-shot registry: `is_pending`
/// reports whether the entry is still unsettled, `settle(allow)` takes it
/// exactly once and delivers the decision to the CEF callback.
pub(crate) fn register_pending(
    mut info: PendingPromptInfo,
    is_pending: impl Fn() -> bool + 'static,
    settle: impl Fn(bool) -> bool + 'static,
) -> u64 {
    let id = NEXT_PENDING_ID.with(|next| {
        let id = next.get();
        next.set(id.wrapping_add(1).max(1));
        id
    });
    info.id = id;
    PENDING.with(|pending| {
        pending.borrow_mut().insert(
            id,
            PendingRecord {
                info,
                is_pending: Box::new(is_pending),
                settle: Box::new(settle),
            },
        );
    });
    id
}

/// The requests still waiting for a decision, pruning settled entries.
pub fn list_pending() -> Vec<PendingPromptInfo> {
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        pending.retain(|_, record| (record.is_pending)());
        pending.values().map(|record| record.info.clone()).collect()
    })
}

/// Answers a pending request programmatically. Returns false when the id is
/// unknown or the request was already settled (or its browser closed), so a
/// decision can never be delivered twice.
pub fn decide_pending(id: u64, allow: bool) -> bool {
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        let Some(record) = pending.get(&id) else {
            return false;
        };
        if !(record.is_pending)() {
            pending.remove(&id);
            return false;
        }
        let settled = (record.settle)(allow);
        if settled {
            pending.remove(&id);
        }
        settled
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_policy_specificity() {
        clear_auto_permissions();
        assert_eq!(auto_decision("https://a.site", "notifications"), None);
        set_auto_permission("*", "*", true);
        assert_eq!(auto_decision("https://a.site", "notifications"), Some(true));
        set_auto_permission("https://a.site", "*", false);
        assert_eq!(
            auto_decision("https://a.site", "notifications"),
            Some(false)
        );
        assert_eq!(auto_decision("https://b.site", "camera"), Some(true));
        set_auto_permission("https://a.site", "notifications", true);
        assert_eq!(auto_decision("https://a.site", "notifications"), Some(true));
        // Case and trailing slash do not matter.
        assert_eq!(
            auto_decision("HTTPS://A.site/", "notifications"),
            Some(true)
        );
        clear_auto_permissions();
        assert_eq!(auto_decision("https://a.site", "notifications"), None);
    }

    #[test]
    fn pending_directory_settles_once() {
        let state = std::rc::Rc::new(Cell::new(0u8)); // 0 pending, 1 allowed, 2 denied
        let state_is = state.clone();
        let state_settle = state.clone();
        let id = register_pending(
            PendingPromptInfo {
                id: 0,
                kind: "prompt".into(),
                origin: "https://a.site".into(),
                permission: "notifications".into(),
                browser_id: Some(1),
            },
            move || state_is.get() == 0,
            move |allow| {
                if state_settle.get() != 0 {
                    return false;
                }
                state_settle.set(if allow { 1 } else { 2 });
                true
            },
        );
        assert_eq!(list_pending().len(), 1);
        assert_eq!(list_pending()[0].id, id);
        assert!(decide_pending(id, true));
        assert_eq!(state.get(), 1);
        // Second decision on the same id is refused.
        assert!(!decide_pending(id, false));
        assert_eq!(state.get(), 1);
        assert!(list_pending().is_empty());
        assert!(!decide_pending(id + 999, true));
    }
}
