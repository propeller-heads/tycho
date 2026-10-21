use diesel::{
    deserialize::{self, FromSql, FromSqlRow},
    pg::{Pg, PgValue},
};

/// A JSONB value kept as unparsed JSON text.
#[derive(Debug, Clone, PartialEq, Eq, FromSqlRow)]
pub(crate) struct RawJson(Box<str>);

impl RawJson {
    pub(crate) fn from_jsonb_bytes(bytes: &[u8]) -> Result<Self, String> {
        let (version, text) = bytes
            .split_first()
            .ok_or_else(|| "empty jsonb value".to_string())?;
        if *version != 1 {
            return Err(format!("unsupported jsonb version {version}"));
        }
        let text =
            std::str::from_utf8(text).map_err(|e| format!("jsonb is not valid UTF-8: {e}"))?;
        Ok(Self(text.into()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromSql<diesel::sql_types::Jsonb, Pg> for RawJson {
    fn from_sql(value: PgValue<'_>) -> deserialize::Result<Self> {
        Self::from_jsonb_bytes(value.as_bytes()).map_err(Into::into)
    }
}

#[cfg(test)]
impl From<&str> for RawJson {
    fn from(text: &str) -> Self {
        Self(text.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_version_1_text() {
        let raw = RawJson::from_jsonb_bytes(b"\x01{\"a\":1}").unwrap();
        assert_eq!(raw.as_str(), "{\"a\":1}");
    }

    #[test]
    fn rejects_other_versions() {
        let err = RawJson::from_jsonb_bytes(b"\x02{}").unwrap_err();
        assert_eq!(err, "unsupported jsonb version 2");
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(RawJson::from_jsonb_bytes(b"").unwrap_err(), "empty jsonb value");
    }

    #[test]
    fn rejects_invalid_utf8() {
        let err = RawJson::from_jsonb_bytes(b"\x01\xff\xfe").unwrap_err();
        assert!(err.starts_with("jsonb is not valid UTF-8"), "{err}");
    }
}
