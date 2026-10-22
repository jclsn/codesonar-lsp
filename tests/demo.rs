//! Checks that the demo in `examples/demo` matches its source code.

use std::path::Path;

use codesonar_lsp::config::Config;
use codesonar_lsp::source;
use codesonar_lsp::warning::Warning;
use tower_lsp::lsp_types::{Position, TextEdit, Url};

fn demo() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/demo"))
}

async fn warnings() -> Vec<Warning> {
    let config = Config {
        sarif_file: Some("codesonar.sarif".into()),
        ..Config::default()
    };
    source::load(&config, Some(demo())).await.unwrap().unwrap()
}

fn source() -> String {
    std::fs::read_to_string(demo().join("main.c")).unwrap()
}

/// Byte offset of an LSP position. The demo is ASCII only.
fn offset(text: &str, position: Position) -> usize {
    let line: usize = text
        .split_inclusive('\n')
        .take(position.line as usize)
        .map(str::len)
        .sum();
    line + position.character as usize
}

fn apply(text: &str, edits: &[TextEdit]) -> String {
    let mut text = text.to_string();
    let mut edits = edits.to_vec();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
    for edit in edits {
        let start = offset(&text, edit.range.start);
        let end = offset(&text, edit.range.end);
        text.replace_range(start..end, &edit.new_text);
    }
    text
}

#[tokio::test]
async fn warnings_point_at_the_code() {
    let text = source();
    let main = Url::from_file_path(demo().join("main.c")).unwrap();
    let expected = [
        ("LANG.ARITH.DIVZERO", "a / b"),
        ("LANG.STRUCT.RPL", "&local"),
        ("IO.INJ.FMT", "name"),
        ("BADFUNC.GETS", "gets"),
        ("ALLOC.UAF", "dup"),
        ("ALLOC.DF", "free(dup)"),
        ("LANG.STRUCT.CONDASSIG", "x = parse_number(input)"),
        ("LANG.STRUCT.EBS", ";"),
        ("LANG.MEM.NPD", "*ptr"),
        ("LANG.MEM.NPD", "leak[0]"),
        ("ALLOC.LEAK", "malloc(100 * sizeof(int))"),
        ("LANG.STRUCT.GOTO", "goto"),
    ];

    let warnings = warnings().await;
    assert_eq!(warnings.len(), expected.len());
    for (warning, (rule, snippet)) in warnings.iter().zip(expected) {
        assert_eq!(warning.uri, main);
        assert_eq!(warning.rule_id.as_deref(), Some(rule));
        assert!(warning.description.is_some(), "{rule} has no explanation");
        let start = offset(&text, warning.range.start);
        let end = offset(&text, warning.range.end);
        assert_eq!(&text[start..end], snippet, "{rule}");
    }
}

#[tokio::test]
async fn quick_fixes_produce_the_expected_code() {
    let text = source();
    let main = Url::from_file_path(demo().join("main.c")).unwrap();
    let warnings = warnings().await;
    let fixed = |rule: &str| {
        let warning = warnings
            .iter()
            .find(|w| w.rule_id.as_deref() == Some(rule))
            .unwrap();
        apply(&text, &warning.fixes[0].changes[&main])
    };

    assert!(fixed("IO.INJ.FMT").contains(r#"printf("%s", name);"#));
    assert!(fixed("BADFUNC.GETS").contains("fgets(input, sizeof(input), stdin);"));
    assert_eq!(fixed("ALLOC.DF").matches("free(dup);").count(), 1);
    assert!(fixed("LANG.STRUCT.CONDASSIG").contains("if ((x = parse_number(input)) != 0)"));
    assert!(fixed("LANG.STRUCT.EBS").contains("for (i = 0; i <= 10; i++) {}"));
    assert!(fixed("ALLOC.LEAK").contains("free(leak);\n    goto end;"));
}
