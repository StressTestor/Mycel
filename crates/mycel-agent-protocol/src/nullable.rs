use serde::{Deserialize, Serialize};

/// A JSON property that distinguishes omission from an explicit `null`.
/// Use `#[serde(default, skip_serializing_if = "OptionalNullable::is_missing")]`
/// on containing fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum OptionalNullable<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl<T> OptionalNullable<T> {
    pub const fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    pub fn as_ref(&self) -> OptionalNullable<&T> {
        match self {
            Self::Missing => OptionalNullable::Missing,
            Self::Null => OptionalNullable::Null,
            Self::Value(value) => OptionalNullable::Value(value),
        }
    }

    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Missing | Self::Null => None,
        }
    }
}

impl<T> Serialize for OptionalNullable<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Missing | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T> Deserialize<'de> for OptionalNullable<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(|value| match value {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}
