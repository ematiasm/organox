//! Customer input validation.
//!
//! The rules here exist to hand the operator a sentence they can act on. The
//! ones that cannot be broken without corrupting data (blank name, blank
//! document, unique document) are also enforced by the database: validation at
//! the boundary is not the last line of defense.

use rust_decimal::Decimal;

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

    #[error("the credit limit cannot be negative")]
    CreditLimitNegative,

    #[error("the credit limit {amount} carries more than two decimals")]
    CreditLimitTooManyDecimals { amount: Decimal },

    #[error("the credit limit cannot exceed {max}")]
    CreditLimitTooLarge { max: Decimal },
}

pub const MAX_NAME_LEN: usize = 200;
pub const MAX_TAX_ID_LEN: usize = 32;
/// The largest value `numeric(14, 2)` can hold. A well-formed body above it
/// would pass sign and scale and then fail inside Postgres as an internal
/// error, so the boundary refuses it with a sentence the operator can act on.
pub fn max_credit_limit() -> Decimal {
    crate::money::MAX_AMOUNT
}
/// The longest address the email standard allows.
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

/// An empty document becomes `None`: "" and NULL must not be two ways of saying
/// "no document", or the partial unique index treats them differently.
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

/// Deliberately minimal: text, one `@`, text. A stricter grammar rejects
/// addresses that are valid, and only the mailbox owner can say whether one
/// exists. The server never claims an address is reachable.
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

/// No format is imposed: phone formats differ per country and per habit, and a
/// rejected valid number blocks a sale. Only length is bounded.
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

/// A credit limit is money the shop is willing to be owed, so it follows the
/// two rules every money amount in this tree follows: never negative, and no
/// finer than cents. `numeric(14, 2)` would silently round a third decimal,
/// and a limit that rounds is a limit the operator never set. `0` is a valid
/// limit — cash only — so only the sign, the scale and the column's magnitude
/// are checked here.
pub fn validate_credit_limit(raw: Decimal) -> Result<Decimal, ValidationError> {
    if raw < Decimal::ZERO {
        return Err(ValidationError::CreditLimitNegative);
    }
    if raw.round_dp(2) != raw {
        return Err(ValidationError::CreditLimitTooManyDecimals { amount: raw });
    }
    if raw > max_credit_limit() {
        return Err(ValidationError::CreditLimitTooLarge {
            max: max_credit_limit(),
        });
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn name_is_trimmed_and_blank_rejected() {
        assert_eq!(validate_name("  Ana Pérez  ").unwrap(), "Ana Pérez");
        assert_eq!(
            validate_name("   ").unwrap_err(),
            ValidationError::NameBlank
        );
        assert_eq!(validate_name("").unwrap_err(), ValidationError::NameBlank);
    }

    #[test]
    fn an_empty_document_becomes_none() {
        assert_eq!(validate_tax_id(Some("   ")).unwrap(), None);
        assert_eq!(validate_tax_id(None).unwrap(), None);
        assert_eq!(
            validate_tax_id(Some(" 20-12345678-9 ")).unwrap(),
            Some("20-12345678-9".into())
        );
    }

    #[test]
    fn email_requires_one_at_with_text_on_both_sides() {
        assert_eq!(
            validate_email(Some(" ana@example.com ")).unwrap(),
            Some("ana@example.com".into())
        );
        assert_eq!(validate_email(Some("")).unwrap(), None);
        assert_eq!(
            validate_email(Some("ana.example.com")).unwrap_err(),
            ValidationError::EmailInvalid
        );
        assert_eq!(
            validate_email(Some("@example.com")).unwrap_err(),
            ValidationError::EmailInvalid
        );
        assert_eq!(
            validate_email(Some("ana@")).unwrap_err(),
            ValidationError::EmailInvalid
        );
        assert_eq!(
            validate_email(Some("ana@one@two")).unwrap_err(),
            ValidationError::EmailInvalid
        );
    }

    #[test]
    fn phone_keeps_whatever_format_was_typed() {
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

    #[test]
    fn a_credit_limit_is_never_negative_nor_finer_than_cents() {
        assert_eq!(validate_credit_limit(dec!(100.00)).unwrap(), dec!(100.00));
        // Zero is a limit — cash only — not the absence of one.
        assert_eq!(validate_credit_limit(Decimal::ZERO).unwrap(), Decimal::ZERO);
        assert_eq!(
            validate_credit_limit(dec!(-1.00)).unwrap_err(),
            ValidationError::CreditLimitNegative
        );
        assert_eq!(
            validate_credit_limit(dec!(10.005)).unwrap_err(),
            ValidationError::CreditLimitTooManyDecimals {
                amount: dec!(10.005)
            }
        );
    }

    /// A limit that fits the JSON contract but not the column would surface as
    /// an internal database error instead of a sentence the operator can act
    /// on, so the boundary owns the column's magnitude too.
    #[test]
    fn a_credit_limit_cannot_exceed_the_column() {
        assert!(validate_credit_limit(max_credit_limit()).is_ok());
        assert_eq!(
            validate_credit_limit(dec!(1000000000000.00)).unwrap_err(),
            ValidationError::CreditLimitTooLarge {
                max: max_credit_limit()
            }
        );
    }
}
