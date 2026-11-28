//! The Space identity's integration tests: real Homes and a real SQLite record.
//!
//! These were the `#[cfg(test)] mod tests` of `src/identity.rs`. Every one
//! opens a Home, so none stayed a unit test; the internals they need come from
//! `neha_space::test_support::identity`.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use neha_space::home::Home;
use neha_space::identity::{
    Account, Identity, IdentityError, MAX_CODE_TRIES, MAX_LOCAL_TICKETS_PER_OWNER,
    MAX_PENDING_PAIRINGS, MAX_PENDING_PER_SOURCE, PAIRING_TTL_SECS, PairingClaim, PairingRequest,
    PairingState,
};
use neha_space::test_support::identity::{
    CODE_ALPHABET, VERSION_ONE_STATEMENTS, VERSION_TWO_STATEMENTS, digest, pool,
};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, SqliteConnection};

async fn identity() -> (tempfile::TempDir, Home, Identity) {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::open(dir.path()).unwrap();
    let identity = Identity::open(&home).await.unwrap();

    (dir, home, identity)
}

/// Claims and finishes a pairing the way the command does, for tests that
/// only need a bonded Computer.
///
/// The command's real order is reserve, finalize, append; these tests have
/// no record to append, so the two identity steps run together.
trait ClaimPairing {
    async fn claim_pairing(
        &self,
        code: &str,
        owner: Id,
        computer: Id,
    ) -> Result<bool, IdentityError>;
    async fn claim_pairing_at(
        &self,
        now: i64,
        code: &str,
        owner: Id,
        computer: Id,
    ) -> Result<bool, IdentityError>;
}

impl ClaimPairing for Identity {
    async fn claim_pairing(
        &self,
        code: &str,
        owner: Id,
        computer: Id,
    ) -> Result<bool, IdentityError> {
        if !self.reserve_pairing(code, owner, computer).await? {
            return Ok(false);
        }
        self.finalize_pairing(owner, computer).await
    }

    async fn claim_pairing_at(
        &self,
        now: i64,
        code: &str,
        owner: Id,
        computer: Id,
    ) -> Result<bool, IdentityError> {
        if !self
            .test_reserve_pairing_at(now, code, owner, computer)
            .await?
        {
            return Ok(false);
        }
        self.finalize_pairing(owner, computer).await
    }
}

#[tokio::test]
async fn creates_the_admin_and_authenticates_its_token() {
    let (_dir, _home, identity) = identity().await;
    let (account, token) = identity
        .create_admin("Ada", "correct horse battery")
        .await
        .unwrap();

    let authenticated = identity.authenticate(&token).await.unwrap();

    assert!(account.is_admin);
    assert_eq!(authenticated, Some(account));
}

#[tokio::test]
async fn logs_in_with_the_password() {
    let (_dir, _home, identity) = identity().await;
    identity
        .create_admin("Ada", "correct horse battery")
        .await
        .unwrap();

    let (account, token) = identity
        .login("Ada", "correct horse battery")
        .await
        .unwrap();

    assert_eq!(account.display_name, "Ada");
    assert!(identity.authenticate(&token).await.unwrap().is_some());
}

#[tokio::test]
async fn rejects_a_wrong_password() {
    let (_dir, _home, identity) = identity().await;
    identity
        .create_admin("Ada", "correct horse battery")
        .await
        .unwrap();

    let error = identity.login("Ada", "wrong password").await;

    assert!(matches!(error, Err(IdentityError::InvalidCredentials)));
}

#[tokio::test]
async fn rejects_a_second_admin_and_a_taken_name() {
    let (_dir, _home, identity) = identity().await;
    identity
        .create_admin("Ada", "correct horse battery")
        .await
        .unwrap();

    let second = identity
        .create_admin("Grace", "correct horse battery")
        .await;

    assert!(matches!(second, Err(IdentityError::AlreadyInitialized)));
    assert!(identity.has_accounts().await.unwrap());
}

#[tokio::test]
async fn stores_only_the_token_digest() {
    let (_dir, _home, identity) = identity().await;
    let (_account, token) = identity
        .create_admin("Ada", "correct horse battery")
        .await
        .unwrap();

    let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_hash FROM tokens")
        .fetch_all(pool(&identity))
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 32);
    assert_ne!(rows[0], token.as_bytes());
    assert!(
        identity
            .authenticate("not a token")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn validates_the_name_and_password() {
    let (_dir, _home, identity) = identity().await;

    assert!(matches!(
        identity.create_admin("   ", "correct horse battery").await,
        Err(IdentityError::InvalidName)
    ));
    assert!(matches!(
        identity.create_admin("Ada", "short").await,
        Err(IdentityError::WeakPassword)
    ));
}

#[tokio::test]
async fn shows_a_pairing_code_once_and_uses_it_once() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/home/ada/neha", "machine-token", "poll-secret")
        .await
        .unwrap();

    let request = identity.pairing_request(&start.code).await.unwrap();

    assert_eq!(
        request,
        Some(PairingRequest {
            device: start.device.clone(),
            machine: "laptop".to_owned(),
            workspace: "/home/ada/neha".to_owned(),
        })
    );
    assert_eq!(
        identity.pairing_request("ZZZZZZZZ").await.unwrap(),
        None,
        "an unknown code names no pairing"
    );
    let owner = Id::new();
    let computer = Id::new();
    assert!(
        identity
            .claim_pairing(&start.code, owner, computer)
            .await
            .unwrap()
    );
    assert!(
        identity
            .pairing_request(&start.code)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !identity
            .claim_pairing(&start.code, owner, Id::new())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_reserved_pairing_is_pending_until_it_is_finalized() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let owner = Id::new();
    let computer = Id::new();

    assert!(
        identity
            .reserve_pairing(&start.code, owner, computer)
            .await
            .unwrap()
    );

    // The code is consumed and the token row is reserved, but not final:
    // the machine may not act yet.
    assert_eq!(
        identity.pairing(&start.device, "poll").await.unwrap(),
        Some(PairingState::Pending)
    );
    assert_eq!(identity.authenticate_computer("token").await.unwrap(), None);
    let finalized: i64 =
        sqlx::query_scalar("SELECT finalized FROM computer_tokens WHERE computer_id = ?")
            .bind(computer.to_string())
            .fetch_one(pool(&identity))
            .await
            .unwrap();
    assert_eq!(finalized, 0, "the reserve writes the token unfinalized");
    assert_eq!(
        identity.pairing_claims().await.unwrap(),
        vec![PairingClaim {
            owner,
            computer,
            finalized: false,
        }]
    );

    assert!(identity.finalize_pairing(owner, computer).await.unwrap());
    let finalized: i64 =
        sqlx::query_scalar("SELECT finalized FROM computer_tokens WHERE computer_id = ?")
            .bind(computer.to_string())
            .fetch_one(pool(&identity))
            .await
            .unwrap();
    assert_eq!(finalized, 1);
    assert_eq!(
        identity.pairing(&start.device, "poll").await.unwrap(),
        Some(PairingState::Approved { computer })
    );
    assert_eq!(
        identity.authenticate_computer("token").await.unwrap(),
        Some(computer),
        "only the final step admits the machine"
    );
    assert!(
        identity
            .pairing_claims()
            .await
            .unwrap()
            .first()
            .is_some_and(|claim| claim.finalized)
    );
}

#[tokio::test]
async fn releasing_a_reserved_pairing_makes_the_code_usable_again() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let owner = Id::new();
    let computer = Id::new();
    assert!(
        identity
            .reserve_pairing(&start.code, owner, computer)
            .await
            .unwrap()
    );

    assert!(identity.release_pairing(owner, computer).await.unwrap());

    assert_eq!(
        identity.pairing(&start.device, "poll").await.unwrap(),
        Some(PairingState::Pending),
        "a released claim leaves the machine waiting, not approved"
    );
    assert_eq!(identity.authenticate_computer("token").await.unwrap(), None);
    assert!(identity.pairing_claims().await.unwrap().is_empty());
    assert!(
        identity
            .pairing_request(&start.code)
            .await
            .unwrap()
            .is_some(),
        "the User may enter the code again"
    );
    assert!(
        identity
            .reserve_pairing(&start.code, Id::new(), Id::new())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_typed_code_ignores_case_and_surrounding_space() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let typed = format!(" {} ", start.code.to_lowercase());
    let request = identity.pairing_request(&typed).await.unwrap();
    assert!(request.is_some());

    // The code is shown as XXXX-XXXX, so the dash is part of what a User may
    // type; the hash names the eight letters alone.
    let dashed = format!(
        " {}-{} ",
        start.code[..4].to_lowercase(),
        start.code[4..].to_lowercase()
    );
    assert!(
        identity.pairing_request(&dashed).await.unwrap().is_some(),
        "a code typed with its displayed dash must work"
    );
}

#[tokio::test]
async fn a_pairing_code_expires() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    let start = identity
        .test_begin_pairing_at(now, "laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let expires_at = now + PAIRING_TTL_SECS * 1_000_000;

    assert!(
        identity
            .test_pairing_request_at(expires_at - 1, &start.code)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        identity
            .test_pairing_request_at(expires_at, &start.code)
            .await
            .unwrap(),
        None
    );
    assert!(
        !identity
            .claim_pairing_at(expires_at, &start.code, Id::new(), Id::new())
            .await
            .unwrap()
    );
    assert_eq!(
        identity
            .test_pairing_at(expires_at, &start.device, "poll")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn an_approved_pairing_expires_with_its_code() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    let start = identity
        .test_begin_pairing_at(now, "laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let computer = Id::new();
    assert!(
        identity
            .claim_pairing_at(now, &start.code, Id::new(), computer)
            .await
            .unwrap()
    );
    let expires_at = now + PAIRING_TTL_SECS * 1_000_000;
    assert_eq!(
        identity
            .test_pairing_at(expires_at - 1, &start.device, "poll")
            .await
            .unwrap(),
        Some(PairingState::Approved { computer })
    );
    assert_eq!(
        identity
            .test_pairing_at(expires_at, &start.device, "poll")
            .await
            .unwrap(),
        None,
        "an expired pairing is not approved or pending, it is gone"
    );
    assert_eq!(
        identity.authenticate_computer("token").await.unwrap(),
        Some(computer),
        "the Computer and its token outlive the pairing row"
    );
}

#[tokio::test]
async fn a_full_quota_refuses_another_pairing() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    // One source may not fill the queue, so fill it one source at a time.
    let mut starts = Vec::new();
    for index in 0..MAX_PENDING_PAIRINGS {
        starts.push(
            identity
                .test_begin_pairing_from_at(
                    now + index,
                    &format!("source-{index}"),
                    "laptop",
                    "/ws",
                    "token",
                    "poll",
                )
                .await
                .unwrap(),
        );
    }

    let error = identity
        .test_begin_pairing_from_at(
            now + MAX_PENDING_PAIRINGS,
            "one-more-source",
            "laptop",
            "/ws",
            "token",
            "poll",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, IdentityError::TooManyPending),
        "a full queue refuses instead of evicting a waiting User"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_codes")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert_eq!(rows, MAX_PENDING_PAIRINGS, "the store stays bounded");
    assert_eq!(
        identity
            .test_pairing_at(now, &starts[0].device, "poll")
            .await
            .unwrap(),
        Some(PairingState::Pending),
        "a refused start leaves the waiting pairings alone"
    );
}

#[tokio::test]
async fn one_source_cannot_fill_the_waiting_queue() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    for index in 0..MAX_PENDING_PER_SOURCE {
        identity
            .test_begin_pairing_from_at(
                now + index,
                "one-machine",
                "laptop",
                "/ws",
                "token",
                "poll",
            )
            .await
            .unwrap();
    }

    let error = identity
        .test_begin_pairing_from_at(
            now + MAX_PENDING_PER_SOURCE,
            "one-machine",
            "laptop",
            "/ws",
            "token",
            "poll",
        )
        .await
        .unwrap_err();
    assert!(matches!(error, IdentityError::TooManyPending));
    // Another source still has room: the cap is per source, not global yet.
    identity
        .test_begin_pairing_from_at(now, "another-machine", "laptop", "/ws", "token", "poll")
        .await
        .expect("a different source is not affected by the first source's cap");
}

#[tokio::test]
async fn a_claimed_pairing_does_not_take_a_waiting_slot() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    let mut starts = Vec::new();
    for index in 0..MAX_PENDING_PAIRINGS {
        starts.push(
            identity
                .test_begin_pairing_from_at(
                    now + index,
                    &format!("source-{index}"),
                    "laptop",
                    "/ws",
                    "token",
                    "poll",
                )
                .await
                .unwrap(),
        );
    }
    // One pairing is claimed and finished: it leaves the waiting count and
    // must survive a full queue.
    let owner = Id::new();
    let computer = Id::new();
    assert!(
        identity
            .claim_pairing_at(now, &starts[0].code, owner, computer)
            .await
            .unwrap()
    );
    // The queue has room for one more waiting pairing now.
    identity
        .test_begin_pairing_from_at(
            now + MAX_PENDING_PAIRINGS,
            "one-more-source",
            "laptop",
            "/ws",
            "token",
            "poll",
        )
        .await
        .expect("a claimed pairing frees a waiting slot");

    assert_eq!(
        identity.authenticate_computer("token").await.unwrap(),
        Some(computer),
        "a finished claim keeps its token"
    );
    assert_eq!(
        identity.pairing_claims().await.unwrap(),
        vec![PairingClaim {
            owner,
            computer,
            finalized: true,
        }]
    );
}

#[tokio::test]
async fn starting_a_pairing_clears_expired_unclaimed_rows() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    for index in 0..MAX_PENDING_PAIRINGS {
        identity
            .test_begin_pairing_from_at(
                now,
                &format!("source-{index}"),
                "laptop",
                "/ws",
                "token",
                "poll",
            )
            .await
            .unwrap();
    }
    let later = now + PAIRING_TTL_SECS * 1_000_000;

    let start = identity
        .test_begin_pairing_from_at(later, "new-source", "laptop", "/ws", "token", "poll")
        .await
        .expect("expired pairings stop holding the quota");

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_codes")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert_eq!(rows, 1, "expired rows are deleted, the new one is kept");
    assert_eq!(
        identity
            .test_pairing_at(later, &start.device, "poll")
            .await
            .unwrap(),
        Some(PairingState::Pending)
    );
}

#[tokio::test]
async fn expired_cleanup_never_deletes_a_claimed_pairing() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    let start = identity
        .test_begin_pairing_at(now, "laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let owner = Id::new();
    let computer = Id::new();
    assert!(
        identity
            .test_reserve_pairing_at(now, &start.code, owner, computer)
            .await
            .unwrap()
    );
    let later = now + PAIRING_TTL_SECS * 1_000_000;
    identity
        .test_begin_pairing_at(later, "laptop", "/ws", "token", "poll")
        .await
        .unwrap();

    // The claim's code row expired, but only the start-up reconciliation
    // may move a claim, so it is still there to be finished.
    assert_eq!(
        identity.pairing_claims().await.unwrap(),
        vec![PairingClaim {
            owner,
            computer,
            finalized: false,
        }]
    );
    assert!(
        identity.finalize_pairing(owner, computer).await.unwrap(),
        "a claimed row outlives its code's expiry"
    );
    assert_eq!(
        identity.authenticate_computer("token").await.unwrap(),
        Some(computer)
    );
}

#[tokio::test]
async fn a_token_held_by_another_computer_cannot_be_reserved() {
    let (_dir, _home, identity) = identity().await;
    let first = identity
        .begin_pairing("laptop", "/ws", "shared-token", "poll")
        .await
        .unwrap();
    let first_computer = Id::new();
    assert!(
        identity
            .claim_pairing(&first.code, Id::new(), first_computer)
            .await
            .unwrap()
    );

    let second = identity
        .begin_pairing("desktop", "/ws", "shared-token", "poll")
        .await
        .unwrap();
    let owner = Id::new();
    let second_computer = Id::new();
    assert!(
        matches!(
            identity
                .reserve_pairing(&second.code, owner, second_computer)
                .await,
            Err(IdentityError::TokenTaken)
        ),
        "the token's primary key reserves it for one Computer only"
    );
    // A failed reserve rolls the claim back: the second code stays usable
    // and no half-claim is left behind.
    assert_eq!(
        identity.pairing(&second.device, "poll").await.unwrap(),
        Some(PairingState::Pending)
    );
    let secondary: Option<String> =
        sqlx::query_scalar("SELECT owner FROM computer_codes WHERE device_id = ?")
            .bind(&second.device)
            .fetch_one(pool(&identity))
            .await
            .unwrap();
    assert_eq!(secondary, None, "the losing reserve leaves no claim");
    assert_eq!(
        identity
            .authenticate_computer("shared-token")
            .await
            .unwrap(),
        Some(first_computer),
        "the first Computer keeps its token"
    );
}

#[tokio::test]
async fn a_reserved_token_cannot_be_reserved_by_another_claim() {
    let (_dir, _home, identity) = identity().await;
    let first = identity
        .begin_pairing("laptop", "/ws", "token", "poll-one")
        .await
        .unwrap();
    let second = identity
        .begin_pairing("desktop", "/ws", "token", "poll-two")
        .await
        .unwrap();
    let owner = Id::new();
    let computer = Id::new();
    assert!(
        identity
            .reserve_pairing(&first.code, owner, computer)
            .await
            .unwrap()
    );

    let other_owner = Id::new();
    let other_computer = Id::new();
    assert!(
        matches!(
            identity
                .reserve_pairing(&second.code, other_owner, other_computer)
                .await,
            Err(IdentityError::TokenTaken)
        ),
        "an unfinalized reservation already owns the token"
    );
    assert_eq!(
        identity.pairing_claims().await.unwrap(),
        vec![PairingClaim {
            owner,
            computer,
            finalized: false,
        }]
    );
}

#[tokio::test]
async fn finishing_a_cleaned_up_claim_reports_that_it_is_gone() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    let start = identity
        .test_begin_pairing_at(now, "laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let owner = Id::new();
    let computer = Id::new();
    assert!(
        identity
            .test_reserve_pairing_at(now, &start.code, owner, computer)
            .await
            .unwrap()
    );
    sqlx::query("DELETE FROM computer_codes WHERE owner = ?")
        .bind(owner.to_string())
        .execute(pool(&identity))
        .await
        .unwrap();

    assert!(
        !identity.finalize_pairing(owner, computer).await.unwrap(),
        "a claim whose row is gone cannot report success"
    );
    assert_eq!(identity.authenticate_computer("token").await.unwrap(), None);
}

#[tokio::test]
async fn finishing_a_claim_twice_is_idempotent() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let owner = Id::new();
    let computer = Id::new();
    assert!(
        identity
            .reserve_pairing(&start.code, owner, computer)
            .await
            .unwrap()
    );
    assert!(identity.finalize_pairing(owner, computer).await.unwrap());
    assert!(identity.finalize_pairing(owner, computer).await.unwrap());
    assert_eq!(
        identity.authenticate_computer("token").await.unwrap(),
        Some(computer)
    );
}

#[tokio::test]
async fn two_machines_starting_at_once_keep_the_store_bounded() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    for index in 0..MAX_PENDING_PAIRINGS - 1 {
        identity
            .test_begin_pairing_from_at(
                now,
                &format!("source-{index}"),
                "laptop",
                "/ws",
                "token",
                "poll",
            )
            .await
            .unwrap();
    }

    let (first, second) = tokio::join!(
        identity.test_begin_pairing_from_at(now, "one", "one", "/ws", "token", "poll"),
        identity.test_begin_pairing_from_at(now, "two", "two", "/ws", "token", "poll"),
    );
    let accepted = usize::from(first.is_ok()) + usize::from(second.is_ok());
    assert_eq!(
        accepted, 1,
        "one start fills the last slot and the other is refused: {first:?} {second:?}"
    );
    let refused = if first.is_err() { first } else { second };
    assert!(
        matches!(refused, Err(IdentityError::TooManyPending)),
        "the queue refuses instead of growing: {refused:?}"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_codes")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert_eq!(rows, MAX_PENDING_PAIRINGS);
}

#[tokio::test]
async fn a_pairing_code_stops_working_after_its_tries() {
    let (_dir, _home, identity) = identity().await;
    let now = 1_000_000_i64;
    let start = identity
        .test_begin_pairing_at(now, "laptop", "/ws", "token", "poll")
        .await
        .unwrap();

    for _ in 0..MAX_CODE_TRIES {
        assert!(
            identity
                .test_pairing_request_at(now, &start.code)
                .await
                .unwrap()
                .is_some()
        );
    }

    assert!(
        identity
            .test_pairing_request_at(now, &start.code)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !identity
            .claim_pairing_at(now, &start.code, Id::new(), Id::new())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pairing_stays_pending_until_the_user_approves_it() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/ws", "token", "poll")
        .await
        .unwrap();

    assert_eq!(
        identity.pairing(&start.device, "poll").await.unwrap(),
        Some(PairingState::Pending)
    );
    assert_eq!(
        identity.pairing(&start.device, "wrong").await.unwrap(),
        None
    );
    assert_eq!(
        identity.pairing("unknown-device", "poll").await.unwrap(),
        None
    );

    let computer = Id::new();
    assert!(
        identity
            .claim_pairing(&start.code, Id::new(), computer)
            .await
            .unwrap()
    );
    assert_eq!(
        identity.pairing(&start.device, "poll").await.unwrap(),
        Some(PairingState::Approved { computer })
    );
}

#[tokio::test]
async fn only_one_of_two_concurrent_claims_wins() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/ws", "token", "poll")
        .await
        .unwrap();
    let ada = Id::new();
    let grace = Id::new();
    let first = Id::new();
    let second = Id::new();

    let (one, two) = tokio::join!(
        identity.claim_pairing(&start.code, ada, first),
        identity.claim_pairing(&start.code, grace, second),
    );
    let one = one.unwrap();
    let two = two.unwrap();

    assert_ne!(one, two, "one claim must lose");
    let (winner_owner, winner) = if one { (ada, first) } else { (grace, second) };
    assert_eq!(
        identity.pairing(&start.device, "poll").await.unwrap(),
        Some(PairingState::Approved { computer: winner })
    );
    assert_eq!(
        identity.pairing_claims().await.unwrap(),
        vec![PairingClaim {
            owner: winner_owner,
            computer: winner,
            finalized: true,
        }],
        "the code admits one User, not both"
    );
    assert_eq!(
        identity.authenticate_computer("token").await.unwrap(),
        Some(winner)
    );
}

#[tokio::test]
async fn the_pairing_tables_store_only_hashes() {
    let (_dir, _home, identity) = identity().await;
    let token = "machine-token-plaintext";
    let poll_secret = "poll-secret-plaintext";
    let start = identity
        .begin_pairing("laptop", "/ws", token, poll_secret)
        .await
        .unwrap();
    assert!(
        identity
            .claim_pairing(&start.code, Id::new(), Id::new())
            .await
            .unwrap()
    );

    let code_hash: Vec<u8> = sqlx::query_scalar("SELECT code_hash FROM computer_codes")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    let poll_hash: Vec<u8> = sqlx::query_scalar("SELECT poll_hash FROM computer_codes")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    let token_hash: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM computer_codes")
        .fetch_one(pool(&identity))
        .await
        .unwrap();

    assert_eq!(code_hash, digest(&start.code));
    assert_eq!(poll_hash, digest(poll_secret));
    assert_eq!(token_hash, digest(token));
    for hash in [&code_hash, &poll_hash, &token_hash] {
        assert_eq!(hash.len(), 32);
    }
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM computer_tokens")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert_eq!(stored, digest(token));
    assert_ne!(stored, token.as_bytes());
}

#[tokio::test]
async fn a_computer_authenticates_with_the_token_the_machine_keeps() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/ws", "machine-token", "poll")
        .await
        .unwrap();
    let computer = Id::new();
    assert!(
        identity
            .claim_pairing(&start.code, Id::new(), computer)
            .await
            .unwrap()
    );

    assert_eq!(
        identity
            .authenticate_computer("machine-token")
            .await
            .unwrap(),
        Some(computer)
    );
    assert_eq!(
        identity.authenticate_computer("not-a-token").await.unwrap(),
        None
    );
    let (_account, account_token) = identity
        .create_admin("Ada", "correct horse battery")
        .await
        .unwrap();
    assert_eq!(
        identity
            .authenticate_computer(&account_token)
            .await
            .unwrap(),
        None,
        "an account token is not a Computer token"
    );
}

#[tokio::test]
async fn pairing_codes_and_devices_have_the_expected_shape() {
    let (_dir, _home, identity) = identity().await;
    let start = identity
        .begin_pairing("laptop", "/ws", "token", "poll")
        .await
        .unwrap();

    assert_eq!(CODE_ALPHABET.len(), 32);
    assert_eq!(start.code.len(), 8);
    assert!(start.code.bytes().all(|byte| CODE_ALPHABET.contains(&byte)));
    assert_eq!(URL_SAFE_NO_PAD.decode(&start.device).unwrap().len(), 16);
}

async fn version_one_database(path: &std::path::Path, token: &str) -> Account {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .busy_timeout(Duration::from_secs(5));
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    for statement in VERSION_ONE_STATEMENTS {
        sqlx::query(statement)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::query("PRAGMA user_version = 1")
        .execute(&mut connection)
        .await
        .unwrap();
    let account = Account {
        id: Id::new(),
        display_name: "Legacy Ada".to_owned(),
        is_admin: true,
    };
    sqlx::query(
        "INSERT INTO accounts (id, display_name, password_hash, is_admin, created_at) \
             VALUES (?, ?, 'unused', 1, 0)",
    )
    .bind(account.id.to_string())
    .bind(&account.display_name)
    .execute(&mut connection)
    .await
    .unwrap();
    let token_hash = digest(token);
    sqlx::query("INSERT INTO tokens (token_hash, account_id, created_at) VALUES (?, ?, 0)")
        .bind(token_hash.as_slice())
        .bind(account.id.to_string())
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();

    account
}

#[tokio::test]
async fn opening_a_version_one_database_keeps_accounts_and_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::open(dir.path()).unwrap();
    let account = version_one_database(&home.db_path(), "legacy-token").await;

    let identity = Identity::open(&home).await.unwrap();

    assert!(identity.has_accounts().await.unwrap());
    assert_eq!(
        identity.authenticate("legacy-token").await.unwrap(),
        Some(account)
    );
}

#[tokio::test]
async fn opening_a_home_renames_and_upgrades_the_legacy_database() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("db").join("neha-1.sqlite3");
    let account = version_one_database(&legacy, "legacy-token").await;

    let home = Home::open(dir.path()).unwrap();
    let identity = Identity::open(&home).await.unwrap();

    assert!(!legacy.exists(), "the legacy file must be renamed");
    assert!(home.db_path().exists());
    assert_eq!(
        identity.authenticate("legacy-token").await.unwrap(),
        Some(account)
    );
}

async fn version_two_database(path: &std::path::Path, token: &str, computer: Id) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .busy_timeout(Duration::from_secs(5));
    let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
    for statement in VERSION_ONE_STATEMENTS {
        sqlx::query(statement)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    for statement in VERSION_TWO_STATEMENTS {
        sqlx::query(statement)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    sqlx::query("PRAGMA user_version = 2")
        .execute(&mut connection)
        .await
        .unwrap();
    let token_hash = digest(token);
    sqlx::query(
        "INSERT INTO computer_tokens (token_hash, computer_id, created_at) \
             VALUES (?, ?, 0)",
    )
    .bind(token_hash.as_slice())
    .bind(computer.to_string())
    .execute(&mut connection)
    .await
    .unwrap();
    connection.close().await.unwrap();
}

#[tokio::test]
async fn opening_a_version_two_database_keeps_paired_computers_active() {
    let dir = tempfile::tempdir().unwrap();
    let home = Home::open(dir.path()).unwrap();
    let computer = Id::new();
    version_two_database(&home.db_path(), "paired-token", computer).await;

    let identity = Identity::open(&home).await.unwrap();

    assert_eq!(
        identity
            .authenticate_computer("paired-token")
            .await
            .unwrap(),
        Some(computer),
        "a token written before schema version 3 still authenticates"
    );
    let finalized: i64 = sqlx::query_scalar("SELECT finalized FROM computer_tokens")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert_eq!(finalized, 1, "the migration marks old tokens final");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_writers_wait_for_the_database_instead_of_erroring() {
    let (_dir, _home, identity) = identity().await;

    // The pool holds two connections, and eight writers start at once. A
    // slow disk without a busy timeout turns that into SQLITE_BUSY
    // ("database is locked") instead of a wait; this is the shape CI caught
    // in the claim race.
    for round in 0..12_i64 {
        let now = 1_000_000 + round * 1_000;
        let machines: Vec<String> = (0..8).map(|index| format!("machine-{index}")).collect();
        let tokens: Vec<String> = (0..8)
            .map(|index| format!("token-{round}-{index}"))
            .collect();
        let polls: Vec<String> = (0..8)
            .map(|index| format!("poll-{round}-{index}"))
            .collect();
        let sources: Vec<String> = (0..8)
            .map(|index| format!("source-{round}-{index}"))
            .collect();
        let starts = futures_util::future::join_all((0..8).map(|index| {
            identity.test_begin_pairing_from_at(
                now,
                &sources[index],
                &machines[index],
                "/ws",
                &tokens[index],
                &polls[index],
            )
        }))
        .await;
        let starts: Vec<_> = starts
            .into_iter()
            .map(|start| start.expect("a concurrent start must wait, not fail"))
            .collect();

        let owners: Vec<Id> = (0..8).map(|_| Id::new()).collect();
        let computers: Vec<Id> = (0..8).map(|_| Id::new()).collect();
        let reservations =
            futures_util::future::join_all(starts.iter().zip(&owners).zip(&computers).map(
                |((start, owner), computer)| {
                    identity.test_reserve_pairing_at(now, &start.code, *owner, *computer)
                },
            ))
            .await;
        for reservation in reservations {
            assert!(
                reservation.expect("a concurrent reserve must wait, not fail"),
                "every owner gets its own code"
            );
        }
        for (owner, computer) in owners.iter().zip(&computers) {
            assert!(
                identity
                    .release_pairing(*owner, *computer)
                    .await
                    .expect("a concurrent release must wait, not fail")
            );
        }
        // Released rows become waiting again; clear the queue so the next
        // round exercises the same six writers, not the global cap.
        sqlx::query("DELETE FROM computer_codes")
            .execute(pool(&identity))
            .await
            .unwrap();
    }

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_codes")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert!(rows <= MAX_PENDING_PAIRINGS, "the store stayed bounded");
    assert!(
        identity.pairing_claims().await.unwrap().is_empty(),
        "released claims leave nothing behind"
    );
}
#[tokio::test]
async fn a_local_ticket_is_stored_as_a_digest_and_works_once() {
    let (_dir, _home, identity) = identity().await;
    let owner = Id::new();
    identity
        .remember_local_ticket("ticket-one", owner, Duration::from_secs(600))
        .await
        .unwrap();

    let hashes: Vec<Vec<u8>> = sqlx::query_scalar("SELECT ticket_hash FROM computer_tickets")
        .fetch_all(pool(&identity))
        .await
        .unwrap();
    assert_eq!(hashes.len(), 1);
    assert_eq!(hashes[0].len(), 32, "the database keeps a digest");
    assert!(
        !hashes[0]
            .windows(b"ticket-one".len())
            .any(|window| window == b"ticket-one"),
        "the plaintext never reaches the database"
    );

    assert_eq!(
        identity
            .claim_local_ticket("ticket-one")
            .await
            .unwrap()
            .map(|(owner, _hash)| owner),
        Some(owner)
    );
    assert_eq!(
        identity.claim_local_ticket("ticket-one").await.unwrap(),
        None,
        "a ticket works once"
    );
}

#[tokio::test]
async fn an_expired_local_ticket_is_not_claimable() {
    let (_dir, _home, identity) = identity().await;
    identity
        .remember_local_ticket("stale", Id::new(), Duration::ZERO)
        .await
        .unwrap();

    assert_eq!(identity.claim_local_ticket("stale").await.unwrap(), None);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_tickets")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert_eq!(rows, 0, "an expired ticket is deleted when it is tried");
}

#[tokio::test]
async fn clearing_tickets_forgets_them_and_a_recorded_token_authenticates() {
    let (_dir, _home, identity) = identity().await;
    let computer = Id::new();
    identity
        .remember_local_ticket("ticket", Id::new(), Duration::from_secs(600))
        .await
        .unwrap();
    identity.clear_local_tickets().await.unwrap();
    assert_eq!(identity.claim_local_ticket("ticket").await.unwrap(), None);

    identity
        .record_computer_token(computer, "credential")
        .await
        .unwrap();
    assert_eq!(
        identity.authenticate_computer("credential").await.unwrap(),
        Some(computer)
    );
    identity.forget_computer_token("credential").await.unwrap();
    assert_eq!(
        identity.authenticate_computer("credential").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn the_admin_is_known_once_it_joins() {
    let (_dir, _home, identity) = identity().await;
    assert_eq!(identity.admin_account().await.unwrap(), None);

    let (account, _token) = identity
        .create_admin("Ada", "correct horse battery")
        .await
        .unwrap();

    assert_eq!(identity.admin_account().await.unwrap(), Some(account));
}
#[tokio::test]
async fn issuing_keeps_at_most_eight_unused_tickets_per_owner() {
    let (_dir, _home, identity) = identity().await;
    let owner = Id::new();
    let mut tickets = Vec::new();
    for number in 0..(MAX_LOCAL_TICKETS_PER_OWNER + 2) {
        let ticket = format!("ticket-{number}");
        identity
            .remember_local_ticket(&ticket, owner, Duration::from_secs(600))
            .await
            .unwrap();
        tickets.push(ticket);
    }

    let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_tickets WHERE owner = ?")
        .bind(owner.to_string().into_bytes())
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert_eq!(held, MAX_LOCAL_TICKETS_PER_OWNER);
    assert_eq!(
        identity.claim_local_ticket(&tickets[0]).await.unwrap(),
        None,
        "the oldest unused ticket is dropped"
    );
    assert!(
        identity
            .claim_local_ticket(tickets.last().unwrap())
            .await
            .unwrap()
            .is_some(),
        "the newest ticket works"
    );
}

#[tokio::test]
async fn issuing_and_claiming_drop_expired_tickets() {
    let (_dir, _home, identity) = identity().await;
    let owner = Id::new();
    sqlx::query(
        "INSERT INTO computer_tickets (ticket_hash, owner, created_at, expires_at) \
             VALUES (?, ?, 0, 0)",
    )
    .bind(digest("stale-ticket").as_slice())
    .bind(owner.to_string().into_bytes())
    .execute(pool(&identity))
    .await
    .unwrap();

    identity
        .remember_local_ticket("fresh-ticket", owner, Duration::from_secs(600))
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM computer_tickets")
        .fetch_one(pool(&identity))
        .await
        .unwrap();
    assert_eq!(rows, 1, "an issue drops what already expired");
    assert_eq!(
        identity.claim_local_ticket("stale-ticket").await.unwrap(),
        None
    );
}

#[tokio::test]
async fn revoking_forgets_claims_and_waiting_rows_for_its_token() {
    let (_dir, _home, identity) = identity().await;
    let first = identity
        .begin_pairing("laptop", "/ws", "shared-token", "poll-one")
        .await
        .unwrap();
    let owner = Id::new();
    let computer = Id::new();
    assert!(
        identity
            .reserve_pairing(&first.code, owner, computer)
            .await
            .unwrap()
    );
    assert!(identity.finalize_pairing(owner, computer).await.unwrap());

    // The same machine starts another pairing with the token it already
    // holds; approving it after revocation must not restore the old token.
    let waiting = identity
        .begin_pairing("laptop", "/ws", "shared-token", "poll-two")
        .await
        .unwrap();

    assert!(identity.revoke_computer(computer).await.unwrap());
    assert_eq!(
        identity
            .authenticate_computer("shared-token")
            .await
            .unwrap(),
        None
    );
    assert_eq!(identity.pairing_claims().await.unwrap(), Vec::new());
    assert_eq!(
        identity.pairing_request(&waiting.code).await.unwrap(),
        None,
        "a waiting row carrying the revoked token is gone"
    );
    assert!(
        !identity.revoke_computer(computer).await.unwrap(),
        "a second revoke is idempotent"
    );
}
