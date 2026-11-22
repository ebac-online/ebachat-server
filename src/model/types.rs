// Based on Mattermost server/public/model/utils.go (StringMap, StringArray, StringInterface),
// Copyright (c) 2015-present Mattermost, Inc. Licensed under AGPL-3.0.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::{PgArgumentBuffer, PgTypeInfo, PgValueRef};
use sqlx::types::Json;
use sqlx::{Decode, Encode, Postgres, Type};

/// `map[string]string` in a jsonb column.
pub type StringMap = Json<BTreeMap<String, String>>;
/// `map[string]any` in a jsonb column.
pub type StringInterface = Json<serde_json::Map<String, serde_json::Value>>;
/// `[]string` stored as JSON text in a varchar column (`Posts.FileIds`, `Posts.Filenames`).
pub type StringArray = JsonText<Vec<String>>;

/// A value stored as JSON in a text column, as Go's `driver.Valuer` on these types writes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JsonText<T>(pub T);

impl<T> Type<Postgres> for JsonText<T> {
    fn type_info() -> PgTypeInfo {
        <String as Type<Postgres>>::type_info()
    }
    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as Type<Postgres>>::compatible(ty)
    }
}

impl<T: Serialize> Encode<'_, Postgres> for JsonText<T> {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <String as Encode<Postgres>>::encode(serde_json::to_string(&self.0)?, buf)
    }
}

impl<'r, T: DeserializeOwned + Default> Decode<'r, Postgres> for JsonText<T> {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let s = <&str as Decode<Postgres>>::decode(value)?;
        // Go's StringArray.Scan of an empty string is an empty array.
        // ... and Go's `null` for a nil map or slice.
        Ok(JsonText(if s.is_empty() || s == "null" { T::default() } else { serde_json::from_str(s)? }))
    }
}

/// `Posts.FileIds`: the ids, and whether the list is `null`.
#[derive(Debug, Clone, Default)]
pub struct FileIds(pub Vec<String>, pub bool);

impl PartialEq for FileIds {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Serialize for FileIds {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.1 { s.serialize_none() } else { self.0.serialize(s) }
    }
}

impl<'de> Deserialize<'de> for FileIds {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Option::<Vec<String>>::deserialize(d)?;
        Ok(FileIds(v.clone().unwrap_or_default(), v.is_none()))
    }
}

impl Type<Postgres> for FileIds {
    fn type_info() -> PgTypeInfo {
        <String as Type<Postgres>>::type_info()
    }
    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as Type<Postgres>>::compatible(ty)
    }
}

impl Encode<'_, Postgres> for FileIds {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <String as Encode<Postgres>>::encode(serde_json::to_string(self)?, buf)
    }
}

impl<'r> Decode<'r, Postgres> for FileIds {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let s = <&str as Decode<Postgres>>::decode(value)?;
        Ok(if s.is_empty() { FileIds::default() } else { serde_json::from_str(s)? })
    }
}

/// A list stored space-separated in a text column (`Roles.Permissions`), a JSON array in the API.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Spaced(pub Vec<String>);

impl Type<Postgres> for Spaced {
    fn type_info() -> PgTypeInfo {
        <String as Type<Postgres>>::type_info()
    }
    fn compatible(ty: &PgTypeInfo) -> bool {
        <String as Type<Postgres>>::compatible(ty)
    }
}

impl Encode<'_, Postgres> for Spaced {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        <String as Encode<Postgres>>::encode(self.0.join(" "), buf)
    }
}

impl<'r> Decode<'r, Postgres> for Spaced {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        let s = <&str as Decode<Postgres>>::decode(value)?;
        Ok(Spaced(s.split_whitespace().map(str::to_string).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::FileIds;

    #[test]
    fn file_ids_keep_null() {
        let nil: FileIds = serde_json::from_str("null").unwrap();
        assert!(nil.1 && nil.0.is_empty());
        assert_eq!(serde_json::to_string(&nil).unwrap(), "null");
        assert_eq!(serde_json::to_string(&FileIds(vec![], false)).unwrap(), "[]");
        assert_eq!(serde_json::to_string(&FileIds(vec!["a".into()], false)).unwrap(), r#"["a"]"#);
    }
}
