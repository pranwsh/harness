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

#[test]
fn strict_arguments_and_encoding_edits() {
    assert!(
        serde_json::from_value::<edit::Args>(
            json!({"edits":[{"path":"x","op":"create","text":"x","typo":true}]})
        )
        .is_err()
    );
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.txt");
    let e = edit::Editor::default();
    let s = Arc::new(HashStore::new());
    for bytes in [
        b"one\r\ntwo".to_vec(),
        vec![255, 254, 111, 0, 110, 0, 101, 0],
    ] {
        fs::write(&p, &bytes).unwrap();
        invoke(
            &e,
            &s,
            json!({"edits":[{"path":p,"op":"replace_text","old":"one","new":"ONE"}]}),
        )
        .unwrap();
        let got = fs::read(&p).unwrap();
        let doc = text::Document::decode(&bytes).unwrap();
        assert_eq!(got, doc.encode(&doc.text.replace("one", "ONE")));
    }
}

#[test]
fn insert_boundaries_delete_and_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.txt");
    fs::write(&p, "a\nb").unwrap();
    let e = edit::Editor::default();
    let s = Arc::new(HashStore::new());
    invoke(&e,&s,json!({"edits":[{"path":p,"op":"insert_before","anchor":anchor(&p,1),"text":"first"},{"path":p,"op":"insert_after","anchor":anchor(&p,2),"text":"last\n"}]})).unwrap();
    assert_eq!(fs::read_to_string(&p).unwrap(), "first\na\nb\nlast");
    let err = invoke(&e,&s,json!({"edits":[{"path":p,"op":"delete","start":anchor(&p,1),"end":anchor(&p,3)},{"path":p,"op":"delete","start":anchor(&p,2),"end":anchor(&p,4)}]})).unwrap_err();
    assert!(err.contains("overlapping"));
    invoke(&e, &s, json!({"edits":[{"path":p,"op":"delete_file"}]})).unwrap();
    assert!(!p.exists());
}

#[test]
fn search_modes_globs_batch_and_caps() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.rs");
    fs::write(&a, "needle\nneedle\n").unwrap();
    fs::write(&b, "needle\n").unwrap();
    let s = HashStore::new();
    let run = |v: Value| read::run(&s, serde_json::from_value(v).unwrap()).unwrap();
    let out = run(
        json!({"path":dir.path(),"search":{"regex":"needle","glob":"**/*.txt","output":"count"}}),
    );
    assert!(out.contains("a.txt matches=2"));
    assert!(!out.contains("b.rs"));
    let out = run(json!({"paths":[a,b],"anchors":false}));
    assert!(out.contains("1: needle"));
    assert!(!out.contains("|needle"));
    let out = run(json!({"path":a,"max_line_width":2}));
    assert!(out.contains("line truncated"));
}

#[test]
#[ignore = "manual performance measurement"]
fn benchmark_windowed_read() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("large.txt");
    fs::write(
        &p,
        "let example_value = 123456789; // benchmark line\n".repeat(100000),
    )
    .unwrap();
    let store = HashStore::new();
    // Warm tokenizer so its one-time construction does not dominate the result.
    let _ = text::bounded("warm".into(), 4096).unwrap();
    let start = std::time::Instant::now();
    for _ in 0..5 {
        let _ = store.snapshot(&p).unwrap();
    }
    eprintln!("legacy snapshot 5 iterations: {:?}", start.elapsed());
    let start = std::time::Instant::now();
    for _ in 0..5 {
        read::run(
            &store,
            serde_json::from_value(json!({"path":p,"ranges":[{"start":100,"end":150}]})).unwrap(),
        )
        .unwrap();
    }
    eprintln!(
        "new read including bounded rendering 5 iterations: {:?}; bytes={}",
        start.elapsed(),
        fs::metadata(&p).unwrap().len()
    );
}

#[test]
fn undo_restores_multi_file_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    let c = dir.path().join("c.txt");
    fs::write(&a, "source").unwrap();
    fs::write(&c, "deleted").unwrap();
    let e = edit::Editor::default();
    let s = Arc::new(HashStore::new());
    let out = invoke(
        &e,
        &s,
        json!({"edits":[{"path":a,"op":"move","to":b},{"path":c,"op":"delete_file"}]}),
    )
    .unwrap();
    let id = out
        .lines()
        .next()
        .unwrap()
        .split("edit_id=")
        .nth(1)
        .unwrap();
    invoke(&e, &s, json!({"undo":id})).unwrap();
    assert_eq!(fs::read_to_string(&a).unwrap(), "source");
    assert!(!b.exists());
    assert_eq!(fs::read_to_string(&c).unwrap(), "deleted");
}
#[test]
fn opposite_order_transactions_do_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    fs::write(&a, "x\n").unwrap();
    fs::write(&b, "x\n").unwrap();
    let s = Arc::new(HashStore::new());
    let e = Arc::new(edit::Editor::default());
    let mut threads = Vec::new();
    for paths in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
        let s = s.clone();
        let e = e.clone();
        threads.push(std::thread::spawn(move || {
            invoke(&e,&s,json!({"edits":paths.iter().map(|p| json!({"path":p,"op":"replace_text","old":"x","new":"x"})).collect::<Vec<_>>()})).unwrap();
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
}
