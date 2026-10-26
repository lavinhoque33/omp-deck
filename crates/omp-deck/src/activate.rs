//! `omp-deck --activate <ticket-file>`: the gateway started this process because a notification
//! was clicked while no window was open. It consumes the private one-use ticket, tells the
//! gateway to open that session for the window that is about to start (or the one that is), and
//! exits: this process never opens a window of its own.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use deck_service::gateway::Profile;
use deck_service::gateway::activation;
use deck_service::gateway::client::{GatewayClient, GatewayOptions, Link};
use deck_service::gateway::protocol::{Mutation, Role};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(25);

/// Consumes `ticket` and sends `InboxOpen(target)` when its epoch is the connected gateway's.
/// The ticket is deleted by `activation::consume` only when it is the own, valid, private file.
pub fn run(ticket: &Path, profile: Profile) -> anyhow::Result<()> {
    let ticket = activation::consume(ticket, &profile).map_err(|e| anyhow!("{e}"))?;
    let client = GatewayClient::start(GatewayOptions::new(profile, Role::Gui, Arc::new(|| {})));
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let epoch = loop {
        match client.link() {
            Link::Up(welcome) => break welcome.epoch,
            Link::Refused(r) => bail!("the gateway refused this client: {r}"),
            Link::Maintenance(_) => bail!("OmpDeck is being updated or removed"),
            Link::Connecting | Link::Down { .. } => {}
        }
        if Instant::now() >= deadline {
            bail!("could not reach the OmpDeck gateway");
        }
        std::thread::sleep(POLL);
    };
    if ticket.epoch != epoch {
        // The gateway restarted since the click: its sessions no longer carry that identity.
        bail!("the notification belongs to an earlier gateway instance");
    }
    let sent = client.mutate(Mutation::InboxOpen(ticket.target));
    let deadline = Instant::now() + ACCEPT_TIMEOUT;
    loop {
        if let Some(r) = sent.accepted.try_take() {
            r.map_err(|e| anyhow!("{e}"))?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the gateway did not accept the activation");
        }
        std::thread::sleep(POLL);
    }
}
