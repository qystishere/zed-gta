use std::collections::HashMap;
use std::sync::Mutex;
use tower_lsp::{jsonrpc, lsp_types::*, Client, LanguageServer, LspService, Server};

struct GtaLsp {
    client: Client,
    documents: Mutex<HashMap<Url, String>>,
}

#[tower_lsp::async_trait]
impl LanguageServer for GtaLsp {
    async fn initialize(&self, _: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                document_formatting_provider: Some(OneOf::Left(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "gta-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.documents
            .lock()
            .unwrap()
            .insert(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.documents
                .lock()
                .unwrap()
                .insert(params.text_document.uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .lock()
            .unwrap()
            .remove(&params.text_document.uri);
    }

    async fn formatting(
        &self,
        params: DocumentFormattingParams,
    ) -> jsonrpc::Result<Option<Vec<TextEdit>>> {
        let docs = self.documents.lock().unwrap();
        let content = match docs.get(&params.text_document.uri) {
            Some(c) => c.clone(),
            None => return Ok(None),
        };
        drop(docs);

        let formatted = format_file(&content);
        if formatted == content {
            return Ok(None);
        }

        let old_lines: Vec<&str> = content.lines().collect();
        let new_lines: Vec<&str> = formatted.lines().collect();
        let mut edits = Vec::new();

        for (i, (old, new)) in old_lines.iter().zip(new_lines.iter()).enumerate() {
            if old != new {
                let common_prefix = old
                    .chars()
                    .zip(new.chars())
                    .take_while(|(a, b)| a == b)
                    .count();
                let common_suffix = old[common_prefix..]
                    .chars()
                    .rev()
                    .zip(new[common_prefix..].chars().rev())
                    .take_while(|(a, b)| a == b)
                    .count();

                let old_end = old.len() - common_suffix;
                let new_end = new.len() - common_suffix;

                edits.push(TextEdit {
                    range: Range {
                        start: Position::new(i as u32, common_prefix as u32),
                        end: Position::new(i as u32, old_end as u32),
                    },
                    new_text: new[common_prefix..new_end].to_string(),
                });
            }
        }

        if edits.is_empty() {
            return Ok(None);
        }

        Ok(Some(edits))
    }
}

enum LineKind {
    Comment(String),
    SectionStart(String),
    SectionEnd(String),
    Entry(Vec<String>),
    Directive(String),
    Empty,
}

fn classify_line(line: &str) -> LineKind {
    let trimmed = line.trim();

    if trimmed.is_empty() {
        return LineKind::Empty;
    }

    if trimmed.starts_with('#') || trimmed.starts_with(';') {
        return LineKind::Comment(trimmed.to_string());
    }

    if trimmed.eq_ignore_ascii_case("end") {
        return LineKind::SectionEnd(trimmed.to_string());
    }

    if !trimmed.contains(',') && !trimmed.contains(' ') && trimmed.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return LineKind::SectionStart(trimmed.to_string());
    }

    if let Some(first_word) = trimmed.split_whitespace().next() {
        if first_word.chars().all(|c| c.is_ascii_uppercase() || c == '_')
            && trimmed.contains(' ')
            && !trimmed.contains(',')
        {
            return LineKind::Directive(trimmed.to_string());
        }
    }

    if trimmed.contains(',') {
        let fields: Vec<String> = trimmed.split(',').map(|f| f.trim().to_string()).collect();
        return LineKind::Entry(fields);
    }

    LineKind::Directive(trimmed.to_string())
}

fn format_entries(entries: &[Vec<String>]) -> Vec<String> {
    if entries.is_empty() {
        return vec![];
    }

    let num_cols = entries.iter().map(|e| e.len()).max().unwrap_or(0);
    let mut max_widths = vec![0usize; num_cols];

    for entry in entries {
        for (i, field) in entry.iter().enumerate() {
            max_widths[i] = max_widths[i].max(field.len());
        }
    }

    entries
        .iter()
        .map(|entry| {
            let mut line = String::new();
            for (i, field) in entry.iter().enumerate() {
                if i == entry.len() - 1 {
                    line.push_str(field);
                } else {
                    line.push_str(field);
                    line.push(',');
                    let padding = max_widths[i] + 2 - field.len() - 1;
                    line.push_str(&" ".repeat(padding));
                }
            }
            line
        })
        .collect()
}

fn format_file(content: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut pending_entries: Vec<Vec<String>> = Vec::new();

    let flush_entries = |pending: &mut Vec<Vec<String>>, result: &mut Vec<String>| {
        if !pending.is_empty() {
            result.extend(format_entries(pending));
            pending.clear();
        }
    };

    for line in content.lines() {
        match classify_line(line) {
            LineKind::Entry(fields) => {
                pending_entries.push(fields);
            }
            other => {
                flush_entries(&mut pending_entries, &mut result);
                match other {
                    LineKind::Comment(s)
                    | LineKind::SectionStart(s)
                    | LineKind::SectionEnd(s)
                    | LineKind::Directive(s) => result.push(s),
                    LineKind::Empty => result.push(String::new()),
                    LineKind::Entry(_) => unreachable!(),
                }
            }
        }
    }

    flush_entries(&mut pending_entries, &mut result);

    let mut output = result.join("\n");
    if content.ends_with('\n') {
        output.push('\n');
    }
    output
}

#[tokio::main]
async fn main() {
    let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());

    let (service, socket) = LspService::new(|client| GtaLsp {
        client,
        documents: Mutex::new(HashMap::new()),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_ide() {
        let input = "objs\n300, elecgate, generic, 1, 150.0, 0\n7656, nt_frstdoor01, generic, 1, 150, 0\nend\n";
        let expected = "objs\n300,  elecgate,      generic, 1, 150.0, 0\n7656, nt_frstdoor01, generic, 1, 150,   0\nend\n";
        assert_eq!(format_file(input), expected);
    }

    #[test]
    fn test_format_preserves_comments() {
        let input = "# comment\nobjs\n300, a, 1\nend\n";
        let expected = "# comment\nobjs\n300, a, 1\nend\n";
        assert_eq!(format_file(input), expected);
    }

    #[test]
    fn test_no_change() {
        let input = "objs\n300, a, 1\nend\n";
        assert_eq!(format_file(input), input);
    }
}
