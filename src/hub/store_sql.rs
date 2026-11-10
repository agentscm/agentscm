//! SQL shared by the SQLite and PostgreSQL stores, written once in PostgreSQL form (`$N`
//! placeholders). SQLite takes the same statement through [`sqlite`]. See
//! `docs/design/store-consolidation.md` for the order in which more statements move here.
use std::borrow::Cow;

pub const GET_SETTING: &str = "SELECT value FROM hub_settings WHERE key = $1";
pub const SET_SETTING: &str = "INSERT INTO hub_settings (key, value, updated_at, updated_by) \
     VALUES ($1, $2, $3, $4) \
     ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = $3, updated_by = $4";
pub const CREATE_SETTING: &str = "INSERT INTO hub_settings (key, value, updated_at, updated_by) \
     VALUES ($1, $2, $3, $4) ON CONFLICT (key) DO NOTHING";

/// The statement for SQLite: each `$N` placeholder outside a string literal becomes `?N`.
pub fn sqlite(sql: &str) -> Cow<'_, str> {
    if !sql.contains('$') {
        return Cow::Borrowed(sql);
    }
    let mut out = String::with_capacity(sql.len());
    let mut quoted = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // A doubled quote inside a literal toggles twice, so it stays inside.
            '\'' => quoted = !quoted,
            '$' if !quoted && chars.peek().is_some_and(char::is_ascii_digit) => {
                out.push('?');
                continue;
            }
            _ => {}
        }
        out.push(c);
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_become_sqlite_ones_outside_string_literals() {
        assert_eq!(
            sqlite("SELECT a FROM t WHERE b = $1 AND c = $12"),
            "SELECT a FROM t WHERE b = ?1 AND c = ?12"
        );
        assert_eq!(
            sqlite("SELECT '$1', 'it''s $2' WHERE x = $3"),
            "SELECT '$1', 'it''s $2' WHERE x = ?3"
        );
        assert_eq!(sqlite("SELECT $ FROM t"), "SELECT $ FROM t");
        assert!(matches!(sqlite("SELECT 1"), Cow::Borrowed(_)));
        assert_eq!(
            sqlite(GET_SETTING),
            "SELECT value FROM hub_settings WHERE key = ?1"
        );
    }

    #[test]
    fn each_shared_statement_renders_the_old_sqlite_text() {
        // The SQLite text each statement replaced, compared with whitespace collapsed.
        let words = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
        for (shared, old) in [
            (GET_SETTING, "SELECT value FROM hub_settings WHERE key = ?1"),
            (
                SET_SETTING,
                "INSERT INTO hub_settings (key, value, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (key) DO UPDATE SET value = ?2, updated_at = ?3, updated_by = ?4",
            ),
            (
                CREATE_SETTING,
                "INSERT INTO hub_settings (key, value, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (key) DO NOTHING",
            ),
        ] {
            assert_eq!(words(&sqlite(shared)), words(old));
        }
    }
}
