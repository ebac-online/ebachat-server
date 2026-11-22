//! API structs and table rows. Go's JSON rules: zero values for missing or `null` fields, `omitempty`, map keys
//! sorted (`BTreeMap`, and `serde_json`'s default sorted `Map`).

pub mod channel;
mod misc;
pub mod post;
pub mod session;
pub mod status;
mod team;
mod types;
mod user;

pub use channel::{Channel, ChannelMember};
pub use misc::{
    Base64, Bot, Draft, Emoji, FileInfo, Preference, Reaction, Role, Scheme, SidebarCategory, Thread, ThreadMembership,
    category,
};
pub use post::{
    Post, PostAcknowledgement, PostList, PostMetadata, PostPriority, ThreadResponse, Threads, kind as post_kind,
};
pub use session::Session;
pub use status::Status;
pub use team::{Team, TeamMember};
pub use types::{FileIds, JsonText, Spaced, StringArray, StringInterface, StringMap};
pub use user::{Show, User};

use serde::{Deserialize, Deserializer};

/// Go's `omitempty`.
pub fn is_zero<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// Go's decoder: `null` leaves the zero value.
pub fn null_default<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(d: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}
