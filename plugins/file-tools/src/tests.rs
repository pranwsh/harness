use super::*;
use serde_json::{Value, json};
use std::{fs, path::Path};

fn invoke(
    editor: &edit::Editor,
    store: &Arc<HashStore>,
    value: Value,
) -> std::result::Result<String, String> {
    editor.run(
        store,
        serde_json::from_value(value).map_err(|e| e.to_string())?,
    )
}
fn anchor(path: &Path, n: usize) -> Value {
    let d = text::Document::decode(&fs::read(path).unwrap()).unwrap();
    serde_json::to_value(d.anchor(n).unwrap()).unwrap()
}
#[test]
fn snapshot_edits_fresh_anchors_dry_run_and_undo() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.txt");
    fs::write(&p, "one\ntwo\nthree\nfour\n").unwrap();
    let editor = edit::Editor::default();
    let store = Arc::new(HashStore::new());
    let edits = json!({"edits":[
        {"path":p,"op":"replace","start":anchor(&p,1),"end":anchor(&p,1),"text":"ONE\nextra"},
        {"path":p,"op":"delete","start":anchor(&p,3),"end":anchor(&p,3)}
    ]});
    let mut dry = edits.clone();
    dry["dry_run"] = json!(true);
    assert!(invoke(&editor, &store, dry).unwrap().contains("DRY_RUN"));
    assert_eq!(fs::read_to_string(&p).unwrap(), "one\ntwo\nthree\nfour\n");
    let out = invoke(&editor, &store, edits).unwrap();
    assert_eq!(fs::read_to_string(&p).unwrap(), "ONE\nextra\ntwo\nfour\n");
    assert!(out.contains("version="));
    assert!(out.contains("|ONE"));
    let id = out
        .lines()
        .next()
        .unwrap()
        .split("edit_id=")
        .nth(1)
        .unwrap();
    invoke(&editor, &store, json!({"undo":id})).unwrap();
    assert_eq!(fs::read_to_string(&p).unwrap(), "one\ntwo\nthree\nfour\n");
}
#[test]
fn all_validate_before_any_write_and_recovery_contains_anchors() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, "a\n").unwrap();
    fs::write(&b, "b\n").unwrap();
    let e = edit::Editor::default();
    let s = Arc::new(HashStore::new());
    let err = invoke(
        &e,
        &s,
        json!({"edits":[
            {"path":a,"op":"replace","start":anchor(&a,1),"end":anchor(&a,1),"text":"changed"},
            {"path":b,"op":"delete","start":{"line":1,"hash":"wrong"},"end":anchor(&b,1)}
        ]}),
    )
    .unwrap_err();
    assert!(err.contains("|b"));
    assert!(err.contains("version="));
    assert_eq!(fs::read_to_string(a).unwrap(), "a\n");
}
#[test]
fn ambiguity_normalization_and_versions() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.txt");
    fs::write(&p, "a  b\na  b\n").unwrap();
    let e = edit::Editor::default();
    let s = Arc::new(HashStore::new());
    let op = json!({"path":p,"op":"replace_text","old":"a b","new":"c"});
    let err = invoke(&e, &s, json!({"edits":[op.clone()]})).unwrap_err();
    assert!(err.contains("ambiguous"));
    assert!(err.contains("2:"));
    let mut chosen = op;
    chosen["occurrence"] = json!(2);
    assert!(
        invoke(&e, &s, json!({"edits":[chosen]}))
            .unwrap()
            .contains("whitespace-normalized")
    );
    assert_eq!(fs::read_to_string(&p).unwrap(), "a  b\nc\n");
    assert!(
        invoke(
            &e,
            &s,
            json!({"edits":[{"path":p,"op":"delete_file","expected_version":"stale"}]})
        )
        .unwrap_err()
        .contains("version mismatch")
    );
}
#[test]
fn lifecycle_multi_file_and_undo_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    let c = dir.path().join("c.txt");
    fs::write(&a, "hello\r\n").unwrap();
    let e = edit::Editor::default();
    let s = Arc::new(HashStore::new());
    let out = invoke(
        &e,
        &s,
        json!({"edits":[{"path":a,"op":"move","to":b},{"path":c,"op":"create","text":"new"}]}),
    )
    .unwrap();
    assert!(!a.exists());
    assert_eq!(fs::read(&b).unwrap(), b"hello\r\n");
    assert!(c.exists());
    let id = out
        .lines()
        .next()
        .unwrap()
        .split("edit_id=")
        .nth(1)
        .unwrap();
    fs::write(&b, "external").unwrap();
    assert!(
        invoke(&e, &s, json!({"undo":id}))
            .unwrap_err()
            .contains("undo refused")
    );
}
#[test]
fn syntax_rejection_and_report_only() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.rs");
    let e = edit::Editor::default();
    let s = Arc::new(HashStore::new());
    let mut args = json!({"edits":[{"path":p,"op":"create","text":"fn x( {"}]});
    assert!(
        invoke(&e, &s, args.clone())
            .unwrap_err()
            .contains("syntax errors")
    );
    assert!(!p.exists());
    args["auto_rollback"] = json!(false);
    assert!(invoke(&e, &s, args).unwrap().contains("syntax errors"));
    assert!(p.exists());
}
#[tokio::test]
async fn tools_registered_with_generated_schema() {
    use harness_contracts::{KEY_TOOLS, ToolCall};
    let ctx = Context::root();
    ctx.load(harness_tools::ToolsPlugin).unwrap();
    ctx.load(harness_hash_base::HashBasePlugin).unwrap();
    ctx.load(FileToolsPlugin).unwrap();
    let tools: Arc<harness_tools::Tools> = ctx.inject_key(KEY_TOOLS).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.txt");
    let out = tools
        .execute(
            "a",
            "s",
            1,
            ToolCall {
                id: "1".into(),
                name: "edit".into(),
                arguments: json!({"edits":[{"path":p,"op":"create","text":"hello"}]}).to_string(),
            },
        )
        .await
        .unwrap();
    assert!(out.contains("OK"));
    let out = tools
        .execute(
            "a",
            "s",
            1,
            ToolCall {
                id: "2".into(),
                name: "read".into(),
                arguments: json!({"path":p}).to_string(),
            },
        )
        .await
        .unwrap();
    assert!(out.contains("|hello"));
    assert!(edit_spec().parameters.to_string().contains("insert_before"));
}
