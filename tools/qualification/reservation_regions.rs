//! Map changed lines to intervals in one common Rust source blob.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;

use syn::spanned::Spanned;

#[derive(Default)]
pub struct Footprint {
    // Doubled line numbers leave an odd coordinate for a gap between functions.
    regions: Vec<RangeInclusive<usize>>,
    uncertain: bool,
}

impl Footprint {
    pub fn compare(&self, other: &Self) -> &'static str {
        for a in &self.regions {
            for b in &other.regions {
                if a.start() <= b.end() && b.start() <= a.end() {
                    return "overlap";
                }
            }
        }
        if self.uncertain || other.uncertain || self.regions.is_empty() || other.regions.is_empty()
        {
            "unknown"
        } else {
            "separate"
        }
    }
}

struct Function {
    key: String,
    first: usize,
    body: usize,
    last: usize,
    top_level: bool,
    starts_line: bool,
    ends_line: bool,
}

struct Map<'a> {
    functions: Vec<Function>,
    counts: BTreeMap<String, usize>,
    lines: Vec<&'a str>,
}

impl<'a> Map<'a> {
    fn parse(source: &'a str) -> Option<Self> {
        let file = syn::parse_file(source).ok()?;
        let mut map = Self {
            functions: Vec::new(),
            counts: BTreeMap::new(),
            lines: source.lines().collect(),
        };
        map.items("", &file.items);
        Some(map)
    }

    // Only walk module-level items and simple inherent impls. Local functions
    // belong to the enclosing function's reservation.
    fn items(&mut self, prefix: &str, items: &[syn::Item]) {
        for item in items {
            match item {
                syn::Item::Fn(f) => self.record(prefix, &f.sig, f.span(), &f.block),
                syn::Item::Mod(m) => {
                    if let Some((_, children)) = &m.content {
                        self.items(&format!("{prefix}mod {}::", m.ident), children);
                    }
                }
                syn::Item::Impl(i) if i.trait_.is_none() && i.generics.params.is_empty() => {
                    let syn::Type::Path(ty) = &*i.self_ty else {
                        continue;
                    };
                    let Some(name) = ty.path.get_ident() else {
                        continue;
                    };
                    if ty.qself.is_some() {
                        continue;
                    }
                    let parent = format!("{prefix}impl {name}::");
                    for member in &i.items {
                        if let syn::ImplItem::Fn(f) = member {
                            self.record(&parent, &f.sig, f.span(), &f.block);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn record(
        &mut self,
        prefix: &str,
        sig: &syn::Signature,
        span: proc_macro2::Span,
        block: &syn::Block,
    ) {
        let key = format!("{prefix}fn {}", sig.ident);
        *self.counts.entry(key.clone()).or_default() += 1;
        let first = span.start();
        let last = block.brace_token.span.close().end();
        // Span columns are character offsets. A sibling on the same line
        // prevents treating that whole line as a standalone insertion.
        let starts_line = self.lines[first.line - 1]
            .chars()
            .take(first.column)
            .all(char::is_whitespace);
        let ends_line = self.lines[last.line - 1]
            .chars()
            .skip(last.column)
            .all(char::is_whitespace);
        self.functions.push(Function {
            starts_line,
            ends_line,
            key,
            first: span.start().line,
            body: block.brace_token.span.open().start().line,
            last: block.brace_token.span.close().end().line,
            top_level: prefix.is_empty(),
        });
    }

    fn unique(&self, key: &str) -> Option<&Function> {
        if self.counts.get(key) != Some(&1) {
            return None;
        }
        self.functions.iter().find(|f| f.key == key)
    }

    /// A zero-length Git range means immediately after the specified line.
    /// Only edits strictly within the braces can take a body reservation.
    fn body(&self, start: usize, count: usize) -> Option<&Function> {
        let end = start.checked_add(count.saturating_sub(1))?;
        let f = self
            .functions
            .iter()
            .find(|f| (start > f.body || count == 0 && start == f.body) && end < f.last)?;
        self.unique(&f.key)
    }

    fn blank(&self, first: usize, end: usize) -> bool {
        self.lines
            .get(first..end)
            .is_some_and(|lines| lines.iter().all(|s| s.trim().is_empty()))
    }

    // Require two named neighbours and a whitespace-only gap in this exact
    // revision. An unindexed const, macro, comment, or deleted item is a barrier.
    fn gap(&self, left_key: &str, right_key: &str) -> Option<(usize, usize)> {
        let left = self.unique(left_key)?;
        let right = self.unique(right_key)?;
        (left.top_level
            && left.ends_line
            && right.top_level
            && right.starts_line
            && self.blank(left.last, right.first.checked_sub(1)?))
        .then_some((left.last, right.first))
    }
}

fn insertion_gap(
    base: &Map,
    before: &Map,
    after: &Map,
    boundary: usize,
    start: usize,
    count: usize,
) -> Option<RangeInclusive<usize>> {
    let start_index = start.checked_sub(1)?;
    let end = start.checked_add(count.checked_sub(1)?)?;
    let inserted = after.functions.iter().find(|f| {
        f.top_level
            && f.starts_line
            && f.ends_line
            && start <= f.first
            && f.last <= end
            && after.blank(start_index, f.first - 1)
            && after.blank(f.last, end)
    })?;
    after.unique(&inserted.key)?;
    if base.counts.contains_key(&inserted.key) || before.counts.contains_key(&inserted.key) {
        return None;
    }
    let left = after
        .functions
        .iter()
        .rfind(|f| f.top_level && f.last < start)?;
    let right = after
        .functions
        .iter()
        .find(|f| f.top_level && f.first > end)?;
    if !left.ends_line || !right.starts_line {
        return None;
    }
    after.unique(&left.key)?;
    after.unique(&right.key)?;
    if !after.blank(left.last, start - 1) || !after.blank(end, right.first - 1) {
        return None;
    }
    let (from, to) = before.gap(&left.key, &right.key)?;
    if !(from..to).contains(&boundary) {
        return None;
    }
    let (base_left, _) = base.gap(&left.key, &right.key)?;
    let point = 2 * base_left + 1;
    Some(point..=point)
}

pub fn classify(
    base: &str,
    before: &str,
    after: &str,
    hunks: &[(usize, usize, usize, usize)],
) -> Footprint {
    let mut result = Footprint::default();
    let (Some(base), Some(before), Some(after)) =
        (Map::parse(base), Map::parse(before), Map::parse(after))
    else {
        result.uncertain = true;
        return result;
    };
    for &(old, removed, new, added) in hunks {
        let region = match (before.body(old, removed), after.body(new, added)) {
            (Some(left), Some(right)) if left.key == right.key => {
                base.unique(&left.key).map(|f| {
                    // A changed declaration with the same name can still be a
                    // conflict witness, but cannot establish disjointness.
                    if base.lines[f.first - 1..f.last] != before.lines[left.first - 1..left.last] {
                        result.uncertain = true;
                    }
                    2 * f.first..=2 * f.last
                })
            }
            _ if removed == 0 && added > 0 => {
                insertion_gap(&base, &before, &after, old, new, added)
            }
            _ => None,
        };
        if let Some(region) = region {
            result.regions.push(region);
        } else {
            result.uncertain = true;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifies_methods_and_nested_tests_across_shifted_lines() {
        let base = "mod tests {\n fn first() {\n  a();\n }\n fn second() {\n  b();\n }\n}\nstruct S;\nimpl S {\n fn first() {\n  c();\n }\n}\n";
        let shifted = format!("// prefix\n{base}");
        let test = classify(base, base, &base.replace("a()", "b()"), &[(3, 1, 3, 1)]);
        let method = classify(
            base,
            &shifted,
            &shifted.replace("c()", "d()"),
            &[(13, 1, 13, 1)],
        );
        let sibling = classify(base, base, &base.replace("b()", "c()"), &[(6, 1, 6, 1)]);
        assert_eq!(test.compare(&method), "separate");
        assert_eq!(test.compare(&sibling), "separate");
        let same = classify(
            base,
            &shifted,
            &shifted.replace("a()", "x()"),
            &[(4, 1, 4, 1)],
        );
        assert_eq!(test.compare(&same), "overlap");
    }

    #[test]
    fn shares_a_gap_between_different_inserted_functions() {
        let base = "fn a() {\n one();\n}\n\nfn b() {\n two();\n}\n";
        let insert = |name: &str| {
            classify(
                base,
                base,
                &base.replace("\n\n", &format!("\n\nfn {name}() {{}}\n\n")),
                &[(4, 0, 5, 2)],
            )
        };
        let first = insert("one");
        let other = insert("two");
        assert_eq!(first.compare(&other), "overlap");
        let inside = classify(
            base,
            base,
            &base.replace("one();", "three();"),
            &[(2, 1, 2, 1)],
        );
        assert_eq!(inside.compare(&first), "separate");
        let barrier = base.replace("\n\n", "\nconst X: u8 = 1;\n\n");
        let uncertain = classify(
            base,
            &barrier,
            &barrier.replace("\n\n", "\n\nfn new() {}\n"),
            &[(5, 0, 6, 1)],
        );
        assert_eq!(uncertain.compare(&inside), "unknown");
    }

    #[test]
    fn unknown_evidence_never_creates_separation() {
        let base = "fn a() {\n one();\n}\n";
        let edit = classify(base, base, &base.replace("one", "two"), &[(2, 1, 2, 1)]);
        let duplicate = format!("{base}{base}");
        for unknown in [
            classify(base, base, base, &[]),
            classify(base, base, "fn {", &[(2, 1, 2, 1)]),
            classify(base, base, "", &[(1, 3, 0, 0)]),
            classify(base, base, &base.replace("fn a", "fn b"), &[(1, 1, 1, 1)]),
            classify(
                base,
                &duplicate,
                &duplicate.replace("one", "two"),
                &[(2, 1, 2, 1), (5, 1, 5, 1)],
            ),
        ] {
            assert_eq!(unknown.compare(&edit), "unknown");
        }
        let mixed = classify(
            base,
            base,
            &base
                .replace("one", "two")
                .replace("}\n", "}\nconst N: u8 = 1;\n"),
            &[(2, 1, 2, 1), (3, 0, 4, 1)],
        );
        assert_eq!(mixed.compare(&edit), "overlap");
    }
}
