//! Integration test: drive the Phase-3 and Phase-4 magic-wand RPCs through
//! the wire layer, verifying the dispatch/apply/discard/conflict flows
//! against an in-process engine. Mirrors the test harness in `comments_crud.rs`.
//! Design: tools/boss/docs/designs/comments-in-markdown-viewer.md §§ Phase 3, 4.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use boss_client::{BossClient, wait_for_socket};
use boss_engine::app::serve;
use boss_engine::config::{RuntimeConfig, WorkConfig};
use boss_protocol::{
    COMMENT_STATUS_DISPATCHED, CommentAnchor, CreateCommentInput, CreateProductInput,
    FrontendEvent, FrontendRequest, MAGIC_WAND_STATUS_CHORE_CREATED, WorkComment, WorkItem,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

struct TestEngine {
    socket_path: PathBuf,
    _temp: tempfile::TempDir,
    join: tokio::task::JoinHandle<Result<()>>,
}

impl TestEngine {
    async fn spawn() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let socket_path = temp.path().join("engine.sock");
        let work_config = WorkConfig::builder().cwd(temp.path().to_path_buf()).db_path(PathBuf::from(":memory:")).build();
        let cfg = Arc::new(RuntimeConfig::from_parts(work_config, None));
        let socket_for_serve = socket_path.clone();
        let join =
            tokio::spawn(async move { serve(cfg, socket_for_serve, None, None, None, None).await });
        if !wait_for_socket(socket_path.to_str().unwrap(), STARTUP_TIMEOUT).await {
            return Err(anyhow!("engine never bound socket {}", socket_path.display()));
        }
        Ok(Self {
            socket_path,
            _temp: temp,
            join,
        })
    }

    fn socket_str(&self) -> &str {
        self.socket_path.to_str().expect("socket path is utf-8")
    }
}

impl Drop for TestEngine {
    fn drop(&mut self) {
        self.join.abort();
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn anchor(exact: &str) -> CommentAnchor {
    CommentAnchor {
        exact: exact.to_owned(),
        prefix: String::new(),
        suffix: String::new(),
    }
}

async fn create_comment(
    client: &mut BossClient,
    artifact_id: &str,
    doc_version: &str,
    exact: &str,
) -> Result<WorkComment> {
    match client
        .send_request(&FrontendRequest::CommentsCreate {
            input: CreateCommentInput {
                artifact_kind: "work_item".to_owned(),
                artifact_id: artifact_id.to_owned(),
                doc_version: doc_version.to_owned(),
                anchor: anchor(exact),
                body: "please improve this section".to_owned(),
                author: "user:test@example.com".to_owned(),
                plain_text_projection_version: 1,
            },
        })
        .await?
    {
        FrontendEvent::CommentResult { comment } => Ok(comment),
        other => Err(anyhow!(
            "unexpected event: {}",
            serde_json::to_string(&other).unwrap_or_default()
        )),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Verify that `CommentsDispatchMagicWand` returns a `WorkError` when the
/// comment references a work item that does not exist. This exercises the
/// wire path through the handler and through `get_work_item`.
///
/// Auth-tier gating for `CommentsDispatchMagicWand` (`AppOrBoss` only) is
/// enforced by the in-process permissive mode in test engines (no trust roots
/// → permissive), so auth-gate logic is verified separately by unit tests in
/// `app.rs` (`authorize_rpc` tests), not here.
#[tokio::test]
async fn magic_wand_dispatch_on_nonexistent_work_item_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // Create a comment against a task id that has no corresponding row in `tasks`.
    let comment = create_comment(&mut client, "task_nonexistent", "v0", "span").await?;

    // Dispatch should return a WorkError because the task doesn't exist.
    let event = client
        .send_request(&FrontendRequest::CommentsDispatchMagicWand {
            comment_id: comment.id.clone(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("task_nonexistent") || message.contains("unknown"),
                "expected error about missing work item, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for missing work item, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}

/// Verify that `CommentsDispatchMagicWand` returns a `WorkError` when the
/// comment id is unknown.
#[tokio::test]
async fn magic_wand_dispatch_unknown_comment_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let event = client
        .send_request(&FrontendRequest::CommentsDispatchMagicWand {
            comment_id: "cmt_nonexistent".to_owned(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("cmt_nonexistent") || message.contains("unknown"),
                "expected error about unknown comment, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for unknown comment, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}

/// Verify that `CommentsDiscardMagicWand` on an unknown dispatch id returns a
/// `WorkError` (not a crash). User-tier is sufficient for discard/apply.
#[tokio::test]
async fn magic_wand_discard_unknown_id_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let event = client
        .send_request(&FrontendRequest::CommentsDiscardMagicWand {
            dispatch_id: "mwd_nonexistent".to_owned(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("mwd_nonexistent"),
                "expected error mentioning dispatch id, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for unknown dispatch, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}

/// Verify that `CommentsApplyMagicWand` on an unknown dispatch id returns a
/// `WorkError`. User-tier is sufficient.
#[tokio::test]
async fn magic_wand_apply_unknown_id_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let event = client
        .send_request(&FrontendRequest::CommentsApplyMagicWand {
            dispatch_id: "mwd_nonexistent".to_owned(),
            current_doc_version: "v0".to_owned(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("mwd_nonexistent"),
                "expected error mentioning dispatch id, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for unknown dispatch, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}

// ── Phase-4: PR-backed doc → Boss chore worker ────────────────────────────────

async fn create_product_with_repo(
    client: &mut BossClient,
    repo_url: &str,
) -> Result<boss_protocol::WorkItem> {
    match client
        .send_request(&FrontendRequest::CreateProduct {
            input: CreateProductInput {
                name: "Test Product".to_owned(),
                description: None,
                repo_remote_url: Some(repo_url.to_owned()),
                design_repo: None,
                docs_repo: None,
                worker_branch_prefix: None,
            },
        })
        .await?
    {
        FrontendEvent::WorkItemCreated { item } => Ok(item),
        other => Err(anyhow!(
            "expected WorkItemCreated, got: {}",
            serde_json::to_string(&other).unwrap_or_default()
        )),
    }
}

/// Phase 4 acceptance test: magic-wand on a `pr_doc` comment against an
/// artifact whose repo is owned by a known product → spawns a chore worker,
/// records a `chore_created` dispatch row, and transitions the comment to
/// `dispatched`. Verifies the audit link (dispatch.chore_id == chore.id).
#[tokio::test]
async fn magic_wand_pr_doc_creates_chore_and_dispatches_comment() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // The repo URL must match a product so the engine can resolve product_id.
    let repo_url = "https://github.com/test-org/test-repo.git";
    let product = create_product_with_repo(&mut client, repo_url).await?;
    let product_id = match &product {
        WorkItem::Product(p) => p.id.clone(),
        other => return Err(anyhow!("expected Product, got: {other:?}")),
    };
    assert!(!product_id.is_empty());

    // Create a comment against a pr_doc artifact on that repo.
    let branch = "boss/exec_test_phase4_a0";
    let path = "tools/boss/docs/designs/some-design.md";
    let artifact_id = format!("pr_doc:{repo_url}:{branch}:{path}");
    let comment = match client
        .send_request(&FrontendRequest::CommentsCreate {
            input: CreateCommentInput {
                artifact_kind: "pr_doc".to_owned(),
                artifact_id: artifact_id.clone(),
                doc_version: "v0".to_owned(),
                anchor: CommentAnchor {
                    exact: "the section that needs editing".to_owned(),
                    prefix: "Before text. ".to_owned(),
                    suffix: " After text.".to_owned(),
                },
                body: "Please clarify this paragraph with a concrete example.".to_owned(),
                author: "user:test@example.com".to_owned(),
                plain_text_projection_version: 1,
            },
        })
        .await?
    {
        FrontendEvent::CommentResult { comment } => comment,
        other => {
            return Err(anyhow!(
                "expected CommentResult, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            ));
        }
    };
    assert_eq!(comment.status, "active");

    // Dispatch the magic wand.
    let dispatch_event = client
        .send_request(&FrontendRequest::CommentsDispatchMagicWand {
            comment_id: comment.id.clone(),
        })
        .await?;

    let dispatch = match dispatch_event {
        FrontendEvent::MagicWandDispatched { dispatch } => dispatch,
        FrontendEvent::WorkError { message } => {
            return Err(anyhow!("unexpected WorkError from magic-wand dispatch: {message}"));
        }
        other => {
            return Err(anyhow!(
                "expected MagicWandDispatched, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            ));
        }
    };

    // The dispatch row must be `chore_created` with a non-null chore_id.
    assert_eq!(
        dispatch.status,
        MAGIC_WAND_STATUS_CHORE_CREATED,
        "dispatch status should be chore_created"
    );
    let chore_id = dispatch
        .chore_id
        .clone()
        .expect("dispatch.chore_id must be set for pr_doc path");
    assert!(!chore_id.is_empty(), "chore_id must be non-empty");
    assert_eq!(dispatch.comment_id, comment.id);
    assert_eq!(dispatch.artifact_kind, "pr_doc");
    assert_eq!(dispatch.artifact_id, artifact_id);
    // No Claude result for chore-backed dispatch.
    assert!(dispatch.result_md.is_none());
    assert!(dispatch.error_kind.is_none());

    // The comment must have transitioned to `dispatched`.
    // Use CommentsList (include_resolved=true to see non-active statuses).
    let list_event = client
        .send_request(&FrontendRequest::CommentsList {
            artifact_kind: "pr_doc".to_owned(),
            artifact_id: artifact_id.clone(),
            include_resolved: true,
        })
        .await?;
    let comments: Vec<WorkComment> = match list_event {
        FrontendEvent::CommentsList { comments, .. } => comments,
        other => {
            return Err(anyhow!(
                "expected CommentsList, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            ));
        }
    };
    let updated_comment = comments
        .iter()
        .find(|c| c.id == comment.id)
        .ok_or_else(|| anyhow!("comment not found in list"))?;
    assert_eq!(
        updated_comment.status,
        COMMENT_STATUS_DISPATCHED,
        "comment status should be dispatched after magic-wand"
    );

    // The spawned chore must exist and carry the right shape.
    let chore_event = client
        .send_request(&FrontendRequest::GetWorkItem { id: chore_id.clone() })
        .await?;
    let chore_task = match chore_event {
        FrontendEvent::WorkItemResult { item: WorkItem::Chore(t) } => t,
        other => {
            return Err(anyhow!(
                "expected Chore WorkItemResult, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            ));
        }
    };
    assert_eq!(chore_task.product_id, product_id);
    assert_eq!(
        chore_task.created_via,
        format!("comment_dispatch:{}", comment.id)
    );
    // repo_remote_url on the chore is None — it's inherited from the product.
    assert!(chore_task.repo_remote_url.is_none());
    // The chore description must mention the branch and the comment body.
    let desc = chore_task.description;
    assert!(
        desc.contains(branch),
        "chore description must mention branch {branch}"
    );
    assert!(
        desc.contains("Please clarify this paragraph with a concrete example."),
        "chore description must include the comment body"
    );
    assert!(
        desc.contains("the section that needs editing"),
        "chore description must include the anchor quote"
    );

    Ok(())
}

/// Verify that dispatching against a `pr_doc` artifact whose repo is NOT
/// owned by any product returns a `WorkError`.
#[tokio::test]
async fn magic_wand_pr_doc_unknown_repo_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    // No product created → no matching repo.
    let artifact_id =
        "pr_doc:https://github.com/nobody/nowhere.git:boss/exec_x_a0:design.md".to_owned();
    let comment = match client
        .send_request(&FrontendRequest::CommentsCreate {
            input: CreateCommentInput {
                artifact_kind: "pr_doc".to_owned(),
                artifact_id,
                doc_version: "v0".to_owned(),
                anchor: CommentAnchor {
                    exact: "span".to_owned(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                body: "fix this".to_owned(),
                author: "user:test@example.com".to_owned(),
                plain_text_projection_version: 1,
            },
        })
        .await?
    {
        FrontendEvent::CommentResult { comment } => comment,
        other => {
            return Err(anyhow!(
                "expected CommentResult, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            ));
        }
    };

    let event = client
        .send_request(&FrontendRequest::CommentsDispatchMagicWand {
            comment_id: comment.id.clone(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("no product"),
                "expected error about missing product, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for unknown repo, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}

/// Verify that dispatching against an unsupported `artifact_kind` returns a
/// `WorkError` with a clear message.
#[tokio::test]
async fn magic_wand_unsupported_artifact_kind_returns_error() -> Result<()> {
    let engine = TestEngine::spawn().await?;
    let mut client = BossClient::connect_socket(engine.socket_str()).await?;

    let comment = match client
        .send_request(&FrontendRequest::CommentsCreate {
            input: CreateCommentInput {
                artifact_kind: "unknown_kind".to_owned(),
                artifact_id: "some_id".to_owned(),
                doc_version: "v0".to_owned(),
                anchor: CommentAnchor {
                    exact: "span".to_owned(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                body: "fix this".to_owned(),
                author: "user:test@example.com".to_owned(),
                plain_text_projection_version: 1,
            },
        })
        .await?
    {
        FrontendEvent::CommentResult { comment } => comment,
        other => {
            return Err(anyhow!(
                "expected CommentResult, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            ));
        }
    };

    let event = client
        .send_request(&FrontendRequest::CommentsDispatchMagicWand {
            comment_id: comment.id.clone(),
        })
        .await?;
    match event {
        FrontendEvent::WorkError { message } => {
            assert!(
                message.contains("unsupported artifact_kind"),
                "expected error about unsupported artifact_kind, got: {message}"
            );
        }
        other => {
            panic!(
                "expected WorkError for unsupported kind, got: {}",
                serde_json::to_string(&other).unwrap_or_default()
            );
        }
    }

    Ok(())
}
