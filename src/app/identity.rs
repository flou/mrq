//! The identity probe, off the startup critical path.
//!
//! The username is not obtainable offline — it is in no config key and in no cache
//! file — and three derived flags depend on it. It is also a network round trip, which
//! is the one thing the first frame must not wait for. So it runs as a tracked
//! background task and publishes what it finds twice: on a [`watch`] channel for the
//! refresh workers, which cannot fetch a row's flags without it, and as an [`AppEvent`]
//! for the loop, which repaints the rows the cache already put on screen.

use std::sync::Arc;

use tokio::sync::watch;

use crate::app::event::{AppEvent, EventSender, Tasks};
use crate::gitlab::client::Client;

/// `None` until the probe lands. Every sender dropped while still `None` means the
/// probe failed and no identity is coming.
pub type Sender = watch::Sender<Option<Arc<str>>>;
pub type Receiver = watch::Receiver<Option<Arc<str>>>;

/// A channel that starts empty, for the probe to fill in.
pub fn channel() -> (Sender, Receiver) {
    watch::channel(None)
}

/// Run the identity probe in the background.
///
/// Tracked and cancel-aware because [`Tasks::shutdown`] awaits every handle: a quit
/// during a slow probe on a VPN must not hold the terminal in the alternate screen for
/// the whole request timeout.
pub fn spawn(tasks: &mut Tasks, events: EventSender, client: Client, identity: Sender) {
    let cancel = tasks.token();
    tasks.track(tokio::spawn(async move {
        let result = tokio::select! {
            () = cancel.cancelled() => return,
            result = crate::gitlab::probe::identify_and_log(&client) => result,
        };

        match result {
            Ok(found) => {
                // The workers first: they are blocked on exactly this, while the event
                // is only a repaint of rows already on screen.
                let _ = identity.send(Some(Arc::from(found.username.as_str())));
                let _ = events.send(AppEvent::Identified {
                    username: found.username,
                });
            }
            Err(error) => {
                // Dropping the sender is how every worker learns there will be no
                // identity; the event is what makes the failure visible and fatal.
                drop(identity);
                let _ = events.send(AppEvent::IdentityFailed {
                    error: Box::new(error),
                });
            }
        }
    }));
}

/// Wait until the identity lands.
///
/// `None` means every sender was dropped without one — the probe failed, the loop is
/// already quitting with its error, and the caller has nothing left to fetch for.
pub async fn awaited(identity: &mut Receiver) -> Option<Arc<str>> {
    loop {
        // Cloned out and dropped within the statement: a live `Ref` holds a read lock
        // on the watch value, and the caller is about to await a network round trip
        // (or, here, `changed()`) with it.
        if let Some(user) = identity.borrow_and_update().clone() {
            return Some(user);
        }
        identity.changed().await.ok()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Gitlab;
    use crate::config::token::{TokenEnv, resolve};
    use crate::error::EXIT_CONFIG;
    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client_for(server: &MockServer) -> Client {
        let gitlab = Gitlab {
            url: server.uri(),
            token: Some("glpat-test".into()),
            timeout_secs: 5,
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        Client::new(&gitlab, token).unwrap()
    }

    #[tokio::test]
    async fn a_successful_probe_publishes_to_both_the_workers_and_the_loop() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
                "currentUser": {"username": "asmith"}
            }})))
            .mount(&server)
            .await;

        let (events, mut rx) = crate::app::event::channel();
        let mut tasks = Tasks::new();
        let (identity_tx, mut identity_rx) = channel();

        spawn(&mut tasks, events, client_for(&server), identity_tx);

        let user =
            tokio::time::timeout(std::time::Duration::from_secs(5), awaited(&mut identity_rx))
                .await
                .expect("the probe should resolve promptly")
                .expect("the probe succeeded");
        assert_eq!(&*user, "asmith");

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("an event should arrive promptly")
            .expect("the channel should not close");
        match event {
            AppEvent::Identified { username } => assert_eq!(username, "asmith"),
            other => panic!("unexpected event: {other:?}"),
        }

        tasks.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_probe_drops_the_sender_and_reports_a_fatal_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let (events, mut rx) = crate::app::event::channel();
        let mut tasks = Tasks::new();
        let (identity_tx, mut identity_rx) = channel();

        spawn(&mut tasks, events, client_for(&server), identity_tx);

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("an event should arrive promptly")
            .expect("the channel should not close");
        match event {
            AppEvent::IdentityFailed { error } => {
                assert!(crate::gitlab::probe::is_fatal(&error));
                assert_eq!(error.exit_code(), EXIT_CONFIG);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // The receiver must resolve to `None` rather than hang: nothing is coming.
        let resolved =
            tokio::time::timeout(std::time::Duration::from_secs(5), awaited(&mut identity_rx))
                .await
                .expect("awaited must not hang once the probe has failed");
        assert!(resolved.is_none());

        tasks.shutdown().await;
    }

    /// A quit during a slow probe must not hold the terminal in the alternate screen for
    /// the whole request timeout.
    #[tokio::test]
    async fn a_cancelled_probe_does_not_block_shutdown() {
        let gitlab = Gitlab {
            url: "http://127.0.0.1:1".into(),
            token: Some("glpat-test".into()),
            timeout_secs: 30,
            ..Gitlab::default()
        };
        let token = resolve(&gitlab, &TokenEnv::default(), None).unwrap().token;
        let client = Client::new(&gitlab, token).unwrap();

        let (events, _rx) = crate::app::event::channel();
        let mut tasks = Tasks::new();
        let (identity_tx, _identity_rx) = channel();

        spawn(&mut tasks, events, client, identity_tx);

        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.shutdown())
            .await
            .expect("shutdown should not wait out the probe's own timeout");
    }
}
