use actix_web::rt;

use autoconnect_common::protocol::{ServerMessage, ServerNotification};
use autopush_common::{
    notification::Notification,
    util::{sec_since_epoch, user_agent::UserAgentInfo},
};

use super::WebPushClient;
use crate::error::SMError;

impl WebPushClient {
    pub async fn on_server_notif(
        &mut self,
        snotif: ServerNotification,
    ) -> Result<Vec<ServerMessage>, SMError> {
        match snotif {
            ServerNotification::CheckStorage => self.check_storage().await,
            ServerNotification::Notification(notif) => Ok(vec![self.notif(notif).await?]),
            ServerNotification::Disconnect => Err(SMError::Ghost),
        }
    }

    /// Move queued push notifications to unacked_direct_notifs (to be stored
    /// in the db)
    pub fn on_server_notif_shutdown(&mut self, snotif: ServerNotification) {
        if let ServerNotification::Notification(notif) = snotif {
            self.ack_state.unacked_direct_notifs.push(notif);
        }
    }

    pub(super) async fn check_storage(&mut self) -> Result<Vec<ServerMessage>, SMError> {
        trace!("WebPushClient::check_storage");
        // TODO:

        // This is responsible for handling sent_from_storage > msg_limit: this
        // is tricky because historically we've sent out the check_storage
        // batch of messages before actually dropping the user. (both in the
        // python/previous state machine). Since we're dropping their messages
        // anyway, we should just drop before sending these messages. This
        // simply means we might enforce the limit at 90 (100-10) instead of
        // 100. we could also increase the limit to match the older behavior.
        //
        self.flags.include_topic = true;
        self.flags.check_storage = true;
        let CheckStorageResponse {
            include_topic,
            mut messages,
            timestamp,
        } = check_storage(self).await?;

        // XXX:
        debug!(
            "WebPushClient::check_storage include_topic: {} unacked_stored_highest -> {:?}",
            include_topic, timestamp
        );
        self.flags.include_topic = include_topic;
        self.ack_state.unacked_stored_highest = timestamp;
        if messages.is_empty() {
            debug!("WebPushClient::check_storage finished");
            self.flags.check_storage = false;
            self.sent_from_storage = 0;
            // XXX: technically back to determine ack? (maybe_post_process_acks)?
            // XXX: DetermineAck
            return Ok(vec![]);
        }

        // Filter out TTL expired messages
        // XXX: could be separated out of this method?
        let now = sec_since_epoch();
        messages.retain(|n| {
            if !n.expired(now) {
                return true;
            }
            if n.sortkey_timestamp.is_none() {
                // XXX: A batch remove_messages would be nice
                let db = self.app_state.db.clone();
                let uaid = self.uaid.clone();
                let sort_key = n.sort_key();
                rt::spawn(async move {
                    if db.remove_message(&uaid, &sort_key).await.is_ok() {
                        debug!(
                            "Deleted expired message without sortkey_timestamp, sort_key: {}",
                            sort_key
                        );
                    }
                });
            }
            false
        });

        self.flags.increment_storage = !include_topic && timestamp.is_some();
        // If there's still messages send them out
        if messages.is_empty() {
            // XXX: DetermineAck
            return Ok(vec![]);
        }
        self.ack_state
            .unacked_stored_notifs
            .extend(messages.iter().cloned());
        let smessages: Vec<_> = messages
            .into_iter()
            .inspect(|msg| {
                emit_metrics_for_send(&self.app_state.metrics, msg, "Stored", &self.ua_info)
            })
            .map(ServerMessage::Notification)
            .collect();
        self.sent_from_storage += smessages.len() as u32;
        Ok(smessages)
    }

    pub(super) async fn increment_storage(&mut self) -> Result<(), SMError> {
        let timestamp = self
            .ack_state
            .unacked_stored_highest
            .ok_or_else(|| SMError::Internal("unacked_stored_highest unset".to_owned()))?;
        self.app_state
            .db
            .increment_storage(&self.uaid, timestamp)
            .await?;
        self.flags.increment_storage = false;
        // XXX: Back to DetermineAck
        // XXX: I think just calling check_storage afterwards in maybe_post_process_acks solves this..
        Ok(())
    }

    async fn notif(&mut self, notif: Notification) -> Result<ServerMessage, SMError> {
        trace!("WebPushClient::notif Sending a direct notif");
        if notif.ttl != 0 {
            self.ack_state.unacked_direct_notifs.push(notif.clone());
        }
        emit_metrics_for_send(&self.app_state.metrics, &notif, "Direct", &self.ua_info);
        Ok(ServerMessage::Notification(notif))
    }
}

use autopush_common::db::CheckStorageResponse;
use cadence::Counted;
async fn check_storage(client: &mut WebPushClient) -> Result<CheckStorageResponse, SMError> {
    let timestamp = client.ack_state.unacked_stored_highest;
    let resp = if client.flags.include_topic {
        client.app_state.db.fetch_messages(&client.uaid, 11).await?
    } else {
        Default::default()
    };
    if !resp.messages.is_empty() {
        debug!("Topic message returns: {:?}", resp.messages);
        client
            .app_state
            .metrics
            .count_with_tags("notification.message.retrieved", resp.messages.len() as i64)
            .with_tag("topic", "true")
            .send();
        return Ok(CheckStorageResponse {
            include_topic: true,
            messages: resp.messages,
            timestamp: resp.timestamp,
        });
    }

    let timestamp = if client.flags.include_topic {
        resp.timestamp
    } else {
        timestamp
    };
    let resp = if resp.timestamp.is_some() {
        client
            .app_state
            .db
            .fetch_timestamp_messages(&client.uaid, timestamp, 10)
            .await?
    } else {
        Default::default()
    };
    let timestamp = resp.timestamp.or(timestamp);
    client
        .app_state
        .metrics
        .count_with_tags("notification.message.retrieved", resp.messages.len() as i64)
        .with_tag("topic", "false")
        .send();

    Ok(CheckStorageResponse {
        include_topic: false,
        messages: resp.messages,
        timestamp: timestamp,
    })
}

// XXX: move elsewhere
use cadence::{CountedExt, StatsdClient};

fn emit_metrics_for_send(
    metrics: &StatsdClient,
    notif: &Notification,
    source: &'static str,
    user_agent_info: &UserAgentInfo,
) {
    metrics
        .incr_with_tags("ua.notification.sent")
        .with_tag("source", source)
        .with_tag("topic", &notif.topic.is_some().to_string())
        .with_tag("os", &user_agent_info.metrics_os)
        // TODO: include `internal` if meta is set
        .send();
    metrics
        .count_with_tags(
            "ua.message_data",
            notif.data.as_ref().map_or(0, |data| data.len() as i64),
        )
        .with_tag("source", source)
        .with_tag("os", &user_agent_info.metrics_os)
        .send();
}
