use fridica::overseer::{executor::*, *};

fn mapping() -> Repository {
    Repository {
        host: "github.com".into(),
        upstream: "upstream/project".into(),
        fork: "owner/project".into(),
        account: "owner".into(),
    }
}
fn item() -> Item {
    Item {
        id: "one".into(),
        author: "owner".into(),
        head: "a".repeat(40),
        tree: "b".repeat(40),
        owner_paused: false,
        linked_thread_paused: false,
        stopped: false,
        merge_predecessors_satisfied: true,
        requirements: vec!["unit".into()],
        required_checks: vec!["ci".into()],
        allowed_reviewers: vec!["reviewer".into()],
        validations: vec![Validation {
            requirement: "unit".into(),
            commit: "a".repeat(40),
            tree: "b".repeat(40),
            passed: true,
            job_id: "j1".into(),
            verified: true,
        }],
        checks: vec![Check {
            name: "ci".into(),
            commit: "a".repeat(40),
            completed: true,
            conclusion: "success".into(),
            verified_by_api: true,
        }],
        reviews: vec![Review {
            reviewer: "reviewer".into(),
            commit: "a".repeat(40),
            approved: true,
            verified_by_api: true,
        }],
        range_diff_changed: false,
    }
}

#[test]
fn current_verified_evidence_required_for_human_merge() {
    let ready = item();
    assert_eq!(plan(&ready), Decision::ReadyForHumanMerge);
    let mut stale = ready.clone();
    stale.head = "c".repeat(40);
    stale.range_diff_changed = true;
    assert!(matches!(plan(&stale), Decision::Validate(_)));
    let mut cancelled = ready.clone();
    cancelled.checks[0].conclusion = "cancelled".into();
    assert!(matches!(plan(&cancelled), Decision::WaitForCi(_)));
    let mut self_review = ready.clone();
    self_review.reviews[0].reviewer = "owner".into();
    assert_eq!(plan(&self_review), Decision::RequestIndependentReview);
    let mut stale_review = ready.clone();
    stale_review.reviews[0].commit = "c".repeat(40);
    assert_eq!(plan(&stale_review), Decision::RequestIndependentReview);
    let mut paused = ready.clone();
    paused.linked_thread_paused = true;
    assert!(matches!(plan(&paused), Decision::Blocked(_)));
    let mut unverified = ready;
    unverified.validations[0].verified = false;
    assert!(matches!(plan(&unverified), Decision::Validate(_)));
}

#[test]
fn trusted_commands_bind_fork_head_tree_and_lease() {
    let push = PreparedPush {
        repository: mapping(),
        branch: "topic/one".into(),
        expected_remote_head: "c".repeat(40),
        commit: "a".repeat(40),
        tree: "b".repeat(40),
    };
    let args = push.arguments(&push.commit, &push.tree).unwrap();
    assert!(args.contains(&"ssh://git@github.com/owner/project.git".into()));
    assert!(args.contains(&format!(
        "--force-with-lease=refs/heads/topic/one:{}",
        "c".repeat(40)
    )));
    assert!(push.arguments(&"d".repeat(40), &push.tree).is_err());
    for name in [
        "-delete",
        ":main",
        "main..evil",
        "a//b",
        "a.lock",
        "a/.hidden",
        "main;echo hacked",
    ] {
        let mut bad = push.clone();
        bad.branch = name.into();
        assert!(bad.arguments(&bad.commit, &bad.tree).is_err());
    }
    let mut upstream = push;
    upstream.repository.fork = upstream.repository.upstream.clone();
    assert!(upstream
        .arguments(&upstream.commit, &upstream.tree)
        .is_err());
}

#[test]
fn api_commands_keep_untrusted_body_as_one_structured_argument() {
    let body = "$(touch /tmp/never); `whoami`\nsecond line";
    let none = fridica::core::egress::DenyList::default();
    let args = gh_arguments(
        &mapping(),
        ApiOperation::Comment {
            number: 1,
            body: body.into(),
        },
        &none,
    )
    .unwrap();
    assert_eq!(args.last().unwrap(), &format!("body={body}"));
    assert!(args.contains(&"repos/upstream/project/issues/1/comments".into()));
    assert!(gh_arguments(
        &mapping(),
        ApiOperation::Checks {
            commit: "../../admin".into()
        },
        &none,
    )
    .is_err());
    // Published comments pass the egress check: private terms and AI trailers
    // are refused before any argument is built, naming only the rule.
    let deny = fridica::core::egress::DenyList::parse("Jane Q\\. Private\n").unwrap();
    for (text, rule) in [
        ("Thanks, Jane Q. Private.", "deny_list:1"),
        (
            "LGTM\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
            "ai_trailer",
        ),
    ] {
        let error = gh_arguments(
            &mapping(),
            ApiOperation::Comment {
                number: 1,
                body: text.into(),
            },
            &deny,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(rule) && !error.contains("Jane"), "{error}");
    }
}

#[tokio::test]
async fn campaign_effect_intent_survives_restart_and_stop_refuses_new_actions() {
    use fridica::{
        core::Authority,
        overseer::registry::{Campaign, Registry},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("overseer.sqlite3");
    let registry = Registry::open(path.clone()).await.unwrap();
    let campaign = Campaign {
        id: "campaign".into(),
        lead: "owner".into(),
        owners: vec!["owner".into()],
        substitutes: vec![],
        merge_order: vec!["one".into()],
    };
    registry
        .campaign(campaign, Authority::Owner, 1.)
        .await
        .unwrap();
    registry
        .item("campaign".into(), item(), Authority::Owner, 1.)
        .await
        .unwrap();
    let action = registry
        .plan("one".into(), "action-one".into(), 1.)
        .await
        .unwrap();
    assert_eq!(
        registry
            .plan("one".into(), "duplicate".into(), 1.)
            .await
            .unwrap(),
        action
    );
    registry.begin(action.clone()).await.unwrap();
    drop(registry);
    let registry = loop {
        match Registry::open(path.clone()).await {
            Ok(registry) => break registry,
            Err(error) if error.to_string().contains("holds the state") => {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await
            }
            Err(error) => panic!("{error}"),
        }
    };
    assert!(registry.begin(action.clone()).await.is_err());
    assert!(registry
        .reconcile(
            action.clone(),
            serde_json::json!({"remote_head":"verified"}),
            false,
            2.
        )
        .await
        .is_err());
    registry
        .reconcile(
            action,
            serde_json::json!({"remote_head":"verified"}),
            true,
            2.,
        )
        .await
        .unwrap();
    registry
        .stop("campaign".into(), Authority::Owner, 3.)
        .await
        .unwrap();
    let next = registry
        .plan("one".into(), "stopped".into(), 3.)
        .await
        .unwrap();
    assert!(registry.begin(next).await.is_err());
}
