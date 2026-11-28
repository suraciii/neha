//! M7 multi-user: invitations, roles, and the Owner transfer.
//!
//! These tests exercise the real HTTP join, the real command surface, and the
//! real start-up repair, with the helpers kept local so the milestone's tests
//! read as one story.

use futures_util::{SinkExt as _, StreamExt as _};
use neha::error::ErrorCode;
use neha::id::Id;
use neha::record::EventKind;
use neha::role::Role;
use neha::surface::{ClientFrame, CommandCall, PROTOCOL_VERSION, ServerFrame};
use neha_space::test_support::server::open_home_with_ticket_directory;
use neha_space::{Home, Running, start};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const PASSWORD: &str = "correct horse battery";

async fn running() -> (tempfile::TempDir, Running, String) {
    let dir = tempfile::tempdir().unwrap();
    let home = test_home(dir.path());
    let running = start(home, 0).await.unwrap();
    let invite = running.invite.clone().expect("a fresh Space invites");

    (dir, running, invite)
}

fn test_home(root: &std::path::Path) -> Home {
    open_home_with_ticket_directory(root, root.join("config")).unwrap()
}

fn secret_of(invite: &str) -> String {
    invite.split_once('#').unwrap().1.to_owned()
}

/// Joins through the real endpoint and returns the session body.
async fn join(running: &Running, secret: &str, name: &str) -> (Value, reqwest::StatusCode) {
    let response = reqwest::Client::new()
        .post(format!("{}/api/join", running.url()))
        .json(&json!({"secret": secret, "display_name": name, "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    let status = response.status();
    (response.json().await.unwrap(), status)
}

/// Returns the account `/api/me` answers with for one token.
async fn me(running: &Running, token: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{}/api/me", running.url()))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn open_socket(running: &Running, token: &str) -> Socket {
    let mut request = format!("ws://{}/api/socket", running.addr)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Hello {
                version: PROTOCOL_VERSION,
                capabilities: Vec::new(),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let _hello = next_frame(&mut socket).await;
    socket
}

async fn next_frame(socket: &mut Socket) -> ServerFrame {
    loop {
        let message = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("a frame within ten seconds")
            .unwrap()
            .unwrap();
        let Message::Text(text) = message else {
            continue;
        };
        return serde_json::from_str(text.as_str()).unwrap();
    }
}

async fn try_command(
    socket: &mut Socket,
    command: CommandCall,
) -> Result<Value, (ErrorCode, String)> {
    try_command_with_id(socket, Id::new(), command).await
}

async fn try_command_with_id(
    socket: &mut Socket,
    id: Id,
    command: CommandCall,
) -> Result<Value, (ErrorCode, String)> {
    socket
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Command { id, command })
                .unwrap()
                .into(),
        ))
        .await
        .unwrap();
    loop {
        match next_frame(socket).await {
            ServerFrame::Accepted { .. } | ServerFrame::Committed { .. } => {}
            ServerFrame::Confirmed { result, .. } => return Ok(result),
            ServerFrame::Error { code, message, .. } => return Err((code, message)),
            ServerFrame::Heartbeat { .. }
            | ServerFrame::Event { .. }
            | ServerFrame::Snapshot { .. } => {}
            other => panic!("unexpected frame {other:?}"),
        }
    }
}

async fn command(socket: &mut Socket, command: CommandCall) -> Value {
    try_command(socket, command).await.unwrap()
}

/// Joins the first User of a fresh Space and returns the Owner's socket.
async fn owner_socket(running: &Running, invite: &str) -> (Socket, Id, String) {
    let (session, status) = join(running, &secret_of(invite), "Ada").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(session["account"]["role"], "owner");
    let token = session["token"].as_str().unwrap().to_owned();
    let id = session["account"]["id"].as_str().unwrap().parse().unwrap();
    let socket = open_socket(running, &token).await;

    (socket, id, token)
}

/// Makes an invitation as the Owner and returns its secret.
async fn make_invite(socket: &mut Socket) -> String {
    let answer = command(socket, CommandCall::InvitationCreate).await;
    answer["secret"].as_str().unwrap().to_owned()
}

/// Joins an invited Member and returns the Member's socket, id, and token.
async fn member_socket(running: &Running, secret: &str, name: &str) -> (Socket, Id, String) {
    let (session, status) = join(running, secret, name).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{session}");
    assert_eq!(session["account"]["role"], "member");
    let token = session["token"].as_str().unwrap().to_owned();
    let id = session["account"]["id"].as_str().unwrap().parse().unwrap();
    let socket = open_socket(running, &token).await;

    (socket, id, token)
}

fn users(result: &Value) -> Vec<Value> {
    result.as_array().expect("a User list").to_vec()
}

fn role_of(users: &[Value], name: &str) -> Option<String> {
    users
        .iter()
        .find(|user| user["display_name"].as_str() == Some(name))
        .and_then(|user| user["role"].as_str().map(str::to_owned))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invitation_admits_one_member() {
    let (_dir, running, invite) = running().await;
    let (mut owner, owner_id, _owner_token) = owner_socket(&running, &invite).await;

    let created = command(&mut owner, CommandCall::InvitationCreate).await;
    let secret = created["secret"].as_str().unwrap().to_owned();
    assert!(created["invitation"]["id"].is_string());
    assert!(created["invitation"]["expires_at"].is_string());

    let pending = command(&mut owner, CommandCall::InvitationList).await;
    assert_eq!(pending.as_array().unwrap().len(), 1);
    assert_eq!(pending[0]["id"], created["invitation"]["id"]);

    let (mut member, member_id, _member_token) = member_socket(&running, &secret, "Eve").await;
    assert_ne!(owner_id, member_id);

    let list = users(&command(&mut owner, CommandCall::UserList).await);
    assert_eq!(list.len(), 2);
    assert_eq!(role_of(&list, "Ada").as_deref(), Some("owner"));
    assert_eq!(role_of(&list, "Eve").as_deref(), Some("member"));

    // The link admits one User, so a second attempt earns no account.
    let (reused, status) = join(&running, &secret, "Mallory").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{reused}");
    let list = users(&command(&mut owner, CommandCall::UserList).await);
    assert_eq!(list.len(), 2);

    // The pending list is now empty: the link is used up.
    let pending = command(&mut owner, CommandCall::InvitationList).await;
    assert_eq!(pending.as_array().unwrap().len(), 0);

    // The invited Member cannot invite anyone or read the pending list.
    assert_eq!(
        try_command(&mut member, CommandCall::InvitationCreate)
            .await
            .unwrap_err()
            .0,
        ErrorCode::Forbidden
    );
    assert_eq!(
        try_command(&mut member, CommandCall::InvitationList)
            .await
            .unwrap_err()
            .0,
        ErrorCode::Forbidden
    );

    owner.close(None).await.unwrap();
    member.close(None).await.unwrap();
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_changes_roles() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, _owner_token) = owner_socket(&running, &invite).await;
    let secret = make_invite(&mut owner).await;
    let (mut member, member_id, _member_token) = member_socket(&running, &secret, "Eve").await;

    // A Member may read the Users but not change a role.
    let list = users(&command(&mut member, CommandCall::UserList).await);
    assert_eq!(list.len(), 2);
    assert_eq!(
        try_command(
            &mut member,
            CommandCall::UserSetRole {
                user: member_id,
                role: Role::Admin,
            },
        )
        .await
        .unwrap_err()
        .0,
        ErrorCode::Forbidden
    );

    // The Owner makes the Member an Admin.
    let changed = command(
        &mut owner,
        CommandCall::UserSetRole {
            user: member_id,
            role: Role::Admin,
        },
    )
    .await;
    let list = users(&changed["users"]);
    assert_eq!(role_of(&list, "Eve").as_deref(), Some("admin"));
    assert_eq!(me(&running, &_member_token).await["role"], "admin");

    // An Admin invites and revokes; the Owner may not give the Owner role
    // through a role change.
    let created = command(&mut member, CommandCall::InvitationCreate).await;
    command(&mut member, CommandCall::InvitationList).await;
    let revoked = command(
        &mut owner,
        CommandCall::InvitationRevoke {
            invitation: created["invitation"]["id"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        },
    )
    .await;
    assert_eq!(revoked["revoked"], true);
    assert_eq!(
        try_command(
            &mut owner,
            CommandCall::UserSetRole {
                user: member_id,
                role: Role::Owner,
            },
        )
        .await
        .unwrap_err()
        .0,
        ErrorCode::InvalidArgument
    );

    // Demoting the Admin takes effect for the socket that is already open:
    // the record decides the role, not the account row.
    command(
        &mut owner,
        CommandCall::UserSetRole {
            user: member_id,
            role: Role::Member,
        },
    )
    .await;
    assert_eq!(me(&running, &_member_token).await["role"], "member");
    assert_eq!(
        try_command(&mut member, CommandCall::InvitationCreate)
            .await
            .unwrap_err()
            .0,
        ErrorCode::Forbidden
    );

    owner.close(None).await.unwrap();
    member.close(None).await.unwrap();
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_transfers_the_space() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, owner_token) = owner_socket(&running, &invite).await;
    let secret = make_invite(&mut owner).await;
    let (mut member, member_id, member_token) = member_socket(&running, &secret, "Eve").await;

    let transferred = command(&mut owner, CommandCall::UserTransfer { user: member_id }).await;
    let list = users(&transferred["users"]);
    assert_eq!(role_of(&list, "Ada").as_deref(), Some("admin"));
    assert_eq!(role_of(&list, "Eve").as_deref(), Some("owner"));
    assert_eq!(me(&running, &owner_token).await["role"], "admin");
    assert_eq!(me(&running, &member_token).await["role"], "owner");

    // The outgoing Owner is an Admin now: it may invite but not transfer back.
    make_invite(&mut owner).await;
    assert_eq!(
        try_command(&mut owner, CommandCall::UserTransfer { user: member_id })
            .await
            .unwrap_err()
            .0,
        ErrorCode::Forbidden
    );

    // The new Owner may transfer the Space back.
    let transferred = command(&mut member, CommandCall::UserTransfer { user: _owner_id }).await;
    let list = users(&transferred["users"]);
    assert_eq!(role_of(&list, "Ada").as_deref(), Some("owner"));
    assert_eq!(role_of(&list, "Eve").as_deref(), Some("admin"));

    owner.close(None).await.unwrap();
    member.close(None).await.unwrap();
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_or_revoked_invitation_admits_no_one() {
    let (_dir, running, invite) = running().await;
    let (mut owner, owner_id, _owner_token) = owner_socket(&running, &invite).await;

    let created = command(&mut owner, CommandCall::InvitationCreate).await;
    let invitation: Id = created["invitation"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    command(&mut owner, CommandCall::InvitationRevoke { invitation }).await;
    let pending = command(&mut owner, CommandCall::InvitationList).await;
    assert_eq!(pending.as_array().unwrap().len(), 0);
    let revoked_secret = created["secret"].as_str().unwrap();
    let (_body, status) = join(&running, revoked_secret, "Mallory").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    // A new link replaces the pending one: the older secret stops working.
    let first = command(&mut owner, CommandCall::InvitationCreate).await;
    let first_secret = first["secret"].as_str().unwrap().to_owned();
    let second = command(&mut owner, CommandCall::InvitationCreate).await;
    let second_secret = second["secret"].as_str().unwrap().to_owned();
    let (_body, status) = join(&running, &first_secret, "Mallory").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    let (member, status) = join(&running, &second_secret, "Eve").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(member["account"]["role"], "member");

    // An invitation the Space made before the cut-off admits no one.
    let (expired, expired_secret) = {
        let fields = running.test_fields();
        fields
            .identity
            .test_create_invitation_at(
                Id::new(),
                neha::time::Timestamp::now()
                    .as_microseconds()
                    .saturating_sub(neha_space::identity::INVITATION_TTL_SECS * 2 * 1_000_000),
                owner_id,
            )
            .await
            .unwrap()
    };
    let _ = expired;
    let (_body, status) = join(&running, &expired_secret.unwrap(), "Mallory").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    let pending = command(&mut owner, CommandCall::InvitationList).await;
    assert_eq!(pending.as_array().unwrap().len(), 0);

    owner.close(None).await.unwrap();
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_drops_a_half_finished_join_and_keeps_a_finished_one() {
    let dir = tempfile::tempdir().unwrap();
    let home = test_home(dir.path());
    let first = start(home.clone(), 0).await.unwrap();
    let invite = first.invite.clone().unwrap();
    let (_owner, _owner_id, _owner_token) = owner_socket(&first, &invite).await;

    // A crash between the account write and the record append: the account
    // never became a User, and its invitation claim is released.
    let orphan_secret = {
        let fields = first.test_fields();
        let owner = fields
            .identity
            .owner_account()
            .await
            .unwrap()
            .expect("the Owner exists");
        let ghost = fields
            .identity
            .test_add_account_for_test("Ghost", PASSWORD)
            .await
            .unwrap();
        let (_invitation, secret) = fields
            .identity
            .create_invitation(Id::new(), owner.id)
            .await
            .unwrap();
        let secret = secret.expect("a fresh invitation shows its secret");
        assert!(
            fields
                .identity
                .reserve_invitation(&secret, ghost.id)
                .await
                .unwrap()
        );
        secret
    };
    first.shutdown().await;

    let second = start(home.clone(), 0).await.unwrap();
    // The orphan account is gone; the released link admits one User again.
    let (session, status) = join(&second, &orphan_secret, "Ghost").await;
    assert_eq!(status, reqwest::StatusCode::OK, "{session}");
    assert_eq!(session["account"]["role"], "member");
    second.shutdown().await;

    // A crash after the record append but before the invitation finished: the
    // User exists, and the pending row is gone rather than claimed forever.
    let third = start(home.clone(), 0).await.unwrap();
    let owner = {
        let fields = third.test_fields();
        fields.identity.owner_account().await.unwrap().unwrap()
    };
    let mut owner_socket = open_socket(&third, &_owner_token).await;
    let created = command(&mut owner_socket, CommandCall::InvitationCreate).await;
    let secret = created["secret"].as_str().unwrap().to_owned();
    let ghost = {
        let fields = third.test_fields();
        let id = Id::new();
        assert!(
            fields
                .identity
                .reserve_invitation(&secret, id)
                .await
                .unwrap()
        );
        fields
            .identity
            .create_invited(id, "Grace", PASSWORD)
            .await
            .unwrap();
        // The join's record append, without the claim's finish.
        fields
            .store
            .append(neha_space::store::AppendRequest::new(
                Id::new(),
                format!("join:{id}"),
                vec![neha::record::EventDraft::new(
                    neha::record::EventKind::UserJoined,
                    neha::record::Author::User(id),
                    json!({"display_name": "Grace", "is_admin": false}),
                )],
            ))
            .await
            .unwrap();
        id
    };
    let _ = owner;
    owner_socket.close(None).await.unwrap();
    third.shutdown().await;

    let fourth = start(home, 0).await.unwrap();
    let mut socket = open_socket(&fourth, &_owner_token).await;
    let list = users(&command(&mut socket, CommandCall::UserList).await);
    assert_eq!(role_of(&list, "Grace").as_deref(), Some("member"));
    let claims = fourth
        .test_fields()
        .identity
        .invitation_claims()
        .await
        .unwrap();
    assert!(claims.is_empty(), "a finished join's claim is dropped");
    // The used link admits no one.
    let (_body, status) = join(&fourth, &secret, "Mallory").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    let _ = ghost;
    socket.close(None).await.unwrap();
    fourth.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_client_sees_a_user_join_live() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, _owner_token) = owner_socket(&running, &invite).await;

    owner
        .send(Message::Text(
            serde_json::to_string(&ClientFrame::Subscribe {
                id: Id::new(),
                cursor: None,
                limit: Some(100),
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    let ServerFrame::Snapshot { .. } = next_frame(&mut owner).await else {
        panic!("a subscriber starts with a snapshot");
    };

    let secret = make_invite(&mut owner).await;
    let (_member, _member_id, _member_token) = member_socket(&running, &secret, "Eve").await;

    // The join reaches the open client's live stream, not only its next
    // snapshot: People adds the new User without a reload.
    let event = loop {
        match next_frame(&mut owner).await {
            ServerFrame::Event { event } if event.kind != EventKind::UserJoined => {}
            ServerFrame::Event { event } => break event,
            ServerFrame::Heartbeat { .. } | ServerFrame::Snapshot { .. } => {}
            other => panic!("unexpected frame {other:?}"),
        }
    };
    assert_eq!(event.kind, EventKind::UserJoined);
    assert_eq!(event.data["display_name"], "Eve");

    owner.close(None).await.unwrap();
    running.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_transfer_answers_the_caller_that_ran_it() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, _owner_token) = owner_socket(&running, &invite).await;
    let secret = make_invite(&mut owner).await;
    let (_member, member_id, _member_token) = member_socket(&running, &secret, "Eve").await;

    // The answer names the caller, so a lost answer can be retried even though
    // the caller is an Admin after the transfer.
    let command_id = Id::new();
    let first = try_command_with_id(
        &mut owner,
        command_id,
        CommandCall::UserTransfer { user: member_id },
    )
    .await
    .unwrap();
    let replayed = try_command_with_id(
        &mut owner,
        command_id,
        CommandCall::UserTransfer { user: member_id },
    )
    .await
    .unwrap();
    assert_eq!(first, replayed);
    let list = users(&replayed["users"]);
    assert_eq!(role_of(&list, "Eve").as_deref(), Some("owner"));

    owner.close(None).await.unwrap();
    running.shutdown().await;
}

/// The command surface answers a role the record gives.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_reads_the_users_but_cannot_administer() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, _owner_token) = owner_socket(&running, &invite).await;
    let secret = make_invite(&mut owner).await;
    let (mut member, _member_id, _member_token) = member_socket(&running, &secret, "Eve").await;

    let list = users(&command(&mut member, CommandCall::UserList).await);
    assert_eq!(list.len(), 2);
    assert_eq!(
        try_command(&mut member, CommandCall::UserTransfer { user: Id::new() })
            .await
            .unwrap_err()
            .0,
        ErrorCode::Forbidden
    );

    owner.close(None).await.unwrap();
    member.close(None).await.unwrap();
    running.shutdown().await;
}

/// The create answer carries the secret once; the stored answer never does.
#[tokio::test(flavor = "multi_thread")]
async fn an_invitation_secret_is_shown_once_and_never_stored() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, _owner_token) = owner_socket(&running, &invite).await;

    let command_id = Id::new();
    let answer = try_command_with_id(&mut owner, command_id, CommandCall::InvitationCreate)
        .await
        .unwrap();
    let secret = answer["secret"]
        .as_str()
        .expect("the create answer shows the secret")
        .to_owned();
    let invitation = answer["invitation"]["id"].as_str().unwrap().to_owned();

    // The stored answer for the command omits the secret, and so does every
    // other command answer; the invitations table holds only the digest.
    {
        let fields = running.test_fields();
        let record = fields.store.command(command_id).await.unwrap().unwrap();
        let stored = record.result.unwrap().to_string();
        assert!(
            !stored.contains(&secret),
            "the stored answer holds the secret: {stored}"
        );
        let answers: Vec<String> =
            sqlx::query_scalar("SELECT result FROM commands WHERE result IS NOT NULL")
                .fetch_all(neha_space::identity::test_support::pool(fields.identity))
                .await
                .unwrap();
        assert!(
            answers.iter().all(|stored| !stored.contains(&secret)),
            "a stored command answer holds the secret"
        );
        let rows: Vec<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM invitations WHERE CAST(secret_hash AS TEXT) LIKE ?",
        )
        .bind(format!("%{secret}%"))
        .fetch_all(neha_space::identity::test_support::pool(fields.identity))
        .await
        .unwrap();
        assert_eq!(rows, vec![0]);
    }

    // A retry of the same command answers the same invitation, rotates
    // nothing, and does not hand the secret out again.
    let retry = try_command_with_id(&mut owner, command_id, CommandCall::InvitationCreate)
        .await
        .unwrap();
    assert_eq!(retry["invitation"]["id"], invitation);
    assert!(retry.get("secret").is_none(), "{retry}");
    let pending = command(&mut owner, CommandCall::InvitationList).await;
    assert_eq!(pending.as_array().unwrap().len(), 1);
    assert_eq!(pending[0]["id"], invitation);

    // A revoke is answered from its recorded result on a retry, even after
    // the row it removed is gone.
    let revoke_id = Id::new();
    let revoked = try_command_with_id(
        &mut owner,
        revoke_id,
        CommandCall::InvitationRevoke {
            invitation: invitation.parse().unwrap(),
        },
    )
    .await
    .unwrap();
    let again = try_command_with_id(
        &mut owner,
        revoke_id,
        CommandCall::InvitationRevoke {
            invitation: invitation.parse().unwrap(),
        },
    )
    .await
    .unwrap();
    assert_eq!(revoked, again);

    owner.close(None).await.unwrap();
    running.shutdown().await;
}

/// Two first joins cannot both become the Owner.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_first_joins_make_one_owner() {
    let dir = tempfile::tempdir().unwrap();
    let home = test_home(dir.path());
    let first = start(home.clone(), 0).await.unwrap();
    let invite = first.invite.clone().expect("a fresh Space invites");
    let secret = secret_of(&invite);

    let (ada, eve) = tokio::join!(join(&first, &secret, "Ada"), join(&first, &secret, "Eve"));
    let mut winners = Vec::new();
    let mut losers = 0;
    for (body, status) in [ada, eve] {
        if status == reqwest::StatusCode::OK {
            assert_eq!(body["account"]["role"], "owner");
            winners.push((
                body["token"].as_str().unwrap().to_owned(),
                body["account"]["id"].clone(),
            ));
        } else {
            assert_eq!(status, reqwest::StatusCode::CONFLICT, "{body}");
            losers += 1;
        }
    }
    assert_eq!(winners.len(), 1, "exactly one first join becomes the Owner");
    assert_eq!(losers, 1);
    let (token, _winner) = winners.pop().unwrap();
    first.shutdown().await;

    // The record replays: one Owner, and the Space starts again.
    let second = start(home, 0).await.unwrap();
    let mut socket = open_socket(&second, &token).await;
    let list = users(&command(&mut socket, CommandCall::UserList).await);
    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(
        role_of(&list, "Ada").or(role_of(&list, "Eve")).as_deref(),
        Some("owner")
    );
    socket.close(None).await.unwrap();
    second.shutdown().await;
}

/// Serialized transfers leave one Owner and a record that replays.
#[tokio::test(flavor = "multi_thread")]
async fn racing_transfers_leave_one_owner() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, owner_token) = owner_socket(&running, &invite).await;
    let grace_secret = make_invite(&mut owner).await;
    let (_grace, grace_id, _grace_token) = member_socket(&running, &grace_secret, "Grace").await;
    let linus_secret = make_invite(&mut owner).await;
    let (_linus, linus_id, _linus_token) = member_socket(&running, &linus_secret, "Linus").await;
    let mut second = open_socket(&running, &owner_token).await;

    let (to_grace, to_linus) = tokio::join!(
        try_command(&mut owner, CommandCall::UserTransfer { user: grace_id }),
        try_command(&mut second, CommandCall::UserTransfer { user: linus_id })
    );
    let accepted = [&to_grace, &to_linus]
        .iter()
        .filter(|result| result.is_ok())
        .count();
    let refused = [&to_grace, &to_linus]
        .iter()
        .filter(|result| result.is_err())
        .count();
    assert_eq!((accepted, refused), (1, 1), "{to_grace:?} {to_linus:?}");
    if let Err((code, _)) = &to_linus {
        assert_eq!(*code, ErrorCode::Forbidden);
    }

    let list = users(&command(&mut owner, CommandCall::UserList).await);
    assert_eq!(
        list.iter()
            .filter(|user| user["role"].as_str() == Some("owner"))
            .count(),
        1,
        "{list:?}"
    );
    second.close(None).await.unwrap();
    owner.close(None).await.unwrap();
    running.shutdown().await;

    // The record replays.
    let dir = tempfile::tempdir().unwrap();
    let home = test_home(dir.path());
    let first = start(home.clone(), 0).await.unwrap();
    let invite = first.invite.clone().unwrap();
    let (mut owner, _id, token) = owner_socket(&first, &invite).await;
    let secret = make_invite(&mut owner).await;
    let (_member, member_id, _member_token) = member_socket(&first, &secret, "Eve").await;
    let mut second = open_socket(&first, &token).await;
    let _ = tokio::join!(
        try_command(&mut owner, CommandCall::UserTransfer { user: member_id }),
        try_command(&mut second, CommandCall::UserTransfer { user: _id })
    );
    owner.close(None).await.unwrap();
    second.close(None).await.unwrap();
    first.shutdown().await;
    let replay = start(home, 0).await.unwrap();
    replay.shutdown().await;
}

/// A new link replaces a claimed one; a failed claim does not keep it alive.
#[tokio::test(flavor = "multi_thread")]
async fn a_rotation_retires_a_claimed_link() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, owner_token) = owner_socket(&running, &invite).await;
    let first = make_invite(&mut owner).await;
    // A join that claimed the link but never wrote its account.
    {
        let fields = running.test_fields();
        assert!(
            fields
                .identity
                .reserve_invitation(&first, Id::new())
                .await
                .unwrap()
        );
    }
    let second = make_invite(&mut owner).await;
    assert_ne!(first, second);
    let (_body, status) = join(&running, &first, "Mallory").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    let (_session, status) = join(&running, &second, "Eve").await;
    assert_eq!(status, reqwest::StatusCode::OK);

    // The claimed, replaced link is gone; the new one admitted one Member.
    let pending = command(&mut owner, CommandCall::InvitationList).await;
    assert!(pending.as_array().unwrap().is_empty(), "{pending}");
    let list = users(&command(&mut owner, CommandCall::UserList).await);
    assert_eq!(role_of(&list, "Eve").as_deref(), Some("member"));
    let _ = owner_token;
    owner.close(None).await.unwrap();
    running.shutdown().await;
}

/// A duplicate create answers one invitation; only one answer carries the
/// secret, and the pending list holds that one.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_create_answers_one_invitation() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, owner_token) = owner_socket(&running, &invite).await;
    let mut second = open_socket(&running, &owner_token).await;
    let command_id = Id::new();

    let (first, retry) = tokio::join!(
        try_command_with_id(&mut owner, command_id, CommandCall::InvitationCreate),
        try_command_with_id(&mut second, command_id, CommandCall::InvitationCreate)
    );
    let first = first.unwrap();
    let retry = retry.unwrap();
    assert_eq!(first["invitation"]["id"], retry["invitation"]["id"]);
    let with_secret = [&first, &retry]
        .iter()
        .filter(|answer| answer.get("secret").is_some())
        .count();
    assert_eq!(with_secret, 1, "{first} {retry}");
    let secret = first
        .get("secret")
        .or_else(|| retry.get("secret"))
        .and_then(Value::as_str)
        .unwrap()
        .to_owned();

    let pending = command(&mut owner, CommandCall::InvitationList).await;
    assert_eq!(pending.as_array().unwrap().len(), 1);
    let (_session, status) = join(&running, &secret, "Eve").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    second.close(None).await.unwrap();
    owner.close(None).await.unwrap();
    running.shutdown().await;
}

/// A claim a crash left without an account is released, not stranded.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_without_an_account_is_released_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let home = test_home(dir.path());
    let first = start(home.clone(), 0).await.unwrap();
    let invite = first.invite.clone().unwrap();
    let (mut owner, _owner_id, _owner_token) = owner_socket(&first, &invite).await;
    let secret = make_invite(&mut owner).await;
    {
        let fields = first.test_fields();
        assert!(
            fields
                .identity
                .reserve_invitation(&secret, Id::new())
                .await
                .unwrap()
        );
    }
    owner.close(None).await.unwrap();
    first.shutdown().await;

    let second = start(home, 0).await.unwrap();
    let (_session, status) = join(&second, &secret, "Eve").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let mut socket = open_socket(&second, &_owner_token).await;
    let list = users(&command(&mut socket, CommandCall::UserList).await);
    assert_eq!(role_of(&list, "Eve").as_deref(), Some("member"));
    assert!(
        second
            .test_fields()
            .identity
            .invitation_claims()
            .await
            .unwrap()
            .is_empty()
    );
    socket.close(None).await.unwrap();
    second.shutdown().await;
}

/// A record naming a User whose account is gone refuses to start.
#[tokio::test(flavor = "multi_thread")]
async fn a_record_user_without_an_account_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let home = test_home(dir.path());
    let first = start(home.clone(), 0).await.unwrap();
    let invite = first.invite.clone().unwrap();
    let (_owner, owner_id, _owner_token) = owner_socket(&first, &invite).await;
    first.shutdown().await;

    let identity = neha_space::identity::Identity::open(&home).await.unwrap();
    identity.forget_account(owner_id).await.unwrap();
    drop(identity);

    let error = match start(home, 0).await {
        Ok(_) => panic!("a record naming an accountless User must not serve"),
        Err(error) => error,
    };
    let message = error.to_string();
    assert!(
        message.contains("no account signs it in"),
        "the refusal names the mismatch: {message}"
    );
}

/// An Administrator whose role changed loses the authority at once, on the
/// socket it already holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_demoted_admin_socket_loses_its_authority() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, _owner_token) = owner_socket(&running, &invite).await;
    let secret = make_invite(&mut owner).await;
    let (session, status) = join(&running, &secret, "Eve").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let eve_id: Id = session["account"]["id"].as_str().unwrap().parse().unwrap();
    let eve_token = session["token"].as_str().unwrap().to_owned();
    let listed = command(
        &mut owner,
        CommandCall::UserSetRole {
            user: eve_id,
            role: Role::Admin,
        },
    )
    .await;
    assert_eq!(
        role_of(&users(&listed["users"]), "Eve").as_deref(),
        Some("admin")
    );

    // Eve holds a socket opened while she was an Admin; the Owner demotes her.
    let mut eve = open_socket(&running, &eve_token).await;
    let _ = command(
        &mut owner,
        CommandCall::UserSetRole {
            user: eve_id,
            role: Role::Member,
        },
    )
    .await;
    assert_eq!(
        try_command(&mut eve, CommandCall::InvitationCreate)
            .await
            .unwrap_err()
            .0,
        ErrorCode::Forbidden
    );

    eve.close(None).await.unwrap();
    owner.close(None).await.unwrap();
    running.shutdown().await;
}

/// A retry after a restart and a rotation answers its own invitation and
/// leaves the newer link alone.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_a_rotation_keeps_the_newer_link() {
    let dir = tempfile::tempdir().unwrap();
    let home = test_home(dir.path());
    let first = start(home.clone(), 0).await.unwrap();
    let invite = first.invite.clone().unwrap();
    let (mut owner, _owner_id, owner_token) = owner_socket(&first, &invite).await;
    let first_id = Id::new();
    let created = try_command_with_id(&mut owner, first_id, CommandCall::InvitationCreate)
        .await
        .unwrap();
    owner.close(None).await.unwrap();
    first.shutdown().await;

    // After a restart another Admin makes a new link; the first create's caller
    // never saw its answer and retries the same command.
    let second = start(home, 0).await.unwrap();
    let mut owner = open_socket(&second, &owner_token).await;
    let newer = make_invite(&mut owner).await;
    let retry = try_command_with_id(&mut owner, first_id, CommandCall::InvitationCreate)
        .await
        .unwrap();
    assert_eq!(retry["invitation"]["id"], created["invitation"]["id"]);
    assert!(retry.get("secret").is_none());
    let pending = command(&mut owner, CommandCall::InvitationList).await;
    assert_eq!(pending.as_array().unwrap().len(), 1, "{pending}");
    let (_session, status) = join(&second, &newer, "Eve").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let (first_body, status) = join(&second, created["secret"].as_str().unwrap(), "Mallory").await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{first_body}");

    owner.close(None).await.unwrap();
    second.shutdown().await;
}

/// A crash between a change's transaction and its event's append is replayed
/// at the next start.
#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_invitation_notification_is_replayed_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let home = test_home(dir.path());
    let first = start(home.clone(), 0).await.unwrap();
    let invite = first.invite.clone().unwrap();
    let (mut owner, _owner_id, _owner_token) = owner_socket(&first, &invite).await;
    let notice = {
        let fields = first.test_fields();
        let owner = fields
            .identity
            .owner_account()
            .await
            .unwrap()
            .expect("the Owner exists");
        let created = fields
            .identity
            .create_invitation_for_command(Id::new(), "crash-test", owner.id, true)
            .await
            .unwrap();
        let neha_space::identity::InvitationCreated::Made { notice, .. } = created else {
            panic!("a fresh command makes an invitation");
        };
        assert_eq!(
            fields.identity.pending_notices().await.unwrap().len(),
            1,
            "the change records the event it owes"
        );
        notice
    };
    owner.close(None).await.unwrap();
    first.shutdown().await;

    let second = start(home.clone(), 0).await.unwrap();
    let fields = second.test_fields();
    assert!(
        fields.identity.pending_notices().await.unwrap().is_empty(),
        "the start replays and forgets the notice"
    );
    let mut cursor = neha::seq::Seq::ZERO;
    let mut seen = false;
    loop {
        let page = fields.store.events_from(cursor, 64).await.unwrap();
        if page.events.is_empty() {
            break;
        }
        for event in &page.events {
            if event.kind == EventKind::InvitationChanged {
                seen = true;
                assert_eq!(event.command, Some(notice));
            }
        }
        cursor = page.next;
    }
    assert!(seen, "the replayed record holds the notification");
    second.shutdown().await;

    // The same repair covers a revoke whose event never reached the record.
    let third = start(home.clone(), 0).await.unwrap();
    let mut owner = open_socket(&third, &_owner_token).await;
    let pending = command(&mut owner, CommandCall::InvitationCreate).await;
    let invitation: Id = pending["invitation"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    {
        let fields = third.test_fields();
        let owner = fields.identity.owner_account().await.unwrap().unwrap();
        let revoked = fields
            .identity
            .revoke_invitation_for_command(
                Id::new(),
                "crash-test-revoke",
                owner.id,
                invitation,
                true,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                revoked,
                neha_space::identity::InvitationRevoked::Revoked { .. }
            ),
            "the revoke commits before its event"
        );
        assert_eq!(fields.identity.pending_notices().await.unwrap().len(), 1);
    }
    owner.close(None).await.unwrap();
    third.shutdown().await;

    let fourth = start(home, 0).await.unwrap();
    let fields = fourth.test_fields();
    assert!(fields.identity.pending_notices().await.unwrap().is_empty());
    assert!(
        fields.identity.invitations().await.unwrap().is_empty(),
        "the replayed revoke left no pending invitation"
    );
    let mut changes = 0;
    let mut cursor = neha::seq::Seq::ZERO;
    loop {
        let page = fields.store.events_from(cursor, 64).await.unwrap();
        if page.events.is_empty() {
            break;
        }
        changes += page
            .events
            .iter()
            .filter(|event| event.kind == EventKind::InvitationChanged)
            .count();
        cursor = page.next;
    }
    assert_eq!(
        changes, 3,
        "the crashed create, the live create, and the replayed revoke are recorded"
    );
    fourth.shutdown().await;
}

/// The invalidation event reaches the Admins and stays away from Members.
#[tokio::test(flavor = "multi_thread")]
async fn an_invitation_change_notifies_only_administrators() {
    let (_dir, running, invite) = running().await;
    let (mut observer, _owner_id, owner_token) = owner_socket(&running, &invite).await;
    let mut actor = open_socket(&running, &owner_token).await;
    let secret = make_invite(&mut actor).await;
    let (mut member, _member_id, _member_token) = member_socket(&running, &secret, "Eve").await;

    // Both clients follow the record; the invitation is made on a third
    // socket, so the observer's command helper cannot consume the event.
    for socket in [&mut observer, &mut member] {
        socket
            .send(Message::Text(
                serde_json::to_string(&ClientFrame::Subscribe {
                    id: Id::new(),
                    cursor: None,
                    limit: None,
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        loop {
            if matches!(next_frame(socket).await, ServerFrame::Snapshot { .. }) {
                break;
            }
        }
    }

    let _ = make_invite(&mut actor).await;
    assert!(
        wait_for_kind(&mut observer, EventKind::InvitationChanged).await,
        "the Admin's client learns the invitation changed"
    );
    let member_saw = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        wait_for_kind(&mut member, EventKind::InvitationChanged),
    )
    .await;
    assert!(
        member_saw.is_err() || !member_saw.unwrap(),
        "a Member's client does not learn about the invitation"
    );

    actor.close(None).await.unwrap();
    member.close(None).await.unwrap();
    observer.close(None).await.unwrap();
    running.shutdown().await;
}

/// A demoted Admin stops seeing invitation history, including its own.
#[tokio::test(flavor = "multi_thread")]
async fn a_demoted_admin_receives_no_invitation_history() {
    let (_dir, running, invite) = running().await;
    let (mut owner, _owner_id, _owner_token) = owner_socket(&running, &invite).await;
    let secret = make_invite(&mut owner).await;
    let (session, status) = join(&running, &secret, "Eve").await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let eve_id: Id = session["account"]["id"].as_str().unwrap().parse().unwrap();
    let eve_token = session["token"].as_str().unwrap().to_owned();
    let _ = command(
        &mut owner,
        CommandCall::UserSetRole {
            user: eve_id,
            role: Role::Admin,
        },
    )
    .await;
    let mut eve = open_socket(&running, &eve_token).await;
    let _ = make_invite(&mut eve).await;
    let _ = command(
        &mut owner,
        CommandCall::UserSetRole {
            user: eve_id,
            role: Role::Member,
        },
    )
    .await;

    // Reconnecting from the beginning shows no invitation change, even the one
    // Eve authored while she administered the Space.
    eve.close(None).await.unwrap();
    let mut eve = open_socket(&running, &eve_token).await;
    eve.send(Message::Text(
        serde_json::to_string(&ClientFrame::Subscribe {
            id: Id::new(),
            cursor: None,
            limit: None,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
    loop {
        if let ServerFrame::Snapshot { events, .. } = next_frame(&mut eve).await {
            assert!(
                events
                    .iter()
                    .all(|event| event.kind != EventKind::InvitationChanged),
                "a demoted Admin reads no invitation history"
            );
            break;
        }
    }

    eve.close(None).await.unwrap();
    owner.close(None).await.unwrap();
    running.shutdown().await;
}

/// Waits until one kind of live event arrives, or the socket moves on.
async fn wait_for_kind(socket: &mut Socket, kind: EventKind) -> bool {
    loop {
        match next_frame(socket).await {
            ServerFrame::Event { event } if event.kind == kind => return true,
            ServerFrame::Event { .. }
            | ServerFrame::Committed { .. }
            | ServerFrame::Accepted { .. }
            | ServerFrame::Confirmed { .. }
            | ServerFrame::Heartbeat { .. } => {}
            _ => return false,
        }
    }
}
