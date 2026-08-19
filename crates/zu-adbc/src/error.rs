//! A zu failure, as ADBC states one.
//!
//! ADBC carries five characters of SQLSTATE beside every error and a
//! GQLSTATUS is five characters, so the code goes across whole. Nothing
//! is mapped onto the nearest SQL condition and nothing is lost: a tool
//! that prints the SQLSTATE prints `42001` and a tool that looks it up
//! finds the same page the engine's own documentation has.
//!
//! [`Status`] is the coarse kind a driver manager and its callers switch
//! on, and it comes off the condition class, which is the first two
//! characters of the code and the thing the standard groups conditions
//! by. That is the same switch every zu client already makes to pick an
//! exception type, spelled once more in ADBC's vocabulary.

use std::os::raw::c_char;

use adbc_core::error::{Error, Status};
use zudb::ZuError;

/// What this crate gives back, which is ADBC's result and not zu's.
pub type Result<T> = std::result::Result<T, Error>;

/// The failure, with its code, its class and everything the record
/// carried that ADBC has no field for.
pub fn adbc(err: ZuError) -> Error {
    match err {
        ZuError::Gql(record) => {
            let status = record.status;
            let mut error = Error {
                message: record.detail.clone(),
                status: kind(status.class()),
                vendor_code: 0,
                sqlstate: sqlstate(status.code()),
                details: None,
            };
            let mut details = vec![
                ("zu.gqlstatus".to_string(), status.code().into()),
                ("zu.doc_url".to_string(), status.doc_url().into_bytes()),
                (
                    "zu.retryable".to_string(),
                    match record.retryable() {
                        true => b"true".to_vec(),
                        false => b"false".to_vec(),
                    },
                ),
            ];
            // The position is what points a caller at the token rather
            // than at the sentence, and an editor wants the numbers and
            // not the prose the message already has.
            if let Some(at) = record.position {
                details.push(("zu.line".to_string(), at.line.to_string().into_bytes()));
                details.push(("zu.column".to_string(), at.column.to_string().into_bytes()));
            }
            if let Some(excerpt) = &record.excerpt {
                details.push(("zu.excerpt".to_string(), excerpt.clone().into_bytes()));
            }
            error.details = Some(details);
            error
        }
        // The rest carry no condition code, so the SQLSTATE stays the
        // all-zero one the standard reserves for "none given" rather
        // than being invented here.
        ZuError::Io(_) => plain(err.to_string(), Status::IO),
        ZuError::Corrupt { .. } => plain(err.to_string(), Status::Internal),
        ZuError::Unsupported { .. } => plain(err.to_string(), Status::NotImplemented),
        ZuError::InvalidArgument(_) => plain(err.to_string(), Status::InvalidArguments),
        // Another writer holds what this one wanted, which is a
        // precondition that was not met rather than damage to anything.
        ZuError::Conflict(_) => plain(err.to_string(), Status::InvalidState),
        ZuError::Interrupted => plain(err.to_string(), Status::Cancelled),
    }
}

/// An error of this crate's own, for the things ADBC asks for that zu
/// has no answer to.
pub fn plain(message: impl Into<String>, status: Status) -> Error {
    Error::with_message_and_status(message, status)
}

/// The one every refusal in this driver uses, so that a caller asking
/// for something unbuilt is told what and not merely told no.
pub fn unbuilt(what: &str) -> Error {
    plain(
        format!("{what}, which this driver does not do yet"),
        Status::NotImplemented,
    )
}

/// The condition class as the kind ADBC groups failures into.
///
/// Everything the standard has a class for that zu can raise is here.
/// Anything else is [`Status::Internal`], which is the honest answer for
/// a code this driver was not taught about: the SQLSTATE beside it still
/// says exactly which condition it was.
fn kind(class: &str) -> Status {
    match class {
        // connection exception
        "08" => Status::IO,
        // data exception
        "22" => Status::InvalidData,
        // invalid transaction state, and invalid transaction termination
        "25" | "2D" => Status::InvalidState,
        // transaction rollback, which is the one worth trying again and
        // says so in `zu.retryable`
        "40" => Status::InvalidState,
        // syntax error or access rule violation
        "42" => Status::InvalidArguments,
        // dependent object error, and graph type violation
        "G1" | "G2" => Status::Integrity,
        _ => Status::Internal,
    }
}

/// The five characters, as the five bytes ADBC keeps them in.
///
/// A GQLSTATUS is five ASCII characters by construction, so this is a
/// copy and not a conversion. A code that somehow is not gets the
/// all-zero SQLSTATE rather than a truncated one, because half a code
/// is worse than none.
fn sqlstate(code: &str) -> [c_char; 5] {
    let mut out = [0; 5];
    let bytes = code.as_bytes();
    if bytes.len() == 5 && code.is_ascii() {
        for (slot, &byte) in out.iter_mut().zip(bytes) {
            *slot = byte as c_char;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use zudb::gqlstatus::codes;

    #[test]
    fn a_condition_keeps_its_code() {
        let err = adbc(ZuError::gql(codes::CG2000, "a graph type violation"));
        assert_eq!(err.status, Status::Integrity);
        assert_eq!(
            err.sqlstate.map(|c| c as u8),
            *b"G2000",
            "the GQLSTATUS goes across as the SQLSTATE, whole"
        );
        assert_eq!(err.message, "a graph type violation");
    }

    #[test]
    fn a_syntax_error_is_the_caller_s_fault() {
        let err = adbc(ZuError::gql(codes::C42001, "not a statement"));
        assert_eq!(err.status, Status::InvalidArguments);
    }

    #[test]
    fn an_interrupt_is_a_cancel() {
        let err = adbc(ZuError::Interrupted);
        assert_eq!(err.status, Status::Cancelled);
        assert_eq!(err.sqlstate, [0; 5], "no condition, so no code invented");
    }

    #[test]
    fn a_condition_carries_what_adbc_has_no_field_for() {
        let err = adbc(ZuError::gql(codes::C40000, "rolled back"));
        let details = err.details.expect("a condition carries details");
        let value = |key: &str| {
            details
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| String::from_utf8(value.clone()).expect("utf-8"))
        };
        assert_eq!(value("zu.gqlstatus").as_deref(), Some("40000"));
        assert_eq!(value("zu.retryable").as_deref(), Some("true"));
    }
}
