// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::min;
use std::io::{Cursor, Write};

use itertools::Itertools;

use crate::backend::{BackendResult, Conflict, ConflictId, ConflictPart, TreeValue};
use crate::diff::{find_line_ranges, Diff, DiffHunk};
use crate::files;
use crate::files::{MergeHunk, MergeResult};
use crate::repo_path::RepoPath;
use crate::store::Store;

const CONFLICT_START_LINE: &[u8] = b"<<<<<<<\n";
const CONFLICT_END_LINE: &[u8] = b">>>>>>>\n";
const CONFLICT_MINUS_LINE: &[u8] = b"-------\n";
const CONFLICT_PLUS_LINE: &[u8] = b"+++++++\n";

fn describe_conflict_part(part: &ConflictPart) -> String {
    match &part.value {
        TreeValue::Normal {
            id,
            executable: false,
        } => {
            format!("file with id {}", id.hex())
        }
        TreeValue::Normal {
            id,
            executable: true,
        } => {
            format!("executable file with id {}", id.hex())
        }
        TreeValue::Symlink(id) => {
            format!("symlink with id {}", id.hex())
        }
        TreeValue::Tree(id) => {
            format!("tree with id {}", id.hex())
        }
        TreeValue::GitSubmodule(id) => {
            format!("Git submodule with id {}", id.hex())
        }
        TreeValue::Conflict(id) => {
            format!("Conflict with id {}", id.hex())
        }
    }
}

fn describe_conflict(conflict: &Conflict, file: &mut dyn Write) -> std::io::Result<()> {
    file.write_all(b"Conflict:\n")?;
    for part in &conflict.removes {
        file.write_all(format!("  Removing {}\n", describe_conflict_part(part)).as_bytes())?;
    }
    for part in &conflict.adds {
        file.write_all(format!("  Adding {}\n", describe_conflict_part(part)).as_bytes())?;
    }
    Ok(())
}

fn file_parts(parts: &[ConflictPart]) -> Vec<&ConflictPart> {
    parts
        .iter()
        .filter(|part| {
            matches!(
                part.value,
                TreeValue::Normal {
                    executable: false,
                    ..
                }
            )
        })
        .collect_vec()
}

fn get_file_contents(store: &Store, path: &RepoPath, part: &ConflictPart) -> Vec<u8> {
    if let TreeValue::Normal {
        id,
        executable: false,
    } = &part.value
    {
        let mut content: Vec<u8> = vec![];
        store
            .read_file(path, id)
            .unwrap()
            .read_to_end(&mut content)
            .unwrap();
        content
    } else {
        panic!("unexpectedly got a non-file conflict part");
    }
}

fn write_diff_hunks(left: &[u8], right: &[u8], file: &mut dyn Write) -> std::io::Result<()> {
    let diff = Diff::for_tokenizer(&[left, right], &find_line_ranges);
    for hunk in diff.hunks() {
        match hunk {
            DiffHunk::Matching(content) => {
                // TODO: Remove this check once https://github.com/rust-lang/rust/issues/89716
                // has been fixed and released for long enough.
                if !content.is_empty() {
                    for line in content.split_inclusive(|b| *b == b'\n') {
                        file.write_all(b" ")?;
                        file.write_all(line)?;
                    }
                }
            }
            DiffHunk::Different(content) => {
                // TODO: Remove this check once https://github.com/rust-lang/rust/issues/89716
                // has been fixed and released for long enough.
                if !content[0].is_empty() {
                    for line in content[0].split_inclusive(|b| *b == b'\n') {
                        file.write_all(b"-")?;
                        file.write_all(line)?;
                    }
                }
                // TODO: Remove this check once https://github.com/rust-lang/rust/issues/89716
                // has been fixed and released for long enough.
                if !content[1].is_empty() {
                    for line in content[1].split_inclusive(|b| *b == b'\n') {
                        file.write_all(b"+")?;
                        file.write_all(line)?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn materialize_conflict(
    store: &Store,
    path: &RepoPath,
    conflict: &Conflict,
    output: &mut dyn Write,
) -> std::io::Result<()> {
    let file_adds = file_parts(&conflict.adds);
    let file_removes = file_parts(&conflict.removes);
    if file_adds.len() != conflict.adds.len() || file_removes.len() != conflict.removes.len() {
        // Unless all parts are regular files, we can't do much better than to try to
        // describe the conflict.
        describe_conflict(conflict, output)?;
        return Ok(());
    }

    let added_content = file_adds
        .iter()
        .map(|part| get_file_contents(store, path, part))
        .collect_vec();
    let removed_content = file_removes
        .iter()
        .map(|part| get_file_contents(store, path, part))
        .collect_vec();
    let removed_slices = removed_content.iter().map(Vec::as_slice).collect_vec();
    let added_slices = added_content.iter().map(Vec::as_slice).collect_vec();

    let merge_result = files::merge(&removed_slices, &added_slices);
    match merge_result {
        MergeResult::Resolved(content) => {
            output.write_all(&content)?;
        }
        MergeResult::Conflict(hunks) => {
            for hunk in hunks {
                match hunk {
                    MergeHunk::Resolved(content) => {
                        output.write_all(&content)?;
                    }
                    MergeHunk::Conflict { removes, adds } => {
                        let num_diffs = min(removes.len(), adds.len());

                        // TODO: Pair up a remove with an add in a way that minimizes the size of
                        // the diff
                        output.write_all(CONFLICT_START_LINE)?;
                        for i in 0..num_diffs {
                            output.write_all(CONFLICT_MINUS_LINE)?;
                            output.write_all(CONFLICT_PLUS_LINE)?;
                            write_diff_hunks(&removes[i], &adds[i], output)?;
                        }
                        for slice in removes.iter().skip(num_diffs) {
                            output.write_all(CONFLICT_MINUS_LINE)?;
                            output.write_all(slice)?;
                        }
                        for slice in adds.iter().skip(num_diffs) {
                            output.write_all(CONFLICT_PLUS_LINE)?;
                            output.write_all(slice)?;
                        }
                        output.write_all(CONFLICT_END_LINE)?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn conflict_to_materialized_value(
    store: &Store,
    path: &RepoPath,
    conflict: &Conflict,
) -> TreeValue {
    let mut buf = vec![];
    materialize_conflict(store, path, conflict, &mut buf).unwrap();
    let file_id = store.write_file(path, &mut Cursor::new(&buf)).unwrap();
    TreeValue::Normal {
        id: file_id,
        executable: false,
    }
}

/// Parses conflict markers from a slice. Returns None if there were no valid
/// conflict markers. The caller has to provide the expected number of removed
/// and added inputs to the conflicts. Conflict markers that are otherwise valid
/// will be considered invalid if they don't have the expected arity.
// TODO: "parse" is not usually the opposite of "materialize", so maybe we
// should rename them to "serialize" and "deserialize"?
pub fn parse_conflict(input: &[u8], num_removes: usize, num_adds: usize) -> Option<Vec<MergeHunk>> {
    if input.is_empty() {
        return None;
    }
    let mut hunks = vec![];
    let mut pos = 0;
    let mut resolved_start = 0;
    let mut conflict_start = None;
    for line in input.split_inclusive(|b| *b == b'\n') {
        if line == CONFLICT_START_LINE {
            conflict_start = Some(pos);
        } else if conflict_start.is_some() && line == CONFLICT_END_LINE {
            let conflict_body = &input[conflict_start.unwrap() + CONFLICT_START_LINE.len()..pos];
            let hunk = parse_conflict_hunk(conflict_body);
            match &hunk {
                MergeHunk::Conflict { removes, adds }
                    if removes.len() == num_removes && adds.len() == num_adds =>
                {
                    let resolved_slice = &input[resolved_start..conflict_start.unwrap()];
                    if !resolved_slice.is_empty() {
                        hunks.push(MergeHunk::Resolved(resolved_slice.to_vec()));
                    }
                    hunks.push(hunk);
                    resolved_start = pos + line.len();
                }
                _ => {}
            }
            conflict_start = None;
        }
        pos += line.len();
    }

    if hunks.is_empty() {
        None
    } else {
        if resolved_start < input.len() {
            hunks.push(MergeHunk::Resolved(input[resolved_start..].to_vec()));
        }
        Some(hunks)
    }
}

fn parse_conflict_hunk(input: &[u8]) -> MergeHunk {
    let mut minus_seen = false;
    let mut plus_seen = false;
    let mut body_seen = false;
    let mut removes = vec![];
    let mut adds = vec![];
    for line in input.split_inclusive(|b| *b == b'\n') {
        if line == CONFLICT_MINUS_LINE {
            minus_seen = true;
            if body_seen {
                plus_seen = false;
                body_seen = false;
            }
            removes.push(vec![]);
        } else if line == CONFLICT_PLUS_LINE {
            plus_seen = true;
            if body_seen {
                minus_seen = false;
                body_seen = false;
            }
            adds.push(vec![]);
        } else if minus_seen && plus_seen {
            body_seen = true;
            if let Some(rest) = line.strip_prefix(b"-") {
                removes.last_mut().unwrap().extend_from_slice(rest);
            } else if let Some(rest) = line.strip_prefix(b"+") {
                adds.last_mut().unwrap().extend_from_slice(rest);
            } else if let Some(rest) = line.strip_prefix(b" ") {
                removes.last_mut().unwrap().extend_from_slice(rest);
                adds.last_mut().unwrap().extend_from_slice(rest);
            } else {
                // Doesn't look like a conflict
                return MergeHunk::Resolved(vec![]);
            }
        } else if minus_seen {
            body_seen = true;
            removes.last_mut().unwrap().extend_from_slice(line);
        } else if plus_seen {
            body_seen = true;
            adds.last_mut().unwrap().extend_from_slice(line);
        } else {
            // Doesn't look like a conflict
            return MergeHunk::Resolved(vec![]);
        }
    }

    MergeHunk::Conflict { removes, adds }
}

pub fn update_conflict_from_content(
    store: &Store,
    path: &RepoPath,
    conflict_id: &ConflictId,
    content: &[u8],
) -> BackendResult<Option<ConflictId>> {
    let mut conflict = store.read_conflict(path, conflict_id)?;

    // First check if the new content is unchanged compared to the old content. If
    // it is, we don't need parse the content or write any new objects to the
    // store. This is also a way of making sure that unchanged tree/file
    // conflicts (for example) are not converted to regular files in the working
    // copy.
    let mut old_content = Vec::with_capacity(content.len());
    materialize_conflict(store, path, &conflict, &mut old_content).unwrap();
    if content == old_content {
        return Ok(Some(conflict_id.clone()));
    }

    let mut removed_content = vec![vec![]; conflict.removes.len()];
    let mut added_content = vec![vec![]; conflict.adds.len()];
    if let Some(hunks) = parse_conflict(content, conflict.removes.len(), conflict.adds.len()) {
        for hunk in hunks {
            match hunk {
                MergeHunk::Resolved(slice) => {
                    for buf in &mut removed_content {
                        buf.extend_from_slice(&slice);
                    }
                    for buf in &mut added_content {
                        buf.extend_from_slice(&slice);
                    }
                }
                MergeHunk::Conflict { removes, adds } => {
                    for (i, buf) in removes.iter().enumerate() {
                        removed_content[i].extend_from_slice(buf);
                    }
                    for (i, buf) in adds.iter().enumerate() {
                        added_content[i].extend_from_slice(buf);
                    }
                }
            }
        }
        // Now write the new files contents we found by parsing the file
        // with conflict markers. Update the Conflict object with the new
        // FileIds.
        for (i, buf) in removed_content.iter().enumerate() {
            let file_id = store.write_file(path, &mut Cursor::new(buf))?;
            if let TreeValue::Normal { id, executable: _ } = &mut conflict.removes[i].value {
                *id = file_id;
            } else {
                // TODO: This can actually happen. We should check earlier
                // that the we only attempt to parse the conflicts if it's a
                // file-only conflict.
                panic!("Found conflict markers in merge of non-files");
            }
        }
        for (i, buf) in added_content.iter().enumerate() {
            let file_id = store.write_file(path, &mut Cursor::new(buf))?;
            if let TreeValue::Normal { id, executable: _ } = &mut conflict.adds[i].value {
                *id = file_id;
            } else {
                panic!("Found conflict markers in merge of non-files");
            }
        }
        let new_conflict_id = store.write_conflict(path, &conflict)?;
        Ok(Some(new_conflict_id))
    } else {
        Ok(None)
    }
}
