use crate::eventloop::{Eventloop, PHOENIX_TOPIC};
use crate::messages::InitGateway;
use anyhow::{Context, Result};
use backoff::backoff::Backoff;
use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};
use boringtun::x25519::StaticSecret;
use clap::Parser;
use connlib_shared::{get_user_agent, login_url, Callbacks, Mode};
use firezone_cli_utils::{setup_global_subscriber, CommonArgs};
use firezone_tunnel::{GatewayState, Tunnel};
use futures::{future, TryFutureExt};
use messages::{EgressMessages, IngressMessages};
use phoenix_channel::{PhoenixChannel, SecureUrl};
use secrecy::{Secret, SecretString};
use std::convert::Infallible;
use std::pin::pin;
use std::sync::Arc;
use tokio::signal::ctrl_c;
use tokio_tungstenite::tungstenite;
use tracing_subscriber::layer;
use url::Url;

mod eventloop;
mod messages;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    setup_global_subscriber(layer::Identity::new());

    let (connect_url, private_key) = login_url(
        Mode::Gateway,
        cli.common.api_url,
        SecretString::new(cli.common.token),
        cli.common.firezone_id,
    )?;

    let task = tokio::spawn(async move { run(connect_url, private_key).await }).map_err(Into::into);

    let ctrl_c = pin!(ctrl_c().map_err(anyhow::Error::new));

    future::try_select(task, ctrl_c)
        .await
        .map_err(|e| e.factor_first().0)?;

    Ok(())
}

async fn run(connect_url: Url, private_key: StaticSecret) -> Result<Infallible> {
    let tunnel: Arc<Tunnel<_, GatewayState>> =
        Arc::new(Tunnel::new(private_key, CallbackHandler).await?);
    let mut exponential_backoff = ExponentialBackoffBuilder::default()
        .with_max_elapsed_time(None)
        .build();

    let (portal, init) = connect_to_portal(&mut exponential_backoff, &connect_url).await?;

    exponential_backoff.reset();

    tunnel
        .set_interface(&init.interface)
        .context("Failed to set interface")?;

    let (portal_tx, portal_rx) = tokio::sync::mpsc::channel(1_000);
    let (portal_sender_tx, portal_sender_rx) = tokio::sync::mpsc::channel(1_000);
    let portal_task = tokio::spawn(async move {
        portal_loop(
            portal,
            portal_tx,
            portal_sender_rx,
            exponential_backoff,
            connect_url,
        )
        .await
        .context("Connection to portal failed")
    });

    let mut eventloop = Eventloop::new(tunnel, portal_rx, portal_sender_tx);

    let eventloop_task = tokio::spawn(async move {
        future::poll_fn(|cx| eventloop.poll(cx))
            .await
            .context("Eventloop failed")
    });

    future::try_select(portal_task, eventloop_task)
        .await
        .map_err(|e| e.factor_first().0)?;

    unreachable!("should never exit without error");
}

async fn portal_loop(
    mut portal: PhoenixChannel<IngressMessages, EgressMessages>,
    tx: tokio::sync::mpsc::Sender<IngressMessages>,
    mut rx: tokio::sync::mpsc::Receiver<EgressMessages>,
    mut exponential_backoff: ExponentialBackoff,
    connect_url: Url,
) -> Result<Infallible> {
    loop {
        handle_portal_messages(portal, tx.clone(), &mut rx).await?;
        (portal, _) = connect_to_portal(&mut exponential_backoff, &connect_url).await?;
        exponential_backoff.reset();
    }
}

async fn handle_portal_messages(
    mut portal: PhoenixChannel<IngressMessages, EgressMessages>,
    tx: tokio::sync::mpsc::Sender<IngressMessages>,
    rx: &mut tokio::sync::mpsc::Receiver<EgressMessages>,
) -> Result<()> {
    loop {
        tokio::select! {
            result = future::poll_fn(|cx| portal.poll(cx)) => {
                match result {
                    Ok(phoenix_channel::Event::InboundMessage { topic: _, msg }) => {
                        tx.send(msg).await?;
                    }
                    Err(e) => {
                        client_errors(e.into())?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
            message = rx.recv() => {
                portal.send(PHOENIX_TOPIC, message);
            }
        }
    }
}

async fn connect_to_portal(
    exponential_backoff: &mut ExponentialBackoff,
    connect_url: &Url,
) -> Result<(PhoenixChannel<IngressMessages, EgressMessages>, InitGateway)> {
    loop {
        let result = phoenix_channel::init::<InitGateway, _, _>(
            Secret::new(SecureUrl::from_url(connect_url.clone())),
            get_user_agent(),
            PHOENIX_TOPIC,
            (),
        )
        .await;

        if let Ok(Ok((portal, init))) = result {
            tracing::debug!("connected to portal");
            return Ok((portal, init));
        }

        if let Err(e) = result {
            client_errors(e.into())?;
        }

        let Some(next_backoff) = exponential_backoff.next_backoff() else {
            panic!("exponential backoff should never end");
        };

        tracing::debug!(retrying_in=?next_backoff, "portal disconnected");
        tokio::time::sleep(next_backoff).await;
    }
}

/// Keep HTTP errors as is convert all other errors to Ok, used to find out if we should keep retrying connection.
fn client_errors(e: anyhow::Error) -> Result<()> {
    // As per HTTP spec, retrying client-errors without modifying the request is pointless. Thus we abort the backoff.
    if e.chain().any(is_client_error) {
        return Err(e);
    }

    Ok(())
}

#[derive(Clone)]
struct CallbackHandler;

impl Callbacks for CallbackHandler {
    type Error = Infallible;
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(flatten)]
    common: CommonArgs,
}

/// Checks whether the given [`std::error::Error`] is in-fact an HTTP error with a 4xx status code.
fn is_client_error(e: &(dyn std::error::Error + 'static)) -> bool {
    let Some(tungstenite::Error::Http(r)) = e.downcast_ref() else {
        return false;
    };

    r.status().is_client_error()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn filters_client_error() {
        let thrown_error =
            anyhow::Error::new(phoenix_channel::Error::WebSocket(tungstenite::Error::Http(
                tungstenite::http::Response::builder()
                    .status(400)
                    .body(None)
                    .unwrap(),
            )));

        let converted_error = client_errors(thrown_error);

        assert!(converted_error.is_err());
    }

    #[test]
    fn ok_for_non_client_error() {
        let thrown_error = anyhow!("normal error");

        let converted_error = client_errors(thrown_error);

        assert!(converted_error.is_ok());
    }
}
