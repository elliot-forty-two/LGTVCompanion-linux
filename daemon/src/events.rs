use std::io;

use lgtv_companion_runtime::ipc::{DisplayPower, HostEvent, Reply, SessionSnapshot};
use tokio::{
    net::unix::OwnedWriteHalf,
    sync::{Semaphore, broadcast},
};

use crate::send;

const EVENT_BUFFER: usize = 64;
const MAX_WATCHERS: usize = 8;

pub struct Feed {
    events: broadcast::Sender<HostEvent>,
    watchers: Option<Semaphore>,
}

impl Feed {
    pub fn new(enabled: bool) -> Self {
        Self {
            events: broadcast::channel(EVENT_BUFFER).0,
            watchers: enabled.then(|| Semaphore::new(MAX_WATCHERS)),
        }
    }

    pub fn publish(&self, event: HostEvent) {
        let _ = self.events.send(event);
    }

    pub fn publish_session_changes(&self, previous: &SessionSnapshot, next: &SessionSnapshot) {
        match (previous.power, next.power) {
            (DisplayPower::Off, DisplayPower::On) => self.publish(HostEvent::DisplaysOn),
            (DisplayPower::On, DisplayPower::Off) => self.publish(HostEvent::DisplaysOff),
            _ => {}
        }
        match (previous.idle, next.idle) {
            (Some(false), Some(true)) => self.publish(HostEvent::UserIdle),
            (Some(true), Some(false)) => self.publish(HostEvent::UserBusy),
            _ => {}
        }
    }

    pub async fn watch(&self, writer: &mut OwnedWriteHalf, id: u64) -> io::Result<()> {
        let permit = match &self.watchers {
            None => Err("Host events are disabled. Set external_api to true in the daemon config"),
            Some(watchers) => watchers
                .try_acquire()
                .map_err(|_| "Too many host event watchers"),
        };
        let _permit = match permit {
            Ok(permit) => permit,
            Err(message) => {
                return send(
                    writer,
                    id,
                    Reply::Error {
                        message: message.into(),
                    },
                )
                .await;
            }
        };
        let mut events = self.events.subscribe();
        send(writer, id, Reply::Subscribed).await?;
        loop {
            match events.recv().await {
                Ok(event) => send(writer, id, Reply::Event { event }).await?,
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    return send(writer, id, Reply::Error {
                        message: format!("Host event watcher fell behind. Events missed: {count}. Reconnect to receive new events"),
                    }).await;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use lgtv_companion_runtime::ipc::{self, Response};
    use tokio::{net::UnixStream, time::timeout};

    use super::*;

    #[tokio::test]
    async fn lagged_watcher_gets_an_error_and_a_new_subscription_sees_fresh_events() {
        let feed = Feed::new(true);
        let (mut client, server) = UnixStream::pair().unwrap();
        let (_, mut writer) = server.into_split();
        let drain = async {
            let subscribed: Response = ipc::read(&mut client).await.unwrap();
            assert!(matches!(subscribed.reply, Reply::Subscribed));
            // Publish an entire burst before the subscriber can drain its queue.
            for _ in 0..=EVENT_BUFFER {
                feed.publish(HostEvent::Suspend);
            }
            let result: Response = ipc::read(&mut client).await.unwrap();
            assert!(matches!(result.reply, Reply::Error { message }
                if message.contains("Events missed: 1")));
        };
        timeout(crate::IO_TIMEOUT, async {
            let (result, ()) = tokio::join!(feed.watch(&mut writer, 1), drain);
            result.unwrap();
        })
        .await
        .unwrap();

        let mut next = feed.events.subscribe();
        assert!(matches!(
            next.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        feed.publish(HostEvent::Resume);
        assert_eq!(next.try_recv().unwrap(), HostEvent::Resume);
    }
}
