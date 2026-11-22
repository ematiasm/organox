//! Supplier input validation.
//!
//! Mirrors `customers::validation` so both party registries share identical
//! field bounds and actionable English error messages.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("the name cannot be blank")]
    NameBlank,

    #[error("the name exceeds {max} characters")]
    NameTooLong { max: usize },

    #[error("the document exceeds {max} characters")]
    TaxIdTooLong { max: usize },

    #[error("the email is not valid")]
    EmailInvalid,

    #[error("the email exceeds {max} characters")]
    EmailTooLong { max: usize },

    #[error("the phone exceeds {max} characters")]
    PhoneTooLong { max: usize },
}

pub const MAX_NAME_LEN: usize = 200;
pub const MAX_TAX_ID_LEN: usize = 32;
pub const MAX_EMAIL_LEN: usize = 254;
pub const MAX_PHONE_LEN: usize = 40;

pub fn validate_name(raw: &str) -> Result<String, ValidationError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ValidationError::NameBlank);
    }
    if trimmed.chars().count() > MAX_NAME_LEN {
        return Err(ValidationError::NameTooLong { max: MAX_NAME_LEN });
    }
    Ok(trimmed.to_string())
}

/// Blank documents become `None` so `""` and `NULL` never diverge under the
/// partial unique index.
pub fn validate_tax_id(raw: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_TAX_ID_LEN {
        return Err(ValidationError::TaxIdTooLong {
            max: MAX_TAX_ID_LEN,
        });
    }
    Ok(Some(trimmed.to_string()))
}

pub fn validate_email(raw: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_EMAIL_LEN {
        return Err(ValidationError::EmailTooLong { max: MAX_EMAIL_LEN });
    }

    let Some((local, domain)) = trimmed.split_once('@') else {
        return Err(ValidationError::EmailInvalid);
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(ValidationError::EmailInvalid);
    }

    Ok(Some(trimmed.to_string()))
}

pub fn validate_phone(raw: Option<&str>) -> Result<Option<String>, ValidationError> {
    let Some(raw) = raw else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.chars().count() > MAX_PHONE_LEN {
        return Err(ValidationError::PhoneTooLong { max: MAX_PHONE_LEN });
    }
    Ok(Some(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_trimmed_and_blank_rejected() {
        assert_eq!(validate_name("  Molino Sur  ").unwrap(), "Molino Sur");
        assert_eq!(
            validate_name("   ").unwrap_err(),
            ValidationError::NameBlank
        );
        assert_eq!(validate_name("").unwrap_err(), ValidationError::NameBlank);
    }

    #[test]
    fn blank_document_becomes_none() {
        assert_eq!(validate_tax_id(Some("   ")).unwrap(), None);
        assert_eq!(validate_tax_id(None).unwrap(), None);
        assert_eq!(
            validate_tax_id(Some(" 30-12345678-9 ")).unwrap(),
            Some("30-12345678-9".into())
        );
    }

    #[test]
    fn email_requires_one_at_with_text_on_both_sides() {
        assert_eq!(
            validate_email(Some(" sales@vendor.test ")).unwrap(),
            Some("sales@vendor.test".into())
        );
        assert_eq!(validate_email(Some("")).unwrap(), None);
        assert_eq!(
            validate_email(Some("vendor.test")).unwrap_err(),
            ValidationError::EmailInvalid
        );
        assert_eq!(
            validate_email(Some("@vendor.test")).unwrap_err(),
            ValidationError::EmailInvalid
        );
        assert_eq!(
            validate_email(Some("a@b@c")).unwrap_err(),
            ValidationError::EmailInvalid
        );
    }

    #[test]
    fn phone_bounds_length_without_imposing_format() {
        assert_eq!(
            validate_phone(Some(" +54 9 11 5555-0100 ")).unwrap(),
            Some("+54 9 11 5555-0100".into())
        );
        assert_eq!(validate_phone(Some("  ")).unwrap(), None);
        let long = "1".repeat(MAX_PHONE_LEN + 1);
        assert_eq!(
            validate_phone(Some(&long)).unwrap_err(),
            ValidationError::PhoneTooLong { max: MAX_PHONE_LEN }
        );
    }
}
