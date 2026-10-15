use std::collections::BTreeSet;

use anyhow::Result;
use editor::{CompletionContext, CompletionProvider, Editor};
use gpui::{Context, Entity, Task, Window};
use language::{Anchor, Buffer, CodeLabel, ToOffset};
use project::{Completion, CompletionDisplayOptions, CompletionResponse, CompletionSource};

pub(crate) struct SqlCompletionProvider {
    candidates: Vec<String>,
}

impl SqlCompletionProvider {
    pub(crate) fn new(identifiers: Vec<String>) -> Self {
        let candidates = SQL_KEYWORDS
            .iter()
            .map(|keyword| (*keyword).to_owned())
            .chain(identifiers)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Self { candidates }
    }
}

impl CompletionProvider for SqlCompletionProvider {
    fn completions(
        &self,
        buffer: &Entity<Buffer>,
        position: Anchor,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Task<Result<Vec<CompletionResponse>>> {
        let buffer = buffer.read(cx);
        let prefix_bytes = buffer
            .reversed_chars_at(position)
            .take_while(|character| is_identifier_character(*character))
            .map(char::len_utf8)
            .sum::<usize>();
        let start = buffer.anchor_before(position.to_offset(buffer).saturating_sub(prefix_bytes));
        let replace_range = start..position;
        Task::ready(Ok(vec![CompletionResponse {
            completions: self
                .candidates
                .iter()
                .map(|candidate| Completion {
                    replace_range: replace_range.clone(),
                    new_text: candidate.clone(),
                    label: CodeLabel::plain(candidate.clone(), None),
                    documentation: None,
                    source: CompletionSource::Custom,
                    icon_path: None,
                    icon_color: None,
                    match_start: Some(start),
                    snippet_deduplication_key: None,
                    insert_text_mode: None,
                    confirm: None,
                    group: None,
                })
                .collect(),
            display_options: CompletionDisplayOptions {
                dynamic_width: true,
            },
            is_incomplete: false,
        }]))
    }

    fn is_completion_trigger(
        &self,
        _buffer: &Entity<Buffer>,
        _position: Anchor,
        text: &str,
        _trigger_in_words: bool,
        _cx: &mut Context<Editor>,
    ) -> bool {
        text.chars().last().is_some_and(is_identifier_character)
    }
}

fn is_identifier_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$' | '.')
}

const SQL_KEYWORDS: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "AS",
    "ASC",
    "AVG",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "COALESCE",
    "COMMIT",
    "COUNT",
    "CREATE",
    "CROSS",
    "CURRENT_DATE",
    "CURRENT_TIMESTAMP",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "FALSE",
    "FROM",
    "FULL",
    "GROUP BY",
    "HAVING",
    "IN",
    "INNER",
    "INSERT INTO",
    "INTERSECT",
    "IS",
    "JOIN",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MAX",
    "MIN",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER BY",
    "OUTER",
    "OVER",
    "PARTITION BY",
    "RETURNING",
    "RIGHT",
    "ROLLBACK",
    "SELECT",
    "SET",
    "SUM",
    "TABLE",
    "THEN",
    "TRUE",
    "UNION",
    "UNION ALL",
    "UPDATE",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WITH",
];
