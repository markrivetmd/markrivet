//! Versioned, portable evidence of exactly one guarded structural deletion.
//! Git object identity is checked by the caller; replay itself is pure.
use std::collections::BTreeSet;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Diagnostic, Document, ErrorCode, Selector,
    edit::{self, ListGuard, Op, Planner},
    git::Entry,
};

pub const MAX_RECEIPT: usize = 65_536;
/// Debug tests lower the canonical receipt bound without thousands of items.
fn max_receipt() -> usize {
    #[cfg(debug_assertions)]
    if let Some(limit) = std::env::var("RIVET_TEST_RECEIPT_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0 && *n < MAX_RECEIPT)
    {
        return limit;
    }
    MAX_RECEIPT
}
pub const MAX_ENCODED_RECEIPT: usize = (MAX_RECEIPT * 4).div_ceil(3);
pub const MAX_VERIFY_REQUEST: u64 = 100 * 1024 * 1024;
pub const MAX_FRAMING: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeleteIntent {
    Delete {
        select: Selector,
        guard: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        list: Option<ListGuard>,
        dropped: Vec<String>,
    },
}

impl DeleteIntent {
    pub fn from_ops(ops: &[Op], dropped: Vec<String>) -> Option<Self> {
        match ops {
            [
                Op::Delete {
                    select,
                    guard,
                    list,
                    ..
                },
            ] => Some(Self::Delete {
                select: select.clone(),
                guard: guard.clone(),
                list: list.clone(),
                dropped,
            }),
            _ => None,
        }
    }

    pub fn dropped(&self) -> &[String] {
        match self {
            Self::Delete { dropped, .. } => dropped,
        }
    }

    pub fn list(&self) -> Option<&ListGuard> {
        match self {
            Self::Delete { list, .. } => list.as_ref(),
        }
    }

    pub fn version(&self) -> u32 {
        if self.list().is_some() { 2 } else { 1 }
    }
}

/// Authoritative, complete candidate-only citation check for dropped list stays.
/// Replay and archive-copy deliberately do not call this live-project check.
pub fn check_list_citations(
    project: &crate::fs::Project,
    path: &str,
    base: &Document,
    candidate: &str,
    intent: &DeleteIntent,
    faults: &crate::fs::WalkFaults,
) -> Result<(), Diagnostic> {
    if intent.list().is_none() {
        return Ok(());
    }
    let DeleteIntent::Delete { select, .. } = intent;
    let crate::document::Target::Item { list, .. } = base.resolve(select)? else {
        return Err(Diagnostic::new(
            ErrorCode::InvalidRequest,
            "list authority requires an item target",
        ));
    };
    // An alias can belong to both the item and its list. It still identifies
    // the removed list, so subtracting item ids would lose citation authority.
    let ids: BTreeSet<_> = base
        .target_ids(crate::document::Target::Block(base.lists[list].block))
        .into_iter()
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let wrap = |e: Diagnostic| {
        let code = serde_json::to_string(&e.code).expect("error code");
        let mut refusal = Diagnostic::new(
            ErrorCode::ListCitationRefused,
            format!(
                "list citation scope incomplete ({code}): {}",
                e.clone().detail()
            ),
        );
        refusal.path = e.path;
        refusal.line = e.line;
        refusal.span = e.span;
        refusal
    };
    let config = crate::schema::load_config(project).map_err(&wrap)?;
    let globs: Vec<_> = config
        .live_patterns()
        .into_iter()
        .chain(config.archive_patterns())
        .collect();
    let (mut files, events, diagnostics) = crate::citations::scope(project, &config, faults);
    if let Some(e) = diagnostics.into_iter().next() {
        return Err(wrap(e));
    }
    for event in events {
        if let Some(e) = event.document_diagnostic(&globs) {
            return Err(wrap(e));
        }
    }
    if !files.iter().any(|(p, _)| p == path) {
        files.push((path.to_string(), false));
    }
    for (p, _) in files {
        let doc = if p == path {
            Document::parse(candidate)
        } else {
            crate::fs::fault("list_citation_read")
                .map_err(|e| Diagnostic::new(ErrorCode::Io, e.to_string()))
                .and_then(|()| project.read(&p))
                .and_then(|s| Document::parse(&s))
        }
        .map_err(|e| wrap(e.with_path(&p)))?;
        if let Some((id, at)) = crate::citations::mentions(&doc, &ids).into_iter().next() {
            let line = doc.file_line(at);
            return Err(Diagnostic::new(
                ErrorCode::ListCitationRefused,
                format!("dropped list stay {id} is cited in {p:?} at line {line}"),
            )
            .with_path(p)
            .with_line(line)
            .with_id(id));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteReceipt {
    pub version: u32,
    pub op: String,
    pub path: String,
    pub parent: String,
    pub base: Entry,
    pub candidate: Entry,
    pub read_sha256: String,
    pub candidate_sha256: String,
    pub operation: DeleteIntent,
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn invalid(message: impl Into<String>) -> Diagnostic {
    Diagnostic::new(ErrorCode::InvalidDeleteReceipt, message)
}

fn hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn require_canonical(path: &str) -> Result<(), Diagnostic> {
    if path.is_empty()
        || path.starts_with('/')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || crate::fs::canonical_path(path).ok().as_deref() != Some(path)
    {
        return Err(Diagnostic::new(
            ErrorCode::PathRejected,
            "path must be canonical and confined",
        ));
    }
    Ok(())
}

impl DeleteReceipt {
    pub fn bytes(&self) -> Result<Vec<u8>, Diagnostic> {
        let bytes = serde_json::to_vec(self).expect("receipt serialises");
        if bytes.len() > max_receipt() {
            return Err(Diagnostic::new(
                ErrorCode::DeleteReceiptTooLarge,
                format!("canonical delete receipt exceeds {} bytes", max_receipt()),
            ));
        }
        Ok(bytes)
    }

    pub fn validate(&self) -> Result<(), Diagnostic> {
        let DeleteIntent::Delete { select, .. } = &self.operation;
        select
            .require_not_position("receipt operation select")
            .map_err(|e| invalid(e.message))?;
        self.bytes()?;
        require_canonical(&self.path).map_err(|e| invalid(e.message))?;
        let oid_len = self.parent.len();
        if self.version != self.operation.version()
            || ![40, 64].contains(&oid_len)
            || !hex(&self.parent, oid_len)
            || !hex(&self.base.oid, oid_len)
            || !hex(&self.candidate.oid, oid_len)
            || !hex(&self.op, 16)
            || !hex(&self.read_sha256, 64)
            || !hex(&self.candidate_sha256, 64)
            || !["100644", "100755"].contains(&self.base.mode.as_str())
            || self.base.mode != self.candidate.mode
        {
            return Err(invalid("unsupported version or invalid receipt identity"));
        }
        let ids = self.operation.dropped();
        let unique: BTreeSet<_> = ids.iter().collect();
        if unique.len() != ids.len()
            || ids.iter().any(|id| {
                id.is_empty()
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        {
            return Err(invalid("invalid or duplicate dropped stay ids"));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<String, Diagnostic> {
        self.validate()?;
        Ok(URL_SAFE_NO_PAD.encode(self.bytes()?))
    }

    pub fn decode(payload: &str) -> Result<Self, Diagnostic> {
        if payload.len() > (max_receipt() * 4).div_ceil(3) {
            return Err(invalid("encoded receipt exceeds limit"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|e| invalid(format!("invalid base64url: {e}")))?;
        if bytes.len() > max_receipt() {
            return Err(invalid("decoded receipt exceeds limit"));
        }
        let receipt: Self = serde_json::from_slice(&bytes)
            .map_err(|e| invalid(format!("invalid receipt JSON: {e}")))?;
        receipt.validate()?;
        if receipt.bytes()? != bytes {
            return Err(invalid("receipt JSON is not canonical"));
        }
        Ok(receipt)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyRequest {
    pub parent: String,
    pub candidate: String,
    pub receipt: DeleteReceipt,
}

/// Parse before converting to Value: serde's struct visitors reject duplicate keys.
pub fn parse_request(input: &[u8]) -> Result<VerifyRequest, Diagnostic> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire<'a> {
        #[serde(borrow)]
        parent: &'a serde_json::value::RawValue,
        #[serde(borrow)]
        candidate: &'a serde_json::value::RawValue,
        #[serde(borrow)]
        receipt: &'a serde_json::value::RawValue,
    }
    if input.len() as u64 > MAX_VERIFY_REQUEST {
        return Err(invalid("request exceeds 100 MiB"));
    }
    let wire: Wire<'_> = serde_json::from_slice(input).map_err(|e| invalid(e.to_string()))?;
    // Receipt strings may use Unicode escapes in transport. Charge their
    // complete representation to the non-document budget, then enforce the
    // receipt limit on canonical decoded JSON, exactly as the writer does.
    let framing = input.len() - wire.parent.get().len() - wire.candidate.get().len();
    if framing > MAX_FRAMING {
        return Err(invalid("request framing exceeds 1 MiB"));
    }
    let parent = serde_json::from_str(wire.parent.get()).map_err(|e| invalid(e.to_string()))?;
    let candidate =
        serde_json::from_str(wire.candidate.get()).map_err(|e| invalid(e.to_string()))?;
    let receipt: DeleteReceipt =
        serde_json::from_str(wire.receipt.get()).map_err(|e| invalid(e.to_string()))?;
    receipt.bytes()?;
    Ok(VerifyRequest {
        parent,
        candidate,
        receipt,
    })
}

/// Value conversion otherwise discards duplicate fields before typed validation.
pub fn reject_duplicates(input: &[u8]) -> Result<(), Diagnostic> {
    use serde::de::{self, MapAccess, SeqAccess, Visitor};
    struct Unique;
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Check;
            impl<'de> Visitor<'de> for Check {
                type Value = Unique;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("JSON without duplicate fields")
                }
                fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Unique, M::Error> {
                    let mut keys = BTreeSet::new();
                    while let Some(key) = map.next_key::<String>()? {
                        if !keys.insert(key.clone()) {
                            return Err(de::Error::custom(format!("duplicate field {key:?}")));
                        }
                        map.next_value::<Unique>()?;
                    }
                    Ok(Unique)
                }
                fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Unique, S::Error> {
                    while seq.next_element::<Unique>()?.is_some() {}
                    Ok(Unique)
                }
                fn visit_bool<E: de::Error>(self, _: bool) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_i64<E: de::Error>(self, _: i64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_u64<E: de::Error>(self, _: u64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_f64<E: de::Error>(self, _: f64) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_str<E: de::Error>(self, _: &str) -> Result<Unique, E> {
                    Ok(Unique)
                }
                fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                    Ok(Unique)
                }
            }
            d.deserialize_any(Check)
        }
    }
    serde_json::from_slice::<Unique>(input)
        .map(|_| ())
        .map_err(|e| invalid(e.to_string()))
}

pub fn verify(request: &VerifyRequest) -> Result<Vec<String>, Diagnostic> {
    let r = &request.receipt;
    r.validate()?;
    for text in [&request.parent, &request.candidate] {
        if text.len() as u64 > crate::fs::MAX_FILE_BYTES {
            return Err(Diagnostic::new(
                ErrorCode::LimitExceeded,
                "decoded document exceeds 8 MiB",
            ));
        }
    }
    if hash(request.parent.as_bytes()) != r.read_sha256
        || hash(request.candidate.as_bytes()) != r.candidate_sha256
    {
        return Err(invalid("document byte hash mismatch"));
    }
    let doc = Document::parse(&request.parent)?;
    let DeleteIntent::Delete {
        select,
        guard,
        list,
        dropped,
    } = &r.operation;
    let change = Planner::new(&doc).structural(&Op::Delete {
        collection: None,
        select: select.clone(),
        guard: guard.clone(),
        list: list.clone(),
    })?;
    let result = edit::plan(&doc, vec![change])?;
    let actual: BTreeSet<_> = result.ops[0].dropped.iter().collect();
    let declared: BTreeSet<_> = dropped.iter().collect();
    if actual != declared {
        return Err(invalid("replayed dropped set differs from receipt"));
    }
    if result.candidate != request.candidate {
        return Err(invalid(
            "replayed candidate differs from every supplied candidate byte",
        ));
    }
    Ok(dropped.clone())
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_receipt_limit() {
        assert_eq!(super::MAX_RECEIPT, 65_536);
        assert_eq!(super::MAX_ENCODED_RECEIPT, 87_382);
    }
}
