const MAX_INPUT_BYTES: usize = 128 * 1024;
const MAX_INPUT_LINES: usize = 512;
const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_CONFLICT_PREVIEW_BYTES: usize = 32 * 1024;

const DEFAULT_WORK: usize = 8_000_000;
const DEFAULT_INPUT_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextBudget {
    remaining_work: usize,
    remaining_input_bytes: usize,
    remaining_output_bytes: usize,
}

impl Default for TextBudget {
    fn default() -> Self {
        Self {
            remaining_work: DEFAULT_WORK,
            remaining_input_bytes: DEFAULT_INPUT_BYTES,
            remaining_output_bytes: DEFAULT_OUTPUT_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextOutcome {
    Merged(Vec<u8>),
    Conflict { preview: Vec<u8>, truncated: bool },
    Unsupported(TextReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextReason {
    InputTooLarge,
    TooManyLines,
    InvalidUtf8,
    Binary,
    IncompatibleLineEndings,
    InputBudgetExceeded,
    WorkBudgetExceeded,
    OutputTooLarge,
    OutputBudgetExceeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Newline {
    Lf,
    CrLf,
}

struct Profile<'a> {
    text: &'a str,
    lines: usize,
    newline: Option<Newline>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LineEdit {
    base_start: usize,
    base_end: usize,
    replacement_start: usize,
    replacement_end: usize,
}

#[derive(Clone, Copy)]
enum EditSide {
    Ours,
    Theirs,
}

#[derive(Clone, Copy)]
struct SelectedEdit {
    edit: LineEdit,
    side: EditSide,
}

struct EditScript {
    edits: Vec<LineEdit>,
    ambiguous: bool,
}

enum ExactMerge {
    Merged(Vec<u8>),
    Overlap,
    Ambiguous,
    UnsafeBoundary,
}

const SEQUENCE_EMPTY: u32 = 0;
const SEQUENCE_AMBIGUOUS: u32 = u32::MAX;
const CHOICE_MATCH: u8 = 1;
const CHOICE_DELETE: u8 = 2;
const CHOICE_INSERT: u8 = 3;

pub(crate) fn merge(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    budget: &mut TextBudget,
) -> TextOutcome {
    // These profile and budget checks bound both the exact line composer and
    // diffy's recursive Myers fallback. Charges are conservative admission,
    // not a measured latency guarantee.
    let base = match profile(base) {
        Ok(profile) => profile,
        Err(reason) => return TextOutcome::Unsupported(reason),
    };
    let ours = match profile(ours) {
        Ok(profile) => profile,
        Err(reason) => return TextOutcome::Unsupported(reason),
    };
    let theirs = match profile(theirs) {
        Ok(profile) => profile,
        Err(reason) => return TextOutcome::Unsupported(reason),
    };
    if !compatible_newlines(&[base.newline, ours.newline, theirs.newline]) {
        return TextOutcome::Unsupported(TextReason::IncompatibleLineEndings);
    }

    let input_bytes = match base
        .text
        .len()
        .checked_add(ours.text.len())
        .and_then(|total| total.checked_add(theirs.text.len()))
    {
        Some(total) => total,
        None => return TextOutcome::Unsupported(TextReason::InputBudgetExceeded),
    };
    let work = match merge_work(base.lines, ours.lines, theirs.lines) {
        Some(work) => work,
        None => return TextOutcome::Unsupported(TextReason::WorkBudgetExceeded),
    };
    if input_bytes > budget.remaining_input_bytes {
        return TextOutcome::Unsupported(TextReason::InputBudgetExceeded);
    }
    if work > budget.remaining_work {
        return TextOutcome::Unsupported(TextReason::WorkBudgetExceeded);
    }
    // Reserve the shared inputs and original diff work atomically before line
    // classification, diffy's Myers fallback, and output allocation begin.
    budget.remaining_input_bytes -= input_bytes;
    budget.remaining_work -= work;

    // These comparisons replace the admitted diff on a fast-path success. If
    // their complete byte scan does not fit inside the original charge, reserve
    // it separately before comparing.
    let compare_whole = input_bytes <= work || {
        if input_bytes <= budget.remaining_work {
            budget.remaining_work -= input_bytes;
            true
        } else {
            false
        }
    };
    if compare_whole {
        if ours.text == theirs.text {
            return retain_exact_output(ours.text, budget);
        }
        if ours.text == base.text {
            return retain_exact_output(theirs.text, budget);
        }
        if theirs.text == base.text {
            return retain_exact_output(ours.text, budget);
        }
    }

    match exact_line_merge(&base, &ours, &theirs, budget) {
        Ok(ExactMerge::Merged(merged)) => return TextOutcome::Merged(merged),
        Ok(ExactMerge::Overlap) => {}
        Ok(ExactMerge::Ambiguous | ExactMerge::UnsafeBoundary) => {
            return forced_conflict(base.text, ours.text, theirs.text, budget);
        }
        Err(reason) => return TextOutcome::Unsupported(reason),
    }

    let mut options = diffy::MergeOptions::new();
    options
        .set_conflict_style(diffy::ConflictStyle::Diff3)
        .set_incomplete_hunk_style(diffy::IncompleteHunkStyle::Git);
    match options.merge(base.text, ours.text, theirs.text) {
        Ok(merged) => retain_output(merged, false, budget),
        Err(conflict) => retain_output(conflict, true, budget),
    }
}

fn profile(bytes: &[u8]) -> Result<Profile<'_>, TextReason> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(TextReason::InputTooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| TextReason::InvalidUtf8)?;
    if bytes.contains(&0) {
        return Err(TextReason::Binary);
    }

    let mut saw_lf = false;
    let mut saw_crlf = false;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' => {
                if bytes.get(index + 1) != Some(&b'\n') {
                    return Err(TextReason::IncompatibleLineEndings);
                }
                saw_crlf = true;
                index += 2;
            }
            b'\n' => {
                saw_lf = true;
                index += 1;
            }
            _ => index += 1,
        }
    }
    if saw_lf && saw_crlf {
        return Err(TextReason::IncompatibleLineEndings);
    }
    let newline = if saw_crlf {
        Some(Newline::CrLf)
    } else if saw_lf {
        Some(Newline::Lf)
    } else {
        None
    };
    let lines = bytes.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
    if lines > MAX_INPUT_LINES {
        return Err(TextReason::TooManyLines);
    }
    Ok(Profile {
        text,
        lines,
        newline,
    })
}

fn compatible_newlines(newlines: &[Option<Newline>]) -> bool {
    let mut selected = None;
    for newline in newlines.iter().flatten() {
        if selected.is_some_and(|selected| selected != *newline) {
            return false;
        }
        selected = Some(*newline);
    }
    true
}

fn merge_work(base_lines: usize, our_lines: usize, their_lines: usize) -> Option<usize> {
    let ours = base_lines.checked_add(our_lines)?;
    let theirs = base_lines.checked_add(their_lines)?;
    ours.checked_mul(ours)?
        .checked_add(theirs.checked_mul(theirs)?)
}

fn exact_line_merge(
    base: &Profile<'_>,
    ours: &Profile<'_>,
    theirs: &Profile<'_>,
    budget: &mut TextBudget,
) -> Result<ExactMerge, TextReason> {
    let work = exact_merge_work(
        base.lines,
        base.text.len(),
        ours.lines,
        ours.text.len(),
        theirs.lines,
        theirs.text.len(),
    )
    .ok_or(TextReason::WorkBudgetExceeded)?;
    if work > budget.remaining_work {
        return Err(TextReason::WorkBudgetExceeded);
    }
    // This separately reserves both LCS tables, deterministic walks, the
    // worst-case cross-side edit comparisons, and output planning before any
    // of their bounded vectors are allocated.
    budget.remaining_work -= work;

    let base_lines = collect_lines(base);
    let our_lines = collect_lines(ours);
    let their_lines = collect_lines(theirs);
    let our_script = line_edits(&base_lines, &our_lines);
    let their_script = line_edits(&base_lines, &their_lines);
    if our_script.ambiguous || their_script.ambiguous {
        return Ok(ExactMerge::Ambiguous);
    }

    for ours in &our_script.edits {
        for theirs in &their_script.edits {
            if edits_conflict(*ours, &our_lines, *theirs, &their_lines) {
                return Ok(ExactMerge::Overlap);
            }
        }
    }

    let selected = combine_edits(
        &our_script.edits,
        &our_lines,
        &their_script.edits,
        &their_lines,
    );
    if has_unsafe_segment_boundary(&base_lines, &our_lines, &their_lines, &selected) {
        return Ok(ExactMerge::UnsafeBoundary);
    }
    let output_bytes = merged_output_len(&base_lines, &our_lines, &their_lines, &selected)
        .ok_or(TextReason::OutputTooLarge)?;
    charge_output(output_bytes, budget)?;

    let mut output = String::with_capacity(output_bytes);
    let mut base_cursor = 0usize;
    for selected in selected {
        let edit = selected.edit;
        append_lines(&mut output, &base_lines[base_cursor..edit.base_start]);
        match selected.side {
            EditSide::Ours => append_lines(
                &mut output,
                &our_lines[edit.replacement_start..edit.replacement_end],
            ),
            EditSide::Theirs => append_lines(
                &mut output,
                &their_lines[edit.replacement_start..edit.replacement_end],
            ),
        }
        base_cursor = edit.base_end;
    }
    append_lines(&mut output, &base_lines[base_cursor..]);
    debug_assert_eq!(output.len(), output_bytes);
    Ok(ExactMerge::Merged(output.into_bytes()))
}

fn collect_lines<'a>(profile: &Profile<'a>) -> Vec<&'a str> {
    let mut lines = Vec::with_capacity(profile.lines);
    lines.extend(profile.text.split_inclusive('\n'));
    lines
}

fn forced_conflict(base: &str, ours: &str, theirs: &str, budget: &mut TextBudget) -> TextOutcome {
    let mut options = diffy::MergeOptions::new();
    options
        .set_conflict_style(diffy::ConflictStyle::Diff3)
        .set_incomplete_hunk_style(diffy::IncompleteHunkStyle::Git);
    match options.merge(base, ours, theirs) {
        Err(conflict) => retain_output(conflict, true, budget),
        Ok(_) => explicit_conflict(base, ours, theirs, budget),
    }
}

fn explicit_conflict(base: &str, ours: &str, theirs: &str, budget: &mut TextBudget) -> TextOutcome {
    const OURS: &[u8] = b"<<<<<<< ours\n";
    const ORIGINAL: &[u8] = b"||||||| original\n";
    const DIVIDER: &[u8] = b"=======\n";
    const THEIRS: &[u8] = b">>>>>>> theirs\n";

    let output_bytes = [OURS.len(), ORIGINAL.len(), DIVIDER.len(), THEIRS.len()]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .and_then(|total| checked_section_len(total, ours))
        .and_then(|total| checked_section_len(total, base))
        .and_then(|total| checked_section_len(total, theirs));
    let output_bytes = match output_bytes {
        Some(output_bytes) => output_bytes,
        None => return TextOutcome::Unsupported(TextReason::OutputTooLarge),
    };
    if let Err(reason) = charge_output(output_bytes, budget) {
        return TextOutcome::Unsupported(reason);
    }

    let mut output = Vec::with_capacity(output_bytes);
    output.extend_from_slice(OURS);
    append_conflict_section(&mut output, ours);
    output.extend_from_slice(ORIGINAL);
    append_conflict_section(&mut output, base);
    output.extend_from_slice(DIVIDER);
    append_conflict_section(&mut output, theirs);
    output.extend_from_slice(THEIRS);
    debug_assert_eq!(output.len(), output_bytes);
    let truncated = output.len() > MAX_CONFLICT_PREVIEW_BYTES;
    output.truncate(MAX_CONFLICT_PREVIEW_BYTES);
    TextOutcome::Conflict {
        preview: output,
        truncated,
    }
}

fn checked_section_len(total: usize, section: &str) -> Option<usize> {
    total
        .checked_add(section.len())?
        .checked_add(usize::from(!section.is_empty() && !section.ends_with('\n')))
}

fn append_conflict_section(output: &mut Vec<u8>, section: &str) {
    output.extend_from_slice(section.as_bytes());
    if !section.is_empty() && !section.ends_with('\n') {
        output.push(b'\n');
    }
}

fn exact_merge_work(
    base_lines: usize,
    base_bytes: usize,
    our_lines: usize,
    our_bytes: usize,
    their_lines: usize,
    their_bytes: usize,
) -> Option<usize> {
    let base_slots = base_lines.checked_add(1)?;
    let our_slots = our_lines.checked_add(1)?;
    let their_slots = their_lines.checked_add(1)?;
    let table_cells = base_slots
        .checked_mul(our_slots)?
        .checked_add(base_slots.checked_mul(their_slots)?)?;
    let cell_allocation = std::mem::size_of::<u16>()
        .checked_add(std::mem::size_of::<u32>())?
        .checked_add(std::mem::size_of::<u8>())?;
    // In addition to exact allocated bytes, reserve eight work units per cell
    // for LCS selection, three optimal candidates, sequence-state joins, and
    // the canonical action retained for the later walk.
    let tables = table_cells.checked_mul(cell_allocation.checked_add(8)?)?;
    // Every line-pair equality is exact. For either table, the sum of bytes
    // inspected is bounded both by base_lines * side_bytes and by
    // side_lines * base_bytes; charge the tighter complete bound.
    let our_comparison_bytes = base_lines
        .checked_mul(our_bytes)?
        .min(our_lines.checked_mul(base_bytes)?);
    let their_comparison_bytes = base_lines
        .checked_mul(their_bytes)?
        .min(their_lines.checked_mul(base_bytes)?);
    let comparison_bytes = our_comparison_bytes.checked_add(their_comparison_bytes)?;
    let comparisons = base_slots.checked_mul(base_slots)?;
    let line_slots = base_lines
        .checked_add(our_lines)?
        .checked_add(their_lines)?;
    let line_allocations = line_slots.checked_mul(std::mem::size_of::<&str>())?;
    let maximum_edits = base_slots.checked_mul(2)?;
    let edit_allocations = maximum_edits
        .checked_mul(std::mem::size_of::<LineEdit>())?
        .checked_add(maximum_edits.checked_mul(std::mem::size_of::<SelectedEdit>())?)?;
    // Each selected edit contributes at most one unchanged and one replacement
    // segment, followed by one final unchanged segment. Reserve the complete
    // EOF-boundary proof before collecting or inspecting those segments.
    let boundary_segments = maximum_edits.checked_mul(2)?.checked_add(1)?;
    let linear = base_lines
        .checked_add(our_lines)?
        .checked_add(their_lines)?
        .checked_add(3)?
        .checked_mul(4)?;
    let input_bytes = base_bytes
        .checked_add(our_bytes)?
        .checked_add(their_bytes)?
        .checked_mul(6)?;
    tables
        .checked_add(comparison_bytes)?
        .checked_add(comparisons)?
        .checked_add(line_allocations)?
        .checked_add(edit_allocations)?
        .checked_add(boundary_segments)?
        .checked_add(linear)?
        .checked_add(input_bytes)
}

fn line_edits(base: &[&str], side: &[&str]) -> EditScript {
    let width = side.len() + 1;
    let mut lcs = vec![0_u16; (base.len() + 1) * width];
    let mut sequences = vec![SEQUENCE_EMPTY; (base.len() + 1) * width];
    let mut choices = vec![0_u8; (base.len() + 1) * width];
    for base_index in (0..base.len()).rev() {
        for side_index in (0..side.len()).rev() {
            let index = base_index * width + side_index;
            let diagonal_index = (base_index + 1) * width + side_index + 1;
            let delete_index = (base_index + 1) * width + side_index;
            let insert_index = base_index * width + side_index + 1;
            let equal = base[base_index] == side[side_index];
            let diagonal = if equal { lcs[diagonal_index] + 1 } else { 0 };
            let delete = lcs[delete_index];
            let insert = lcs[insert_index];
            let best = diagonal.max(delete).max(insert);
            lcs[index] = best;

            let mut sequence = None;
            if equal && diagonal == best {
                let candidate = if sequences[diagonal_index] == SEQUENCE_AMBIGUOUS {
                    SEQUENCE_AMBIGUOUS
                } else {
                    // The line profile bounds this matrix far below u32::MAX.
                    index as u32 + 1
                };
                join_sequence(&mut sequence, candidate);
            }
            if delete == best {
                join_sequence(&mut sequence, sequences[delete_index]);
            }
            if insert == best {
                join_sequence(&mut sequence, sequences[insert_index]);
            }
            sequences[index] = sequence.unwrap_or(SEQUENCE_EMPTY);
            choices[index] = if equal {
                CHOICE_MATCH
            } else if delete >= insert {
                CHOICE_DELETE
            } else {
                CHOICE_INSERT
            };
        }
    }

    let mut edits = Vec::with_capacity(base.len() + 1);
    let mut base_index = 0usize;
    let mut side_index = 0usize;
    let mut change_start = None;
    while base_index < base.len() || side_index < side.len() {
        let choice = if base_index == base.len() {
            CHOICE_INSERT
        } else if side_index == side.len() {
            CHOICE_DELETE
        } else {
            choices[base_index * width + side_index]
        };
        match choice {
            CHOICE_MATCH => {
                finish_edit(&mut edits, &mut change_start, base_index, side_index);
                base_index += 1;
                side_index += 1;
            }
            CHOICE_DELETE => {
                change_start.get_or_insert((base_index, side_index));
                base_index += 1;
            }
            CHOICE_INSERT => {
                change_start.get_or_insert((base_index, side_index));
                side_index += 1;
            }
            _ => unreachable!("bounded line walk has a canonical action"),
        }
    }
    finish_edit(&mut edits, &mut change_start, base_index, side_index);
    EditScript {
        edits,
        ambiguous: sequences[0] == SEQUENCE_AMBIGUOUS,
    }
}

fn join_sequence(sequence: &mut Option<u32>, candidate: u32) {
    match *sequence {
        None => *sequence = Some(candidate),
        Some(SEQUENCE_AMBIGUOUS) => {}
        Some(current) if candidate == SEQUENCE_AMBIGUOUS || current != candidate => {
            *sequence = Some(SEQUENCE_AMBIGUOUS);
        }
        Some(_) => {}
    }
}

fn finish_edit(
    edits: &mut Vec<LineEdit>,
    change_start: &mut Option<(usize, usize)>,
    base_end: usize,
    replacement_end: usize,
) {
    if let Some((base_start, replacement_start)) = change_start.take() {
        edits.push(LineEdit {
            base_start,
            base_end,
            replacement_start,
            replacement_end,
        });
    }
}

fn edits_conflict(
    ours: LineEdit,
    our_lines: &[&str],
    theirs: LineEdit,
    their_lines: &[&str],
) -> bool {
    if same_edit(ours, our_lines, theirs, their_lines) {
        return false;
    }
    let ours_insertion = ours.base_start == ours.base_end;
    let theirs_insertion = theirs.base_start == theirs.base_end;
    match (ours_insertion, theirs_insertion) {
        (true, true) => ours.base_start == theirs.base_start,
        (true, false) => ours.base_start >= theirs.base_start && ours.base_start < theirs.base_end,
        (false, true) => theirs.base_start >= ours.base_start && theirs.base_start < ours.base_end,
        (false, false) => ours.base_start < theirs.base_end && theirs.base_start < ours.base_end,
    }
}

fn same_edit(ours: LineEdit, our_lines: &[&str], theirs: LineEdit, their_lines: &[&str]) -> bool {
    ours.base_start == theirs.base_start
        && ours.base_end == theirs.base_end
        && our_lines[ours.replacement_start..ours.replacement_end]
            == their_lines[theirs.replacement_start..theirs.replacement_end]
}

fn combine_edits(
    ours: &[LineEdit],
    our_lines: &[&str],
    theirs: &[LineEdit],
    their_lines: &[&str],
) -> Vec<SelectedEdit> {
    let mut selected = Vec::with_capacity(ours.len() + theirs.len());
    let mut our_index = 0usize;
    let mut their_index = 0usize;
    while our_index < ours.len() || their_index < theirs.len() {
        match (ours.get(our_index), theirs.get(their_index)) {
            (Some(ours), Some(theirs)) if same_edit(*ours, our_lines, *theirs, their_lines) => {
                selected.push(SelectedEdit {
                    edit: *ours,
                    side: EditSide::Ours,
                });
                our_index += 1;
                their_index += 1;
            }
            (Some(ours), Some(theirs)) if edit_key(*ours) < edit_key(*theirs) => {
                selected.push(SelectedEdit {
                    edit: *ours,
                    side: EditSide::Ours,
                });
                our_index += 1;
            }
            (Some(_), Some(theirs)) => {
                selected.push(SelectedEdit {
                    edit: *theirs,
                    side: EditSide::Theirs,
                });
                their_index += 1;
            }
            (Some(ours), None) => {
                selected.push(SelectedEdit {
                    edit: *ours,
                    side: EditSide::Ours,
                });
                our_index += 1;
            }
            (None, Some(theirs)) => {
                selected.push(SelectedEdit {
                    edit: *theirs,
                    side: EditSide::Theirs,
                });
                their_index += 1;
            }
            (None, None) => break,
        }
    }
    selected
}

fn edit_key(edit: LineEdit) -> (usize, usize) {
    (edit.base_start, edit.base_end)
}

fn has_unsafe_segment_boundary(
    base: &[&str],
    ours: &[&str],
    theirs: &[&str],
    selected: &[SelectedEdit],
) -> bool {
    let mut previous_unterminated = false;
    let mut base_cursor = 0usize;
    for selected in selected {
        let edit = selected.edit;
        if segment_follows_unterminated(
            &base[base_cursor..edit.base_start],
            &mut previous_unterminated,
        ) {
            return true;
        }
        let replacement = match selected.side {
            EditSide::Ours => &ours[edit.replacement_start..edit.replacement_end],
            EditSide::Theirs => &theirs[edit.replacement_start..edit.replacement_end],
        };
        if segment_follows_unterminated(replacement, &mut previous_unterminated) {
            return true;
        }
        base_cursor = edit.base_end;
    }
    segment_follows_unterminated(&base[base_cursor..], &mut previous_unterminated)
}

fn segment_follows_unterminated(lines: &[&str], previous_unterminated: &mut bool) -> bool {
    let Some(last) = lines.last() else {
        return false;
    };
    if *previous_unterminated {
        return true;
    }
    *previous_unterminated = !last.ends_with('\n');
    false
}

fn merged_output_len(
    base: &[&str],
    ours: &[&str],
    theirs: &[&str],
    selected: &[SelectedEdit],
) -> Option<usize> {
    let mut total = 0usize;
    let mut base_cursor = 0usize;
    for selected in selected {
        let edit = selected.edit;
        total = checked_line_bytes(total, &base[base_cursor..edit.base_start])?;
        total = match selected.side {
            EditSide::Ours => {
                checked_line_bytes(total, &ours[edit.replacement_start..edit.replacement_end])?
            }
            EditSide::Theirs => {
                checked_line_bytes(total, &theirs[edit.replacement_start..edit.replacement_end])?
            }
        };
        base_cursor = edit.base_end;
    }
    checked_line_bytes(total, &base[base_cursor..])
}

fn checked_line_bytes(mut total: usize, lines: &[&str]) -> Option<usize> {
    for line in lines {
        total = total.checked_add(line.len())?;
    }
    Some(total)
}

fn append_lines(output: &mut String, lines: &[&str]) {
    for line in lines {
        output.push_str(line);
    }
}

fn retain_exact_output(output: &str, budget: &mut TextBudget) -> TextOutcome {
    if let Err(reason) = charge_output(output.len(), budget) {
        return TextOutcome::Unsupported(reason);
    }
    TextOutcome::Merged(output.as_bytes().to_vec())
}

fn retain_output(output: String, conflict: bool, budget: &mut TextBudget) -> TextOutcome {
    if let Err(reason) = charge_output(output.len(), budget) {
        return TextOutcome::Unsupported(reason);
    }

    let mut bytes = output.into_bytes();
    if conflict {
        let truncated = bytes.len() > MAX_CONFLICT_PREVIEW_BYTES;
        bytes.truncate(MAX_CONFLICT_PREVIEW_BYTES);
        TextOutcome::Conflict {
            preview: bytes,
            truncated,
        }
    } else {
        TextOutcome::Merged(bytes)
    }
}

fn charge_output(output_bytes: usize, budget: &mut TextBudget) -> Result<(), TextReason> {
    if output_bytes > MAX_OUTPUT_BYTES {
        return Err(TextReason::OutputTooLarge);
    }
    if output_bytes > budget.remaining_output_bytes {
        return Err(TextReason::OutputBudgetExceeded);
    }
    // Charge the complete library result even when only a bounded conflict
    // preview is retained.
    budget.remaining_output_bytes -= output_bytes;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(base: &[u8], ours: &[u8], theirs: &[u8]) -> Vec<u8> {
        match merge(base, ours, theirs, &mut TextBudget::default()) {
            TextOutcome::Merged(bytes) => bytes,
            outcome => panic!("expected merged text, got {outcome:?}"),
        }
    }

    fn conflict(base: &[u8], ours: &[u8], theirs: &[u8]) -> (Vec<u8>, bool) {
        match merge(base, ours, theirs, &mut TextBudget::default()) {
            TextOutcome::Conflict { preview, truncated } => (preview, truncated),
            outcome => panic!("expected text conflict, got {outcome:?}"),
        }
    }

    #[test]
    fn merges_disjoint_changes_identically_when_sides_are_reversed() {
        let base = b"alpha\nmiddle\nomega\n";
        let ours = b"ALPHA\nmiddle\nomega\n";
        let theirs = b"alpha\nmiddle\nOMEGA\n";
        let expected = b"ALPHA\nmiddle\nOMEGA\n";

        assert_eq!(merged(base, ours, theirs), expected);
        assert_eq!(merged(base, theirs, ours), expected);
    }

    #[test]
    fn merges_the_same_change_from_both_sides() {
        assert_eq!(
            merged(b"alpha\nbeta\n", b"alpha\nBETA\n", b"alpha\nBETA\n"),
            b"alpha\nBETA\n"
        );
    }

    #[test]
    fn merges_adjacent_line_changes() {
        let base = b"alpha\nbeta\ngamma\ndelta\n";
        let ours = b"alpha\nBETA\ngamma\ndelta\n";
        let theirs = b"alpha\nbeta\nGAMMA\ndelta\n";
        let expected = b"alpha\nBETA\nGAMMA\ndelta\n";

        assert_eq!(merged(base, ours, theirs), expected);
        assert_eq!(merged(base, theirs, ours), expected);
    }

    #[test]
    fn overlapping_edits_remain_a_conflict_preview() {
        let (preview, truncated) = conflict(b"value\n", b"ours\n", b"theirs\n");

        assert!(!truncated);
        assert!(preview.starts_with(b"<<<<<<< ours\n"));
        assert!(
            preview
                .windows(b"||||||| original\n".len())
                .any(|window| { window == b"||||||| original\n" })
        );
        assert!(preview.ends_with(b">>>>>>> theirs\n"));
    }

    #[test]
    fn merges_disjoint_insertions_and_deletions() {
        assert_eq!(
            merged(
                b"alpha\nomega\n",
                b"alpha\nbeta\nomega\n",
                b"alpha\nomega\ntail\n",
            ),
            b"alpha\nbeta\nomega\ntail\n"
        );
        assert_eq!(
            merged(
                b"alpha\nbeta\ngamma\n",
                b"alpha\ngamma\n",
                b"alpha\nbeta\nGAMMA\n",
            ),
            b"alpha\nGAMMA\n"
        );
        assert_eq!(
            merged(
                b"alpha\nbeta\ngamma\n",
                b"alpha\nbeta\nGAMMA\n",
                b"alpha\ngamma\n",
            ),
            b"alpha\nGAMMA\n"
        );
    }

    #[test]
    fn same_position_insertions_require_the_same_text() {
        assert_eq!(
            merged(
                b"alpha\nomega\n",
                b"alpha\nbeta\nomega\n",
                b"alpha\nbeta\nomega\n",
            ),
            b"alpha\nbeta\nomega\n"
        );
        assert!(matches!(
            merge(
                b"alpha\nomega\n",
                b"alpha\nbeta\nomega\n",
                b"alpha\ngamma\nomega\n",
                &mut TextBudget::default(),
            ),
            TextOutcome::Conflict { .. }
        ));
        assert!(matches!(
            merge(
                b"alpha\nomega\n",
                b"alpha\ngamma\nomega\n",
                b"alpha\nbeta\nomega\n",
                &mut TextBudget::default(),
            ),
            TextOutcome::Conflict { .. }
        ));
    }

    #[test]
    fn insertion_at_a_changed_range_start_remains_a_conflict() {
        assert!(matches!(
            merge(
                b"alpha\nomega\n",
                b"alpha\ninserted\nomega\n",
                b"alpha\nOMEGA\n",
                &mut TextBudget::default(),
            ),
            TextOutcome::Conflict { .. }
        ));
    }

    #[test]
    fn repeated_lines_do_not_hide_disjoint_changes() {
        let base = b"same\nleft\nsame\nright\nsame\n";
        let ours = b"same\nLEFT\nsame\nright\nsame\n";
        let theirs = b"same\nleft\nsame\nRIGHT\nsame\n";

        assert_eq!(
            merged(base, ours, theirs),
            b"same\nLEFT\nsame\nRIGHT\nsame\n"
        );
        assert_eq!(
            merged(base, theirs, ours),
            b"same\nLEFT\nsame\nRIGHT\nsame\n"
        );
    }

    #[test]
    fn ambiguous_repeated_line_deletion_and_edit_remain_a_conflict() {
        let base = b"same\nsame\n";
        let deletion = b"same\n";
        let edit = b"RIGHT\nsame\n";

        assert!(matches!(
            merge(base, deletion, edit, &mut TextBudget::default()),
            TextOutcome::Conflict { .. }
        ));
        assert!(matches!(
            merge(base, edit, deletion, &mut TextBudget::default()),
            TextOutcome::Conflict { .. }
        ));
    }

    #[test]
    fn unchanged_repeated_region_does_not_block_distinct_nonoverlap() {
        let base = b"left\nsame\nsame\nright\n";
        let ours = b"LEFT\nsame\nsame\nright\n";
        let theirs = b"left\nsame\nsame\nRIGHT\n";
        let expected = b"LEFT\nsame\nsame\nRIGHT\n";

        assert_eq!(merged(base, ours, theirs), expected);
        assert_eq!(merged(base, theirs, ours), expected);
    }

    #[test]
    fn replacement_action_order_tie_keeps_one_matching_sequence() {
        let base = b"before\nvalue\nafter\nlast\n";
        let ours = b"before\nVALUE\nafter\nlast\n";
        let theirs = b"before\nvalue\nAFTER\nlast\n";
        let expected = b"before\nVALUE\nAFTER\nlast\n";

        assert_eq!(merged(base, ours, theirs), expected);
        assert_eq!(merged(base, theirs, ours), expected);
    }

    #[test]
    fn unterminated_replacement_before_insertion_remains_a_conflict() {
        for (ours, theirs) in [
            (b"B".as_slice(), b"b\nc\n".as_slice()),
            (b"b\nc\n".as_slice(), b"B".as_slice()),
        ] {
            let (preview, truncated) = conflict(b"b\n", ours, theirs);
            assert!(!truncated);
            assert!(preview.starts_with(b"<<<<<<< ours\n"));
        }

        for (ours, theirs) in [
            (b"B".as_slice(), b"b\r\nc\r\n".as_slice()),
            (b"b\r\nc\r\n".as_slice(), b"B".as_slice()),
        ] {
            let (preview, truncated) = conflict(b"b\r\n", ours, theirs);
            assert!(!truncated);
            assert!(preview.starts_with(b"<<<<<<< ours\n"));
        }
    }

    #[test]
    fn eof_boundary_allows_empty_segments_and_terminal_unterminated_text() {
        assert_eq!(
            merged(
                b"alpha\nbeta\ngamma\n",
                b"alpha\ngamma\n",
                b"alpha\nbeta\nGAMMA\n",
            ),
            b"alpha\nGAMMA\n"
        );
        assert_eq!(
            merged(
                b"alpha\nbeta\ngamma\n",
                b"alpha\nbeta\nGAMMA\n",
                b"alpha\ngamma\n",
            ),
            b"alpha\nGAMMA\n"
        );
        assert_eq!(
            merged(
                b"alpha\nmiddle\nomega\n",
                b"ALPHA\nmiddle\nomega\n",
                b"alpha\nmiddle\nOMEGA",
            ),
            b"ALPHA\nmiddle\nOMEGA"
        );
        assert_eq!(merged(b"b\n", b"B\n", b"b\nc\n"), b"B\nc\n");
    }

    #[test]
    fn preserves_missing_eof_newline() {
        let base = b"alpha\nmiddle\nomega";
        let ours = b"ALPHA\nmiddle\nomega";
        let theirs = b"alpha\nmiddle\nOMEGA";
        let result = merged(base, ours, theirs);

        assert_eq!(result, b"ALPHA\nmiddle\nOMEGA");
        assert!(!result.ends_with(b"\n"));
    }

    #[test]
    fn preserves_crlf_and_rejects_incompatible_endings() {
        let result = merged(
            b"alpha\r\nmiddle\r\nomega\r\n",
            b"ALPHA\r\nmiddle\r\nomega\r\n",
            b"alpha\r\nmiddle\r\nOMEGA\r\n",
        );
        assert_eq!(result, b"ALPHA\r\nmiddle\r\nOMEGA\r\n");
        assert!(
            result
                .iter()
                .enumerate()
                .all(|(index, byte)| *byte != b'\n' || index > 0 && result[index - 1] == b'\r')
        );

        assert_eq!(
            merge(
                b"alpha\n",
                b"alpha\r\n",
                b"alpha\n",
                &mut TextBudget::default(),
            ),
            TextOutcome::Unsupported(TextReason::IncompatibleLineEndings)
        );
        assert_eq!(
            merge(
                b"alpha\rbeta",
                b"alpha\rbeta",
                b"alpha\rbeta",
                &mut TextBudget::default(),
            ),
            TextOutcome::Unsupported(TextReason::IncompatibleLineEndings)
        );
        assert_eq!(
            merge(
                b"alpha\nbeta\r\n",
                b"alpha\nbeta\r\n",
                b"alpha\nbeta\r\n",
                &mut TextBudget::default(),
            ),
            TextOutcome::Unsupported(TextReason::IncompatibleLineEndings)
        );
    }

    #[test]
    fn rejects_binary_invalid_utf8_and_input_profile_limits() {
        assert_eq!(
            merge(b"a\0", b"a\0", b"a\0", &mut TextBudget::default()),
            TextOutcome::Unsupported(TextReason::Binary)
        );
        assert_eq!(
            merge(&[0xff], &[0xff], &[0xff], &mut TextBudget::default()),
            TextOutcome::Unsupported(TextReason::InvalidUtf8)
        );
        let too_large = vec![b'a'; MAX_INPUT_BYTES + 1];
        assert_eq!(
            merge(&too_large, b"small", b"small", &mut TextBudget::default()),
            TextOutcome::Unsupported(TextReason::InputTooLarge)
        );
        let too_many_lines = "x\n".repeat(MAX_INPUT_LINES + 1);
        assert_eq!(
            merge(
                too_many_lines.as_bytes(),
                b"small\n",
                b"small\n",
                &mut TextBudget::default(),
            ),
            TextOutcome::Unsupported(TextReason::TooManyLines)
        );
    }

    #[test]
    fn enforces_shared_input_work_and_output_budgets() {
        let mut input_budget = TextBudget {
            remaining_input_bytes: 3,
            ..TextBudget::default()
        };
        assert_eq!(
            merge(b"a", b"a", b"a", &mut input_budget),
            TextOutcome::Merged(b"a".to_vec())
        );
        assert_eq!(
            merge(b"a", b"a", b"a", &mut input_budget),
            TextOutcome::Unsupported(TextReason::InputBudgetExceeded)
        );

        let mut work_budget = TextBudget {
            remaining_work: 8,
            ..TextBudget::default()
        };
        assert_eq!(
            merge(b"a", b"a", b"a", &mut work_budget),
            TextOutcome::Merged(b"a".to_vec())
        );
        assert_eq!(
            merge(b"a", b"a", b"a", &mut work_budget),
            TextOutcome::Unsupported(TextReason::WorkBudgetExceeded)
        );

        let mut exact_work_budget = TextBudget {
            remaining_work: 32,
            ..TextBudget::default()
        };
        assert_eq!(
            merge(
                b"alpha\nbeta\n",
                b"ALPHA\nbeta\n",
                b"alpha\nBETA\n",
                &mut exact_work_budget,
            ),
            TextOutcome::Unsupported(TextReason::WorkBudgetExceeded)
        );
        assert_eq!(exact_work_budget.remaining_work, 0);
        assert_eq!(
            exact_work_budget.remaining_output_bytes,
            DEFAULT_OUTPUT_BYTES
        );

        let mut output_budget = TextBudget {
            remaining_output_bytes: 1,
            ..TextBudget::default()
        };
        assert_eq!(
            merge(b"a", b"a", b"a", &mut output_budget),
            TextOutcome::Merged(b"a".to_vec())
        );
        assert_eq!(
            merge(b"a", b"a", b"a", &mut output_budget),
            TextOutcome::Unsupported(TextReason::OutputBudgetExceeded)
        );

        let mut exact_output_budget = TextBudget {
            remaining_output_bytes: 3,
            ..TextBudget::default()
        };
        assert_eq!(
            merge(
                b"alpha\nbeta\n",
                b"ALPHA\nbeta\n",
                b"alpha\nBETA\n",
                &mut exact_output_budget,
            ),
            TextOutcome::Unsupported(TextReason::OutputBudgetExceeded)
        );
        assert_eq!(exact_output_budget.remaining_output_bytes, 3);

        let mut per_output_budget = TextBudget::default();
        assert_eq!(
            charge_output(MAX_OUTPUT_BYTES + 1, &mut per_output_budget),
            Err(TextReason::OutputTooLarge)
        );
        assert_eq!(
            per_output_budget.remaining_output_bytes,
            DEFAULT_OUTPUT_BYTES
        );
    }

    #[test]
    fn truncates_large_git_style_preview_without_resolving_conflict() {
        let base = vec![b'a'; 20_000];
        let ours = vec![b'b'; 20_000];
        let theirs = vec![b'c'; 20_000];

        let (preview, truncated) = conflict(&base, &ours, &theirs);
        assert!(truncated);
        assert_eq!(preview.len(), MAX_CONFLICT_PREVIEW_BYTES);
        assert!(preview.starts_with(b"<<<<<<< ours\n"));
        assert!(
            preview
                .windows(b"||||||| original\n".len())
                .any(|window| { window == b"||||||| original\n" })
        );
    }
}
