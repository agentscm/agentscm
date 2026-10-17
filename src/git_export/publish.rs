//! What a forge needs to rebuild one exported commit: its metadata and the files that
//! differ from its single parent. Read through the same hardened local Git layer as the
//! export itself; nothing here touches a network or changes the mirror.
use serde::Serialize;

use super::git::validate_oid;
use super::{ExportState, Mirror};
use crate::hub::publish::{MAX_BLOB_BYTES, MAX_CHANGES, MAX_TOTAL_BYTES};
use crate::{Error, Result};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub email: String,
    /// `YYYY-MM-DDTHH:MM:SSZ`.
    pub date: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    /// `100644`, `100755` or `120000`; `000000` for a deletion.
    pub mode: String,
    /// Git blob SHA-1; None for a deletion.
    pub blob: Option<String>,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    pub revision: String,
    pub commit: String,
    pub tree: String,
    pub parent: String,
    pub author: Person,
    pub committer: Person,
    pub message: String,
    pub changes: Vec<Change>,
}

impl Mirror {
    /// The exported commit of integration event `sequence` as a forge could rebuild it. The
    /// record must be Verified or Mirrored with its objects intact, and the commit must have
    /// exactly one parent (the base the forge already holds). Only regular files, executables
    /// and symlinks, within the size limits, are supported; anything else is an error before
    /// any upload.
    pub fn publication(&self, sequence: u64) -> Result<Publication> {
        let status = self
            .status(sequence)?
            .ok_or_else(|| Error::NotFound("export record".into()))?;
        let record = status.record;
        if !matches!(record.state, ExportState::Verified | ExportState::Mirrored)
            || status.object_error.is_some()
        {
            return Err(Error::Conflict(
                "the export is not verified with its objects intact".into(),
            ));
        }
        let (Some(commit), Some(tree)) = (record.commit, record.tree) else {
            return Err(Error::Conflict("the export has no commit yet".into()));
        };
        let raw = self
            .git
            .run(&["cat-file", "commit", &commit], None, 1 << 20)?;
        let raw =
            String::from_utf8(raw).map_err(|_| Error::Invalid("the commit is not UTF-8".into()))?;
        let (headers, message) = raw
            .split_once("\n\n")
            .ok_or_else(|| Error::Invalid("the commit has no message".into()))?;
        let (mut parents, mut author, mut committer, mut commit_tree) =
            (Vec::new(), None, None, None);
        for line in headers.lines() {
            match line.split_once(' ') {
                Some(("tree", value)) => commit_tree = Some(value),
                Some(("parent", value)) => parents.push(value),
                Some(("author", value)) => author = Some(person(value)?),
                Some(("committer", value)) => committer = Some(person(value)?),
                _ => {
                    return Err(Error::Invalid(
                        "the commit has an unsupported header".into(),
                    ));
                }
            }
        }
        if commit_tree != Some(tree.as_str()) {
            return Err(Error::Conflict(
                "the commit's tree differs from the record".into(),
            ));
        }
        let [parent] = parents[..] else {
            return Err(Error::Invalid(
                "publishing needs a commit with exactly one parent (a mirror seeded from the \
                 forge's base commit)"
                    .into(),
            ));
        };
        validate_oid(parent)?;
        let changes = self.changes(parent, &commit)?;
        Ok(Publication {
            revision: record.event.after.revision,
            tree,
            parent: parent.to_owned(),
            author: author.ok_or_else(|| Error::Invalid("the commit has no author".into()))?,
            committer: committer
                .ok_or_else(|| Error::Invalid("the commit has no committer".into()))?,
            message: message.to_owned(),
            commit,
            changes,
        })
    }

    fn changes(&self, parent: &str, commit: &str) -> Result<Vec<Change>> {
        let raw = self.git.run(
            &[
                "diff-tree",
                "-r",
                "-z",
                "--raw",
                "--no-renames",
                "--no-abbrev",
                parent,
                commit,
            ],
            None,
            16 * 1024 * 1024,
        )?;
        let mut fields = raw.split(|b| *b == 0).filter(|f| !f.is_empty());
        let mut changes = Vec::new();
        let mut total = 0_u64;
        while let Some(meta) = fields.next() {
            let path = fields
                .next()
                .ok_or_else(|| Error::Invalid("truncated Git diff".into()))?;
            let meta =
                std::str::from_utf8(meta).map_err(|_| Error::Invalid("invalid Git diff".into()))?;
            let path = String::from_utf8(path.to_vec())
                .map_err(|_| Error::Invalid("a changed path is not UTF-8".into()))?;
            // `:<old mode> <new mode> <old sha> <new sha> <status>`
            let parts: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
            let [old_mode, new_mode, _, new_blob, _] = parts[..] else {
                return Err(Error::Invalid("unreadable Git diff".into()));
            };
            let supported = |mode: &str| matches!(mode, "100644" | "100755" | "120000" | "000000");
            if !supported(old_mode) || !supported(new_mode) {
                return Err(Error::Invalid(format!(
                    "{path}: only regular files, executables and symlinks can be published"
                )));
            }
            if changes.len() >= MAX_CHANGES {
                return Err(Error::Limit("more than 1000 changed paths".into()));
            }
            if new_mode == "000000" {
                changes.push(Change {
                    path,
                    mode: new_mode.into(),
                    blob: None,
                    size: 0,
                });
                continue;
            }
            validate_oid(new_blob)?;
            let size: u64 = self
                .git
                .text(&["cat-file", "-s", new_blob])?
                .parse()
                .map_err(|_| Error::Invalid("unreadable blob size".into()))?;
            total += size;
            if size > MAX_BLOB_BYTES as u64 || total > MAX_TOTAL_BYTES as u64 {
                return Err(Error::Limit(format!(
                    "{path}: files over 8 MiB, or more than 64 MiB in total, cannot be published"
                )));
            }
            changes.push(Change {
                path,
                mode: new_mode.into(),
                blob: Some(new_blob.into()),
                size,
            });
        }
        Ok(changes)
    }

    /// The bytes of a blob listed in a [`Publication`].
    pub fn blob(&self, oid: &str, size: u64) -> Result<Vec<u8>> {
        validate_oid(oid)?;
        let bytes = self
            .git
            .run(&["cat-file", "blob", oid], None, size + 8192)?;
        if bytes.len() as u64 != size {
            return Err(Error::Conflict("a blob changed while it was read".into()));
        }
        Ok(bytes)
    }
}

/// `Name <email> <unix seconds> +0000`; git-export always writes UTC.
fn person(line: &str) -> Result<Person> {
    let bad = || Error::Invalid("unsupported author or committer line".into());
    let (name, rest) = line.split_once(" <").ok_or_else(bad)?;
    let (email, rest) = rest.split_once("> ").ok_or_else(bad)?;
    let (seconds, zone) = rest.split_once(' ').ok_or_else(bad)?;
    if zone != "+0000" {
        return Err(bad());
    }
    let seconds: i64 = seconds.parse().map_err(|_| bad())?;
    Ok(Person {
        name: name.into(),
        email: email.into(),
        date: rfc3339(seconds),
    })
}

/// `YYYY-MM-DDTHH:MM:SSZ` of unix seconds (Howard Hinnant's civil-from-days).
fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::{person, rfc3339};

    #[test]
    fn dates_and_identities_round_trip() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_780_000_000), "2026-05-28T20:26:40Z");
        let p = person("Snapshot test <t@example.invalid> 1780000000 +0000").unwrap();
        assert_eq!(
            (p.name.as_str(), p.date.as_str()),
            ("Snapshot test", "2026-05-28T20:26:40Z")
        );
        assert!(person("A <b> 1 +0100").is_err());
    }
}
