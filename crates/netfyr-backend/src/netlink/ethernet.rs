//! Compatibility re-export: delegates to `interface::query_interfaces`.
//!
//! Existing integration tests import `ethernet::query_ethernet`. This shim
//! preserves that API while the canonical implementation lives in `interface.rs`.

use netfyr_state::{entity_types::ETHERNET, Selector, StateSet};
use rtnetlink::Handle;

use crate::BackendError;

pub async fn query_ethernet(
    handle: &Handle,
    selector: Option<&Selector>,
) -> Result<StateSet, BackendError> {
    super::interface::query_interfaces(handle, Some(ETHERNET), selector).await
}
