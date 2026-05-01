//! Cross-profile slot carry.
//!
//! When the user switches from profile A to profile B and a slot has the same
//! action UUID + same settings in both profiles, we don't fire willDisappear
//! on A's instance and willAppear on B's slot. Plugin state (graphs, counters,
//! cached values) survives the switch. The plugin keeps using A's context
//! string for its instance — events from B's slot get redirected to A's
//! instance via this module.
//!
//! Only the active profile lookups go through `anchor_or_self`. Disk JSON and
//! UI/edit operations (events/frontend/instances.rs) keep using native lookups
//! so the user's edits land in the profile they're editing, not the anchor.

use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::sync::RwLock;

/// Map: "device\u{1}target_profile\u{1}controller\u{1}position" → anchor_profile.
/// When `target_profile` is the active profile and we look up the slot at
/// (controller, position), we instead look up that slot in `anchor_profile`.
static CARRIES: LazyLock<RwLock<HashMap<String, String>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

fn key(device: &str, profile: &str, controller: &str, position: u8) -> String {
	format!("{}\u{1}{}\u{1}{}\u{1}{}", device, profile, controller, position)
}

/// Returns the anchor profile for a slot if a carry is installed; None means
/// the slot has no carry — look it up natively.
pub async fn anchor_for(device: &str, profile: &str, controller: &str, position: u8) -> Option<String> {
	CARRIES.read().await.get(&key(device, profile, controller, position)).cloned()
}

/// Anchor profile if carried, else the profile itself. Use this at dispatch
/// sites where you've built a context from the device's active profile and
/// need to know which profile's slot to actually read.
pub async fn anchor_or_self(device: &str, profile: &str, controller: &str, position: u8) -> String {
	anchor_for(device, profile, controller, position).await.unwrap_or_else(|| profile.to_owned())
}

pub async fn install(device: &str, target_profile: &str, controller: &str, position: u8, anchor_profile: &str) {
	CARRIES.write().await.insert(key(device, target_profile, controller, position), anchor_profile.to_owned());
}

pub async fn break_for(device: &str, profile: &str, controller: &str, position: u8) {
	CARRIES.write().await.remove(&key(device, profile, controller, position));
}

/// Drop every carry whose target_profile is `profile`. Called when leaving a
/// profile so the dormant carries don't leak.
pub async fn break_all_for(device: &str, profile: &str) {
	let prefix = format!("{}\u{1}{}\u{1}", device, profile);
	let mut map = CARRIES.write().await;
	map.retain(|k, _| !k.starts_with(&prefix));
}

/// Returns true if the given (active_profile, controller, position) is a
/// carry slot whose anchor matches `candidate_anchor`. Used to decide
/// "should this device-update push, even though context.profile != active?".
pub async fn is_anchor_for_active(device: &str, active: &str, controller: &str, position: u8, candidate_anchor: &str) -> bool {
	anchor_for(device, active, controller, position).await.as_deref() == Some(candidate_anchor)
}
