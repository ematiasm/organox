//! HTTP transport helpers.

use serde::Deserialize;
use uuid::Uuid;

/// Distinguishes an omitted field from an explicit `null` in a PATCH body.
///
/// A plain `Option<T>` collapses both cases into `None`, so a PATCH could not
/// tell "leave this untouched" (field absent) from "clear this" (field `null`).
/// A field declared `Option<Option<T>>` with this deserializer keeps them
/// apart: an omitted field stays `None` and an explicit `null` becomes
/// `Some(None)`, so `null` clears and absence leaves untouched.
pub fn present_option<'de, D>(deserializer: D) -> Result<Option<Option<Uuid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Uuid>::deserialize(deserializer).map(Some)
}
