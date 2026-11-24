use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Half-open UTF-8 byte extent into the original file bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn slice(self, source: &str) -> Option<&str> {
        source.get(self.start..self.end)
    }

    pub fn len(self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn contains(self, pos: usize) -> bool {
        self.start <= pos && pos < self.end
    }

    /// True when the extents share a byte, or an insertion point lies strictly inside.
    pub fn overlaps(self, other: Span) -> bool {
        match (self.is_empty(), other.is_empty()) {
            (true, true) => self.start == other.start,
            (true, false) => other.start < self.start && self.start < other.end,
            (false, true) => self.start < other.start && other.start < self.end,
            (false, false) => self.start < other.end && other.start < self.end,
        }
    }
}

/// Selects one addressable unit. Stay ids share one namespace per document
/// (markstay §5.5): a heading's stay selects its section, a list item's child
/// stay selects the item, and any other stay selects its top-level block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Selector {
    Id {
        id: String,
    },
    /// Consecutive heading titles ending at the target section. A suffix of
    /// the full path is accepted; more than one match is an error.
    HeadingPath {
        path: Vec<String>,
    },
    FrontmatterKey {
        key: String,
    },
    /// A list item by place: the `index`-th (zero-based) direct item of the
    /// top-level lists in a section's own body, counted in document order
    /// as one sequence. `heading_path` matches as [`Selector::HeadingPath`]
    /// does; absent or `null` names the preamble before the first heading.
    /// Resolution refuses an item whose bytes repeat anywhere in the document.
    Position {
        #[serde(default)]
        heading_path: Option<Vec<String>>,
        index: u64,
    },
    Document,
    /// Stamp scope: every section, and list items only inside the sections a
    /// bound collection holds. Needs a project, so it never resolves through
    /// [`crate::Document`] alone.
    Collections,
}

impl Selector {
    /// A position is accepted only by `get`, `set` and `update-item`. Every
    /// other selector-typed field calls this first, before any other check.
    pub fn require_not_position(&self, field: &str) -> Result<(), Diagnostic> {
        match self {
            Selector::Position { .. } => Err(Diagnostic::new(
                ErrorCode::InvalidRequest,
                format!(
                    "{field}: a position selector is accepted only by get, set and update-item; stamp the file to address the item by id"
                ),
            )),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionPart {
    Title,
    Body,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    Start,
    End,
}

/// Guard scope for typed item writes. `field` is reserved: a field guard needs
/// schema, binding and structural dependencies plus whole-item invariant
/// checks under the writer lock, and none of that exists yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardScope {
    #[default]
    Action,
    Field,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    InvalidDeleteReceipt,
    DeleteReceiptTooLarge,
    /// A sole list's trailer includes a marker the item archive cannot preserve.
    ListTrailerRefused,
    /// A dropped list is cited, or its complete citation scope cannot be checked.
    ListCitationRefused,
    ArchiveCopyRefused,
    MoveRefused,
    PolicyConflict,
    PathRejected,
    NotFound,
    UnsupportedSyntax,
    InvalidContent,
    MissingSelector,
    AmbiguousSelector,
    /// No exact source occurrence in the selected section's own body.
    MatchNotFound,
    /// More than one exact source occurrence, including overlapping ones.
    AmbiguousMatch,
    DuplicateId,
    GuardConflict,
    /// A delete target is outside the queue named by its collection precondition.
    CollectionConflict,
    InvalidSchema,
    InvalidItem,
    StampRefused,
    StaleCursor,
    LockTimeout,
    ExternalChange,
    /// An Emacs buffer holds unsaved changes to the file (its `.#<name>`
    /// lock is live), or a lock beside it cannot be read.
    EditorLock,
    Io,
    CommitUncertain,
    LimitExceeded,
    OrphanMarker,
    UnaddressedMarker,
    MalformedMarker,
    /// The document or frontmatter key a write would create is already there.
    AlreadyExists,
    /// Commit mode: the path has a pending record recovery could not settle.
    PendingUnresolved,
    /// Commit mode, `on_dirty: refuse`: the file is not known clean.
    CommitRefused,
    /// `commit-pending` would not remove the named git lock.
    ClearLockRefused,
    /// `query` with `ready`: a wait that names no single task item, or a loop.
    UnresolvedWait,
    /// Git history cannot be inspected under the supported repository policy.
    HistoryUnavailable,
    /// A history request cannot fit its bounded response envelope.
    HistoryResourceLimit,
    HistoryRevisionMissing,
    HistoryRevisionUnreachable,
    HistoryGap,
    HistoryIdentityAmbiguous,
    HistoryTargetAbsent,
    HistoryKindChanged,
    HistoryLocationChanged,
    HistoryLocationUnsupported,
    HistoryLineageBoundary,
    HistoryChanged,
    HistoryNoPreviousVersion,
    /// A defect in Rivet itself; the message says whether a write may have landed.
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveReason {
    SourceNotLive,
    SourceArchive,
    DestinationNotLive,
    DestinationArchive,
    NotNative,
    SoleItem,
    ReferenceDefinition,
    DestinationCarriesId,
    ArchivedId,
    NewlineMismatch,
    ListShape,
    UnitMatches,
    FieldsChanged,
    MembershipChanged,
    FenceUnsupported,
    ProtectedUnit,
    ParentLevel,
    SiblingTitle,
}

/// Which document a [`CarrierBlocker`] addresses. One value, named rather than
/// assumed: a blocker is a position in the bytes **this operation read**, and
/// in nothing else.
///
/// That matters because a write can mint and refuse in the same pass. Stamping
/// `- ok\n- bad <b>\n` stamps the first item and refuses the second, and the
/// inserted marker shifts every byte after it, so the offset addresses the file
/// that was read and not the file that was written. A `preview` leaves the file
/// identical, which is why a migration reads positions from a preview and edits
/// the originals.
///
/// Once the bytes change, an offset is not translated, it is recomputed: read
/// again and take fresh positions. A guard does not rescue a stale one, because
/// only a document guard covers the whole file; a section, list or collection
/// guard hashes its own target, so it still matches after unrelated bytes
/// earlier in the file have moved every offset after them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerOrigin {
    AsRead,
}

/// The first character that makes a markstay §3.4 carrier refuse, and the list
/// it sits in. One character can block every later item of its list, so every
/// refusal in that cascade reports the same `offset`: that, scoped to the file,
/// is the grouping key a migration report counts downstream refusals by.
///
/// Present only when the offender is in the document. A refusal over text the
/// caller supplied (an item's content, or the Markdown a typed insert renders
/// from fields) carries no blocker: the reason names the character, and the
/// caller holds that text already. Reporting an offset into rendered Markdown
/// would address a string nobody sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarrierBlocker {
    pub origin: BlockerOrigin,
    /// The offending character, as written.
    pub character: String,
    /// Byte offset into the document as read.
    pub offset: usize,
    /// 1-based line.
    pub line: usize,
    /// 1-based count of Unicode scalar values from the start of `line` to the
    /// blocker, where `offset` is a byte count from the start of the file. Not
    /// a display column: a tab, a combining mark or an emoji built from several
    /// scalars will not line up with what an editor draws. It is the coordinate
    /// a `chars()` walk of the line produces, and `offset` the one a byte seek
    /// produces.
    pub column: usize,
    /// First line of the list whose source carries the blocker.
    pub list_line: usize,
    /// The list's own stay id, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_id: Option<String>,
}

impl CarrierBlocker {
    /// `offset` is a byte offset into `text`, at a character boundary.
    pub fn new(
        text: &str,
        offset: usize,
        character: char,
        list_line: usize,
        list_id: Option<String>,
    ) -> Self {
        let line_start = text.as_bytes()[..offset]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        CarrierBlocker {
            origin: BlockerOrigin::AsRead,
            character: character.to_string(),
            offset,
            line: text.as_bytes()[..offset]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1,
            column: text[line_start..offset].chars().count() + 1,
            list_line,
            list_id,
        }
    }
}

/// Machine-readable reason for refusing the explicit unowned lock form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockClearReason {
    PendingRecords,
    RepositoryUnknown,
    LockUnknown,
    LockMissing,
    UnsupportedLock,
    IdentityMismatch,
    LockRecent,
    ProcessUnknown,
    LiveGitProcess,
    LockInUse,
    IdentityOrRemoveFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<LockClearReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_reason: Option<MoveReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// For guard conflicts: the revision the target has now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// For §3.4 refusals: the character that blocked the carrier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocker: Option<CarrierBlocker>,
    /// For a guard conflict on a position handle: where the one item that
    /// still has the guarded revision now sits. A hint the engine never
    /// applies; a retry with it is checked again under a fresh lock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relocated: Option<Relocated>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relocated {
    pub select: Selector,
}

impl Diagnostic {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Diagnostic {
            code,
            message: message.into(),
            reason: None,
            move_reason: None,
            path: None,
            line: None,
            span: None,
            id: None,
            current: None,
            blocker: None,
            relocated: None,
        }
    }

    pub fn with_lock_reason(mut self, reason: LockClearReason) -> Self {
        self.reason = Some(reason);
        self
    }

    pub fn with_move_reason(mut self, reason: MoveReason) -> Self {
        self.move_reason = Some(reason);
        self
    }

    pub fn with_line(mut self, line: usize) -> Self {
        self.line = Some(line);
        self
    }

    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.path = (!path.is_empty()).then_some(path);
        self
    }

    /// Preserve a diagnostic's location when a persisted report needs text.
    pub(crate) fn detail(self) -> String {
        match self.path {
            Some(path) => format!("{path:?}: {}", self.message),
            None => self.message,
        }
    }

    pub fn with_blocker(mut self, blocker: CarrierBlocker) -> Self {
        self.blocker = Some(blocker);
        self
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

pub(crate) fn unsupported(message: impl Into<String>, span: Option<Span>) -> Diagnostic {
    let mut d = Diagnostic::new(ErrorCode::UnsupportedSyntax, message);
    d.span = span;
    d
}

/// The hex width of a revision digest: SHA-256 truncated to 128 bits. A
/// revision detects concurrent change, so the bound that matters is accidental
/// collision between the versions of one unit, not second-preimage resistance.
/// Every revision a caller holds is also echoed back through the response of
/// every read, so halving the width halves what a conversation carries.
pub const REVISION_HEX: usize = 32;

/// Versioned, domain-separated SHA-256, truncated to [`REVISION_HEX`]. Each
/// field is length framed; the kind is also visible so a guard of the wrong
/// kind is reported as such. Stored marker evidence is never a revision input:
/// callers pass current bytes.
pub fn revision(kind: &str, identity: &str, bytes: &[u8], dependencies: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for field in [
        b"rivet:revision:v1".as_slice(),
        kind.as_bytes(),
        identity.as_bytes(),
        bytes,
    ]
    .into_iter()
    .chain(dependencies.iter().copied())
    {
        h.update((field.len() as u64).to_be_bytes());
        h.update(field);
    }
    let digest = format!("{:x}", h.finalize());
    format!("rivet:v1:{kind}:{}", &digest[..REVISION_HEX])
}

/// The kind embedded in a revision string, if it is well formed.
pub fn revision_kind(revision: &str) -> Option<&str> {
    let rest = revision.strip_prefix("rivet:v1:")?;
    let (kind, hex) = rest.split_once(':')?;
    (hex.len() == REVISION_HEX && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(kind)
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn diagnostic_paths_omit_the_empty_root_and_survive_text_conversion() {
        let empty = Diagnostic::new(ErrorCode::PolicyConflict, "root event").with_path("");
        assert!(empty.path.is_none());
        assert!(serde_json::to_value(&empty).unwrap().get("path").is_none());
        assert_eq!(empty.detail(), "root event");
        let located = Diagnostic::new(ErrorCode::PolicyConflict, "archive changed")
            .with_path("archive/month/closed.md");
        assert_eq!(
            located.detail(),
            "\"archive/month/closed.md\": archive changed"
        );
        let reset = Diagnostic::new(ErrorCode::PolicyConflict, "root event")
            .with_path("archive/month")
            .with_path("");
        assert!(reset.path.is_none());
    }
}
