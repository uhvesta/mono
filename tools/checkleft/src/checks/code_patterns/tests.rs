use std::fs;
use std::path::Path;

use tempfile::tempdir;

use super::CodePatternsCheck;
use crate::check::Check;
use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use crate::output::Finding;
use crate::source_tree::LocalSourceTree;

/// Write `source` as `demo/Foo.java`, run `CodePatternsCheck` with a single
/// zero-arg `Future#get()` `nocall` rule over it, and return the findings.
///
/// Keeps the behavior-level tests below focused on the observable result
/// (number/line of findings) rather than on harness boilerplate.
async fn findings_for_future_get(source: &str) -> Vec<Finding> {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(temp.path().join("src/main/java/demo/Foo.java"), source).expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check")
        .findings
}

fn finding_lines(findings: &[Finding]) -> Vec<u32> {
    findings
        .iter()
        .filter_map(|finding| finding.location.as_ref().and_then(|location| location.line))
        .collect()
}

#[tokio::test]
async fn flags_future_get_on_completable_future_variable() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(
        temp.path().join("src/main/java/demo/Foo.java"),
        r#"
package demo;

import java.util.concurrent.CompletableFuture;

class Foo {
    String load() throws Exception {
        CompletableFuture<String> future = someAsyncMethod();
        return future.get();
    }

    CompletableFuture<String> someAsyncMethod() {
        return new CompletableFuture<>();
    }
}
"#,
    )
    .expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let result = check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check");

    assert_eq!(result.findings.len(), 1);
    assert_eq!(
        result.findings[0].location.as_ref().and_then(|location| location.line),
        Some(9)
    );
}

#[tokio::test]
async fn flags_var_inferred_from_local_method_return_type() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(
        temp.path().join("src/main/java/demo/Foo.java"),
        r#"
package demo;

import java.util.concurrent.CompletableFuture;

class Foo {
    String load() throws Exception {
        var future = someAsyncMethod();
        return future.get();
    }

    CompletableFuture<String> someAsyncMethod() {
        return new CompletableFuture<>();
    }
}
"#,
    )
    .expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let result = check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check");

    assert_eq!(result.findings.len(), 1);
}

#[tokio::test]
async fn flags_same_file_subtype_for_future_pattern() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(
        temp.path().join("src/main/java/demo/Foo.java"),
        r#"
package demo;

import java.util.concurrent.Future;

class MyFuture implements Future<String> {}

class Foo {
    String load(MyFuture future) throws Exception {
        return future.get();
    }
}
"#,
    )
    .expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let result = check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check");

    assert_eq!(result.findings.len(), 1);
}

#[tokio::test]
async fn ignores_timeout_overload_and_unrelated_get() {
    let temp = tempdir().expect("create temp dir");
    fs::create_dir_all(temp.path().join("src/main/java/demo")).expect("create dirs");
    fs::write(
        temp.path().join("src/main/java/demo/Foo.java"),
        r#"
package demo;

import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

class Other {
    String get() { return "ok"; }
}

class Foo {
    String load(Future<String> future, Other other) throws Exception {
        future.get(1L, TimeUnit.SECONDS);
        return other.get();
    }
}
"#,
    )
    .expect("write source");

    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let result = check
        .run(
            &ChangeSet::new(vec![ChangedFile {
                path: Path::new("src/main/java/demo/Foo.java").to_path_buf(),
                kind: ChangeKind::Modified,
                old_path: None,
            }]),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect("run check");

    assert!(result.findings.is_empty());
}

#[tokio::test]
async fn rejects_unsupported_language() {
    let temp = tempdir().expect("create temp dir");
    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let error = check
        .run(
            &ChangeSet::default(),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "kotlin"
                rules = [{ nocall = "java.util.concurrent.Future#get()" }]
            }),
        )
        .await
        .expect_err("must fail");

    assert!(error.to_string().contains("unsupported code-patterns `lang`"));
}

#[tokio::test]
async fn rejects_non_zero_arg_nocall_pattern() {
    let temp = tempdir().expect("create temp dir");
    let check = CodePatternsCheck;
    let tree = LocalSourceTree::new(temp.path()).expect("create tree");
    let error = check
        .run(
            &ChangeSet::default(),
            &tree,
            &toml::Value::Table(toml::toml! {
                lang = "java"
                rules = [{ nocall = "java.util.concurrent.Future#get(..)" }]
            }),
        )
        .await
        .expect_err("must fail");

    assert!(
        error
            .to_string()
            .contains("currently supports only zero-argument signatures")
    );
}

// (1) Subtype reached via class `extends` (the superclass path of
// collect_declared_supertypes), as opposed to the existing `implements` test.
// `MyFuture extends BaseFuture`, and `BaseFuture implements Future`, so the
// `is_assignable_to` walk must cross the superclass edge to reach Future.
#[tokio::test]
async fn flags_subtype_via_class_extends_superclass() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

class BaseFuture implements Future<String> {}

class MyFuture extends BaseFuture {}

class Foo {
    String load(MyFuture future) throws Exception {
        return future.get();
    }
}
"#,
    )
    .await;

    assert_eq!(finding_lines(&findings), vec![12]);
}

// (2) Interface `extends_interfaces` chain: `Level2 extends Level1` and
// `Level1 extends Future`. Exercises the interface_declaration branch of
// collect_declared_supertypes plus a multi-hop supertype walk.
#[tokio::test]
async fn flags_subtype_via_interface_extends_chain() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

interface Level1 extends Future<String> {}

interface Level2 extends Level1 {}

class Foo {
    String load(Level2 future) throws Exception {
        return future.get();
    }
}
"#,
    )
    .await;

    assert_eq!(finding_lines(&findings), vec![12]);
}

// (3a) Field-access owner resolution through `this`: `this.future.get()`.
// resolve_expression_type must resolve `this` to the enclosing type, then
// lookup_field_type to find the declared field's type.
#[tokio::test]
async fn flags_this_qualified_field_access_receiver() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

class Foo {
    Future<String> future;

    String load() throws Exception {
        return this.future.get();
    }
}
"#,
    )
    .await;

    assert_eq!(finding_lines(&findings), vec![10]);
}

// (3b) Field-access owner resolution through another object's field:
// `holder.future.get()`. The receiver chain is variable -> field -> method,
// so lookup_field_type runs against the resolved owner of `holder`.
#[tokio::test]
async fn flags_object_field_access_receiver() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

class Holder {
    Future<String> future;
}

class Foo {
    String load(Holder holder) throws Exception {
        return holder.future.get();
    }
}
"#,
    )
    .await;

    assert_eq!(finding_lines(&findings), vec![12]);
}

// (4a) `super`-qualified receiver. `super` resolves to the first direct
// supertype of the enclosing type; here that is Future, so `super.get()`
// matches.
#[tokio::test]
async fn flags_super_qualified_receiver() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

abstract class MyFuture implements Future<String> {
    String load() throws Exception {
        return super.get();
    }
}
"#,
    )
    .await;

    assert_eq!(finding_lines(&findings), vec![8]);
}

// (4b) Cast-expression receiver: `((Future<String>) obj).get()`. The receiver
// type comes from the cast target, resolved through the parenthesized wrapper.
#[tokio::test]
async fn flags_cast_expression_receiver() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

class Foo {
    String load(Object obj) throws Exception {
        return ((Future<String>) obj).get();
    }
}
"#,
    )
    .await;

    assert_eq!(finding_lines(&findings), vec![8]);
}

// (5) Chained invocation: the receiver's type comes from a prior call's return
// type. `makeFuture()` returns Future, so `makeFuture().get()` matches via the
// method_invocation arm of resolve_expression_type.
#[tokio::test]
async fn flags_chained_invocation_receiver_from_return_type() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

class Foo {
    Future<String> makeFuture() {
        return null;
    }

    String load() throws Exception {
        return makeFuture().get();
    }
}
"#,
    )
    .await;

    assert_eq!(finding_lines(&findings), vec![12]);
}

// (6a) Shadowing: an inner block redeclares `value` as an unrelated type. The
// shadowed call must NOT match, while the outer call (a Future) must. Exercises
// push_scope/pop_scope and innermost-first lookup_variable_type.
#[tokio::test]
async fn ignores_shadowed_variable_in_nested_scope() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

class Other {
    String get() {
        return "x";
    }
}

class Foo {
    void load(Future<String> value) throws Exception {
        {
            Other value = new Other();
            value.get();
        }
        value.get();
    }
}
"#,
    )
    .await;

    // Only the outer `value.get()` (line 18) matches; the shadowed Other.get()
    // on line 16 does not.
    assert_eq!(finding_lines(&findings), vec![18]);
}

// (6b) Reassignment across nested scopes: `value` is declared as Object in the
// method body, then reassigned to a Future inside a nested block. assign_variable
// updates the outer scope's binding, so the later `value.get()` matches.
#[tokio::test]
async fn tracks_reassignment_across_nested_scopes() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

class Foo {
    Future<String> makeFuture() {
        return null;
    }

    void load() throws Exception {
        Object value = new Object();
        {
            value = makeFuture();
        }
        value.get();
    }
}
"#,
    )
    .await;

    assert_eq!(finding_lines(&findings), vec![16]);
}

// (6c) Reassignment that narrows away from Future: a Future parameter is
// reassigned to an unrelated type, so the subsequent `get()` must not match.
// Confirms assign_variable replaces (not merely augments) the tracked type.
#[tokio::test]
async fn ignores_variable_reassigned_to_unrelated_type() {
    let findings = findings_for_future_get(
        r#"
package demo;

import java.util.concurrent.Future;

class Other {
    String get() {
        return "x";
    }
}

class Foo {
    void load(Future<String> value) throws Exception {
        value = new Other();
        value.get();
    }
}
"#,
    )
    .await;

    assert!(findings.is_empty());
}
