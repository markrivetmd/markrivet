//! Parsing the object formats returned by Git, independently of command execution.

#[derive(Clone, Copy)]
pub(crate) enum OidCase {
    /// Git's canonical output and history request contract use lowercase hex.
    Canonical,
    /// Archive evidence accepts either ASCII hex case, as Git does.
    Either,
}

pub(crate) fn valid_oid(oid: &str, len: usize, case: OidCase) -> bool {
    matches!(len, 40 | 64)
        && oid.len() == len
        && oid.bytes().all(|b| {
            b.is_ascii_hexdigit() && (matches!(case, OidCase::Either) || !b.is_ascii_uppercase())
        })
}

pub(crate) struct CommitObject<'a> {
    pub parents: Vec<&'a str>,
    pub author: &'a str,
    pub committer: &'a str,
    pub message: &'a str,
}

pub(crate) fn parse_commit(bytes: &[u8], oid_len: usize) -> Result<CommitObject<'_>, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "invalid_commit_utf8")?;
    let (headers, message) = text.split_once("\n\n").ok_or("invalid_commit")?;
    let parents: Vec<_> = headers
        .lines()
        .filter_map(|l| l.strip_prefix("parent "))
        .collect();
    if parents
        .iter()
        .any(|p| !valid_oid(p, oid_len, OidCase::Canonical))
    {
        return Err("invalid_parent");
    }
    Ok(CommitObject {
        parents,
        author: headers
            .lines()
            .find_map(|l| l.strip_prefix("author "))
            .unwrap_or(""),
        committer: headers
            .lines()
            .find_map(|l| l.strip_prefix("committer "))
            .unwrap_or(""),
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_lengths_and_case_are_explicit() {
        for len in [40, 64] {
            for (digit, canonical, either) in [
                ('a', true, true),
                ('A', false, true),
                ('0', true, true),
                ('g', false, false),
            ] {
                let oid = digit.to_string().repeat(len);
                assert_eq!(valid_oid(&oid, len, OidCase::Canonical), canonical);
                assert_eq!(valid_oid(&oid, len, OidCase::Either), either);
                assert!(!valid_oid(
                    &oid,
                    if len == 40 { 64 } else { 40 },
                    OidCase::Either
                ));
            }
        }
        for value in ["", "abcd", "é", " a", "a\n"] {
            assert!(!valid_oid(value, value.len(), OidCase::Either));
        }
    }

    #[test]
    fn commits_keep_parent_order_and_message_and_ignore_folded_headers() {
        for len in [40, 64] {
            let a = "a".repeat(len);
            let b = "b".repeat(len);
            let text = format!(
                "tree {a}\nparent {a}\nparent {b}\nauthor A\ncommitter C\ngpgsig signature\n parent bogus\n\nsubject\n\nparent message\n"
            );
            let commit = parse_commit(text.as_bytes(), len).unwrap();
            assert_eq!(commit.parents, [a.as_str(), b.as_str()]);
            assert_eq!((commit.author, commit.committer), ("A", "C"));
            assert_eq!(commit.message, "subject\n\nparent message\n");
            assert!(
                parse_commit(format!("tree {a}\n\nroot").as_bytes(), len)
                    .unwrap()
                    .parents
                    .is_empty()
            );
            assert!(matches!(
                parse_commit(format!("parent {a}\nparent BAD\n\nx").as_bytes(), len),
                Err("invalid_parent")
            ));
        }
        assert!(matches!(parse_commit(b"tree x", 40), Err("invalid_commit")));
        assert!(matches!(
            parse_commit(b"\xff\n\nx", 40),
            Err("invalid_commit_utf8")
        ));
    }
}
