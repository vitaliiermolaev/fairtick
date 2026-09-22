//! Scenario tests for the durable reward outbox (src/db.rs). These cover the guarantees
//! that make match-end crystal grants crash-safe instead of fire-and-forget:
//!   - a grant enqueued in one transaction is credited exactly once by the drain;
//!   - a process crash BETWEEN enqueue and credit is recovered by the startup replay
//!     (modelled here as "enqueue, then drain later" with no immediate credit);
//!   - replaying the same match outcome (duplicate enqueue) and re-draining never
//!     double-credits;
//!   - a grant whose user has vanished is marked terminal `failed`, not silently lost.

use fairtick::db::{
    apply_pending_reward, create_user, drain_pending_rewards, enqueue_rewards, get_user, init_db,
    OutboxReward, RewardApply,
};
use sqlx::SqlitePool;
use tempfile::TempDir;

/// A fresh on-disk SQLite db (real file, so multiple pool connections share state — an
/// `sqlite::memory:` url would give each connection its own empty db). The returned
/// TempDir must be kept alive for the db file to survive the test.
async fn test_db() -> (SqlitePool, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let url = format!("sqlite:{}?mode=rwc", dir.path().join("test.db").display());
    let pool = init_db(&url).await.expect("init_db");
    (pool, dir)
}

async fn crystals_of(pool: &SqlitePool, user_id: &str) -> i64 {
    get_user(pool, user_id).await.expect("get_user").expect("user exists").crystals
}

async fn status_of(pool: &SqlitePool, reward_id: &str) -> Option<String> {
    sqlx::query_as::<_, (String,)>("SELECT status FROM reward_outbox WHERE reward_id=?")
        .bind(reward_id)
        .fetch_optional(pool)
        .await
        .expect("status query")
        .map(|(s,)| s)
}

#[tokio::test]
async fn enqueue_then_drain_credits_exactly_once() {
    let (pool, _dir) = test_db().await;
    let user = create_user(&pool, "apple-1", "alice").await.unwrap();

    enqueue_rewards(
        &pool,
        &[OutboxReward { reward_id: format!("room-A:{}", user.id), user_id: user.id.clone(), crystals: 20 }],
    )
    .await
    .unwrap();

    // Enqueue alone is durable but NOT yet credited.
    assert_eq!(crystals_of(&pool, &user.id).await, 0, "enqueue must not credit");

    assert_eq!(drain_pending_rewards(&pool).await, 1, "first drain credits");
    assert_eq!(crystals_of(&pool, &user.id).await, 20);
    assert_eq!(status_of(&pool, &format!("room-A:{}", user.id)).await.as_deref(), Some("applied"));

    // Re-draining (periodic worker / a second startup) must not double-credit.
    assert_eq!(drain_pending_rewards(&pool).await, 0, "re-drain is a no-op");
    assert_eq!(crystals_of(&pool, &user.id).await, 20);
}

#[tokio::test]
async fn replay_after_crash_before_credit() {
    // Models a crash between match-end (enqueue committed) and the immediate credit:
    // the row survives as `pending`, and the next startup's drain replays it.
    let (pool, _dir) = test_db().await;
    let user = create_user(&pool, "apple-2", "bob").await.unwrap();
    let reward_id = format!("room-B:{}", user.id);

    enqueue_rewards(
        &pool,
        &[OutboxReward { reward_id: reward_id.clone(), user_id: user.id.clone(), crystals: 5 }],
    )
    .await
    .unwrap();
    // (no immediate drain here — simulate the process dying)
    assert_eq!(status_of(&pool, &reward_id).await.as_deref(), Some("pending"));

    // "Restart": startup replay drains pending.
    assert_eq!(drain_pending_rewards(&pool).await, 1);
    assert_eq!(crystals_of(&pool, &user.id).await, 5);
}

#[tokio::test]
async fn duplicate_enqueue_same_match_is_noop() {
    // Same reward_id re-enqueued (e.g. a replayed match outcome) must INSERT OR IGNORE,
    // never overwrite the amount or set up a second credit.
    let (pool, _dir) = test_db().await;
    let user = create_user(&pool, "apple-3", "carol").await.unwrap();
    let reward_id = format!("room-C:{}", user.id);
    let reward = OutboxReward { reward_id: reward_id.clone(), user_id: user.id.clone(), crystals: 20 };

    enqueue_rewards(&pool, std::slice::from_ref(&reward)).await.unwrap();
    assert_eq!(drain_pending_rewards(&pool).await, 1);
    assert_eq!(crystals_of(&pool, &user.id).await, 20);

    // Re-enqueue the SAME id with a different amount; must be ignored (row already exists,
    // now `applied`).
    enqueue_rewards(
        &pool,
        &[OutboxReward { reward_id: reward_id.clone(), user_id: user.id.clone(), crystals: 999 }],
    )
    .await
    .unwrap();
    assert_eq!(drain_pending_rewards(&pool).await, 0, "ignored row stays applied");
    assert_eq!(crystals_of(&pool, &user.id).await, 20, "no double / overwritten credit");
}

#[tokio::test]
async fn batch_credits_all_players_in_one_match() {
    let (pool, _dir) = test_db().await;
    let winner = create_user(&pool, "apple-w", "winner").await.unwrap();
    let loser = create_user(&pool, "apple-l", "loser").await.unwrap();

    enqueue_rewards(
        &pool,
        &[
            OutboxReward {
                reward_id: format!("room-D:{}", winner.id),
                user_id: winner.id.clone(),
                crystals: 20,
            },
            OutboxReward {
                reward_id: format!("room-D:{}", loser.id),
                user_id: loser.id.clone(),
                crystals: 5,
            },
        ],
    )
    .await
    .unwrap();

    assert_eq!(drain_pending_rewards(&pool).await, 2);
    assert_eq!(crystals_of(&pool, &winner.id).await, 20);
    assert_eq!(crystals_of(&pool, &loser.id).await, 5);
}

#[tokio::test]
async fn missing_user_marks_failed_not_lost() {
    // A grant for a user that doesn't exist must be terminal `failed` (surfaced for
    // reconcile) and must NOT keep retrying forever as `pending`.
    let (pool, _dir) = test_db().await;
    let reward_id = "room-E:ghost".to_string();
    enqueue_rewards(
        &pool,
        &[OutboxReward { reward_id: reward_id.clone(), user_id: "ghost".to_string(), crystals: 7 }],
    )
    .await
    .unwrap();

    match apply_pending_reward(&pool, &reward_id).await.unwrap() {
        RewardApply::UserMissing { user_id } => assert_eq!(user_id, "ghost"),
        other => panic!("expected UserMissing, got {:?}", DebugApply(&other)),
    }
    assert_eq!(status_of(&pool, &reward_id).await.as_deref(), Some("failed"));
    assert_eq!(drain_pending_rewards(&pool).await, 0, "failed row is terminal, not retried");
}

#[tokio::test]
async fn concurrent_apply_credits_exactly_once() {
    // The drain runs from three places (startup replay, post-match, periodic worker) that can
    // overlap. The atomic claim (UPDATE ... WHERE status='pending') must let exactly ONE win.
    let (pool, _dir) = test_db().await;
    let user = create_user(&pool, "apple-cc", "concur").await.unwrap();
    let reward_id = format!("room-CC:{}", user.id);
    enqueue_rewards(
        &pool,
        &[OutboxReward { reward_id: reward_id.clone(), user_id: user.id.clone(), crystals: 20 }],
    )
    .await
    .unwrap();

    // Fire several concurrent applies at the same reward_id.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let pool = pool.clone();
        let rid = reward_id.clone();
        handles.push(tokio::spawn(async move { apply_pending_reward(&pool, &rid).await }));
    }
    let mut applied = 0;
    for h in handles {
        if let Ok(Ok(RewardApply::Applied { .. })) = h.await {
            applied += 1;
        }
    }

    assert_eq!(applied, 1, "exactly one concurrent apply may credit");
    assert_eq!(
        crystals_of(&pool, &user.id).await,
        20,
        "concurrent applies credit the grant exactly once, not N times"
    );
}

// RewardApply doesn't derive Debug (it carries no-op variants we don't want to format in
// prod logs); a tiny local wrapper keeps the panic message readable in this one test.
struct DebugApply<'a>(&'a RewardApply);
impl std::fmt::Debug for DebugApply<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            RewardApply::Applied { .. } => write!(f, "Applied"),
            RewardApply::UserMissing { .. } => write!(f, "UserMissing"),
            RewardApply::Skipped => write!(f, "Skipped"),
        }
    }
}
