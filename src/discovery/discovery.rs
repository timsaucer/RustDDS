use log::{debug, error, info};
use mio::{Ready, Poll, PollOpt, Events};
use mio_extras::timer::Timer;
use mio_extras::channel as mio_channel;

use std::{
  sync::{Arc, RwLock},
  sync::RwLockReadGuard,
  sync::RwLockWriteGuard,
  time::Duration as StdDuration,
};

use crate::{
  dds::{
    with_key::datareader::DataReader,
    with_key::datawriter::DataWriter,
    topic::*,
    participant::{DomainParticipantWeak},
    qos::{
      QosPolicies,
      policy::{
        Reliability, History, Durability, Presentation, PresentationAccessScope, Deadline,
        Ownership, Liveliness, TimeBasedFilter, DestinationOrder, ResourceLimits,
      },
    },
    readcondition::ReadCondition,
  },
  dds::values::result::Error,
  serialization::CDRDeserializerAdapter,
  structure::entity::Entity,
  structure::guid::GUID,
  dds::qos::QosPolicyBuilder,
};

use crate::discovery::{
  data_types::spdp_participant_data::SPDPDiscoveredParticipantData,
  data_types::topic_data::{DiscoveredWriterData, DiscoveredReaderData},
  discovery_db::DiscoveryDB,
};

use crate::structure::{duration::Duration, guid::EntityId, time::Timestamp};

use crate::serialization::{CDRSerializerAdapter, pl_cdr_deserializer::PlCdrDeserializerAdapter};

use crate::network::constant::*;
use super::data_types::topic_data::{
  DiscoveredTopicData, ParticipantMessageData, ParticipantMessageDataKind,
};
use byteorder::LittleEndian;

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum DiscoveryCommand {
  STOP_DISCOVERY,
  REMOVE_LOCAL_WRITER { guid: GUID },
  REMOVE_LOCAL_READER { guid: GUID },
  REFRESH_LAST_MANUAL_LIVELINESS,
  ASSERT_TOPIC_LIVELINESS { writer_guid: GUID },
}

pub struct LivelinessState {
  last_auto_update: Timestamp,
  last_manual_participant_update: Timestamp,
}

impl LivelinessState {
  pub fn new() -> LivelinessState {
    LivelinessState {
      last_auto_update: Timestamp::now(),
      last_manual_participant_update: Timestamp::now(),
    }
  }
}

pub(crate) struct Discovery {
  poll: Poll,
  domain_participant: DomainParticipantWeak,
  discovery_db: Arc<RwLock<DiscoveryDB>>,
  discovery_started_sender: std::sync::mpsc::Sender<Result<(), Error>>,
  discovery_updated_sender: mio_channel::SyncSender<DiscoveryNotificationType>,
  discovery_command_receiver: mio_channel::Receiver<DiscoveryCommand>,
}

unsafe impl Sync for Discovery {}
unsafe impl Send for Discovery {}

impl Discovery {
  const PARTICIPANT_CLEANUP_PERIOD: StdDuration = StdDuration::from_secs(2);
  const TOPIC_CLEANUP_PERIOD: StdDuration = StdDuration::from_secs(10); // timer for cleaning up inactive topics
  const SEND_PARTICIPANT_INFO_PERIOD: StdDuration = StdDuration::from_secs(2);
  const SEND_READERS_INFO_PERIOD: StdDuration = StdDuration::from_secs(2);
  const SEND_WRITERS_INFO_PERIOD: StdDuration = StdDuration::from_secs(2);
  const SEND_TOPIC_INFO_PERIOD: StdDuration = StdDuration::from_secs(20);
  const CHECK_PARTICIPANT_MESSAGES: StdDuration = StdDuration::from_secs(1);

  pub(crate) const PARTICIPANT_MESSAGE_QOS: QosPolicies = QosPolicies {
    durability: Some(Durability::TransientLocal),
    presentation: None,
    deadline: None,
    latency_budget: None,
    ownership: None,
    liveliness: None,
    time_based_filter: None,
    reliability: Some(Reliability::Reliable {
      max_blocking_time: Duration::DURATION_ZERO,
    }),
    destination_order: None,
    history: Some(History::KeepLast { depth: 1 }),
    resource_limits: None,
    lifespan: None,
  };

  pub fn new(
    domain_participant: DomainParticipantWeak,
    discovery_db: Arc<RwLock<DiscoveryDB>>,
    discovery_started_sender: std::sync::mpsc::Sender<Result<(), Error>>,
    discovery_updated_sender: mio_channel::SyncSender<DiscoveryNotificationType>,
    discovery_command_receiver: mio_channel::Receiver<DiscoveryCommand>,
  ) -> Discovery {
    let poll = match mio::Poll::new() {
      Ok(p) => p,
      Err(e) => {
        error!("Failed to start discovery poll. {:?}", e);
        discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        panic!("");
      }
    };

    Discovery {
      poll,
      domain_participant,
      discovery_db,
      discovery_started_sender,
      discovery_updated_sender,
      discovery_command_receiver,
    }
  }

  fn create_spdp_patricipant_qos() -> QosPolicies {
    QosPolicyBuilder::new()
      .reliability(Reliability::BestEffort)
      .history(History::KeepLast { depth: 1 })
      .build()
  }

  pub fn discovery_event_loop(discovery: Discovery) {
    let mut liveliness_state = LivelinessState::new();

    match discovery.poll.register(
      &discovery.discovery_command_receiver,
      DISCOVERY_COMMAND_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Failed to register Discovery STOP. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let discovery_subscriber_qos = Discovery::subscriber_qos();
    let discovery_subscriber = match discovery
      .domain_participant
      .create_subscriber(&discovery_subscriber_qos)
    {
      Ok(s) => s,
      Err(e) => {
        error!("Unable to create Discovery Subscriber. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let discovery_publisher_qos = Discovery::subscriber_qos();
    let discovery_publisher = match discovery
      .domain_participant
      .create_publisher(&discovery_publisher_qos)
    {
      Ok(p) => p,
      Err(e) => {
        error!("Unable to create Discovery Publisher. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    // Participant
    let dcps_participant_topic = match discovery.domain_participant.create_topic(
      "DCPSParticipant",
      "SPDPDiscoveredParticipantData",
      &Discovery::create_spdp_patricipant_qos(),
      TopicKind::WithKey,
    ) {
      Ok(t) => t,
      Err(e) => {
        error!("Unable to create DCPSParticipant topic. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_participant_reader = match discovery_subscriber
      .create_datareader::<SPDPDiscoveredParticipantData,PlCdrDeserializerAdapter<SPDPDiscoveredParticipantData>>(
        &dcps_participant_topic,
        Some(EntityId::ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER),
        None,
      ) {
        Ok(r) => r,
        Err(e) => {
          error!("Unable to create DataReader for DCPSParticipant. {:?}", e);
           // were trying to quit, if send fails just ignore
           discovery
            .discovery_started_sender
            .send(Err(Error::PreconditionNotMet))
            .unwrap_or(());
          return;
        }
      };

    // register participant reader
    match discovery.poll.register(
      &dcps_participant_reader,
      DISCOVERY_PARTICIPANT_DATA_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Failed to register participant reader to poll. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    // create lease duration check timer
    let mut participant_cleanup_timer: Timer<()> = Timer::default();
    participant_cleanup_timer.set_timeout(Discovery::PARTICIPANT_CLEANUP_PERIOD, ());
    match discovery.poll.register(
      &participant_cleanup_timer,
      DISCOVERY_PARTICIPANT_CLEANUP_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to create participant cleanup timer. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let dcps_participant_writer = match discovery_publisher
      .create_datawriter::<SPDPDiscoveredParticipantData, CDRSerializerAdapter<SPDPDiscoveredParticipantData,LittleEndian> >(
        Some(EntityId::ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER),
        &dcps_participant_topic,
        None,
      ) {
        Ok(w) => w,
        Err(e) => {
          error!("Unable to create DataWriter for DCPSParticipant. {:?}", e);
          // were trying to quit, if send fails just ignore
          discovery
            .discovery_started_sender
            .send(Err(Error::PreconditionNotMet))
            .unwrap_or(());
          return;
        }
      };

    // creating timer for sending out own participant data
    let mut participant_send_info_timer: Timer<()> = Timer::default();
    participant_send_info_timer.set_timeout(Discovery::SEND_PARTICIPANT_INFO_PERIOD, ());

    match discovery.poll.register(
      &participant_send_info_timer,
      DISCOVERY_SEND_PARTICIPANT_INFO_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register participant info sender. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    // Subcription
    let dcps_subscription_qos = Discovery::subscriber_qos();
    let dcps_subscription_topic = match discovery.domain_participant.create_topic(
      "DCPSSubscription",
      "DiscoveredReaderData",
      &dcps_subscription_qos,
      TopicKind::WithKey,
    ) {
      Ok(t) => t,
      Err(e) => {
        error!("Unable to create DCPSSubscription topic. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_subscription_reader = match discovery_subscriber
      .create_datareader::<DiscoveredReaderData, PlCdrDeserializerAdapter<DiscoveredReaderData>>(
        &dcps_subscription_topic,
        Some(EntityId::ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER),
        None,
      ) {
      Ok(r) => r,
      Err(e) => {
        error!("Unable to create DataReader for DCPSSubscription. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    match discovery.poll.register(
      &dcps_subscription_reader,
      DISCOVERY_READER_DATA_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register subscription reader. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_subscription_writer = match discovery_publisher
      .create_datawriter::<DiscoveredReaderData,CDRSerializerAdapter<DiscoveredReaderData,LittleEndian>>(
        Some(EntityId::ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER),
        &dcps_subscription_topic,
        None,
      ) {
        Ok(w) => w,
        Err(e) => {
          error!("Unable to create DataWriter for DCPSSubscription. {:?}", e);
          // were trying to quit, if send fails just ignore
          discovery
            .discovery_started_sender
            .send(Err(Error::PreconditionNotMet))
            .unwrap_or(());
          return;
        }
      };

    let mut readers_send_info_timer: Timer<()> = Timer::default();
    readers_send_info_timer.set_timeout(Discovery::SEND_READERS_INFO_PERIOD, ());
    match discovery.poll.register(
      &readers_send_info_timer,
      DISCOVERY_SEND_READERS_INFO_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register readers info sender. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    // Publication
    let dcps_publication_qos = Discovery::subscriber_qos();
    let dcps_publication_topic = match discovery.domain_participant.create_topic(
      "DCPSPublication",
      "DiscoveredWriterData",
      &dcps_publication_qos,
      TopicKind::WithKey,
    ) {
      Ok(t) => t,
      Err(e) => {
        error!("Unable to create DCPSPublication topic. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_publication_reader = match discovery_subscriber
      .create_datareader::<DiscoveredWriterData, PlCdrDeserializerAdapter<DiscoveredWriterData>>(
        &dcps_publication_topic,
        Some(EntityId::ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER),
        None,
      ) {
      Ok(r) => r,
      Err(e) => {
        error!("Unable to create DataReader for DCPSPublication. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    match discovery.poll.register(
      &dcps_publication_reader,
      DISCOVERY_WRITER_DATA_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to regiser writers info sender. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_publication_writer = match discovery_publisher
      .create_datawriter::<DiscoveredWriterData, CDRSerializerAdapter<DiscoveredWriterData,LittleEndian>>(
        Some(EntityId::ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER),
        &dcps_publication_topic,
        None,
      ) {
        Ok(w) => w,
        Err(e) => {
          error!("Unable to create DataWriter for DCPSPublication. {:?}", e);
          // were trying to quit, if send fails just ignore
          discovery
            .discovery_started_sender
            .send(Err(Error::PreconditionNotMet))
            .unwrap_or(());
          return;
        }
      };

    let mut writers_send_info_timer: Timer<()> = Timer::default();
    writers_send_info_timer.set_timeout(Discovery::SEND_WRITERS_INFO_PERIOD, ());
    match discovery.poll.register(
      &writers_send_info_timer,
      DISCOVERY_SEND_WRITERS_INFO_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register readers info sender. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    // Topic
    let dcps_topic_qos = QosPolicyBuilder::new().build();
    let dcps_topic = match discovery.domain_participant.create_topic(
      "DCPSTopic",
      "DiscoveredTopicData",
      &dcps_topic_qos,
      TopicKind::WithKey,
    ) {
      Ok(t) => t,
      Err(e) => {
        error!("Unable to create DCPSTopic topic. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    // create lease duration check timer
    let mut topic_cleanup_timer: Timer<()> = Timer::default();
    topic_cleanup_timer.set_timeout(Discovery::TOPIC_CLEANUP_PERIOD, ());
    match discovery.poll.register(
      &topic_cleanup_timer,
      DISCOVERY_TOPIC_CLEANUP_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register topic cleanup timer. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_reader = match discovery_subscriber
      .create_datareader::<DiscoveredTopicData, PlCdrDeserializerAdapter<DiscoveredTopicData>>(
        &dcps_topic,
        Some(EntityId::ENTITYID_SEDP_BUILTIN_TOPIC_READER),
        None,
      ) {
      Ok(r) => r,
      Err(e) => {
        error!("Unable to create DataReader for DCPSTopic. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    match discovery.poll.register(
      &dcps_reader,
      DISCOVERY_TOPIC_DATA_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register topic reader. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_writer = match discovery_publisher
      .create_datawriter::<DiscoveredTopicData, CDRSerializerAdapter<DiscoveredTopicData,LittleEndian>>(
        Some(EntityId::ENTITYID_SEDP_BUILTIN_TOPIC_WRITER),
        &dcps_topic,
        None,
      ) {
        Ok(w) => w,
        Err(e) => {
          error!("Unable to create DataWriter for DCPSTopic. {:?}", e);
          // were trying to quit, if send fails just ignore
          discovery
            .discovery_started_sender
            .send(Err(Error::PreconditionNotMet))
            .unwrap_or(());
          return;
        }
      };

    let mut topic_info_send_timer: Timer<()> = Timer::default();
    topic_info_send_timer.set_timeout(Discovery::SEND_TOPIC_INFO_PERIOD, ());
    match discovery.poll.register(
      &topic_info_send_timer,
      DISCOVERY_SEND_TOPIC_INFO_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register topic info sender. {:?}", e);
        // were trying to quit, if send fails just ignore
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    // Participant Message Data 8.4.13
    let participant_message_data_topic = match discovery.domain_participant.create_topic(
      "DCPSParticipantMessage",
      "ParticipantMessageData",
      &Discovery::PARTICIPANT_MESSAGE_QOS,
      TopicKind::WithKey,
    ) {
      Ok(t) => t,
      Err(e) => {
        error!("Unable to create DCPSParticipantMessage topic. {:?}", e);
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_participant_message_reader = match discovery_subscriber
      .create_datareader::<ParticipantMessageData, CDRDeserializerAdapter<ParticipantMessageData>>(
        &participant_message_data_topic,
        Some(EntityId::ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER),
        None,
      ) {
      Ok(r) => r,
      Err(e) => {
        error!("Unable to create DCPSParticipantMessage reader. {:?}", e);
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    match discovery.poll.register(
      &dcps_participant_message_reader,
      DISCOVERY_PARTICIPANT_MESSAGE_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register DCPSParticipantMessage reader. {:?}", e);
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_participant_message_writer = match discovery_publisher
      .create_datawriter::<ParticipantMessageData, CDRSerializerAdapter<ParticipantMessageData, LittleEndian>>(
        Some(EntityId::ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER),
        &participant_message_data_topic,
        None,
      ) {
      Ok(w) => w,
      Err(e) => {
        error!("Unable to create DCPSParticipantMessage writer. {:?}", e);
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    let mut dcps_participant_message_timer = mio_extras::timer::Timer::default();
    dcps_participant_message_timer.set_timeout(Discovery::CHECK_PARTICIPANT_MESSAGES, ());
    match discovery.poll.register(
      &dcps_participant_message_timer,
      DISCOVERY_PARTICIPANT_MESSAGE_TIMER_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    ) {
      Ok(_) => (),
      Err(e) => {
        error!("Unable to register DCPSParticipantMessage timer. {:?}", e);
        discovery
          .discovery_started_sender
          .send(Err(Error::PreconditionNotMet))
          .unwrap_or(());
        return;
      }
    };

    discovery.initialize_participant(&discovery.domain_participant);

    discovery.write_writers_info(&mut dcps_publication_writer);
    discovery.write_readers_info(&mut dcps_subscription_writer);

    match discovery.discovery_started_sender.send(Ok(())) {
      Ok(_) => (),
      // Participant has probably crashed at this point
      _ => return,
    };

    loop {
      let mut events = Events::with_capacity(1024);
      match discovery.poll.poll(&mut events, None) {
        Ok(_) => (),
        Err(e) => {
          error!("Failed in waiting of poll in discovery. {:?}", e);
          return;
        }
      }

      for event in events.into_iter() {
        if event.token() == DISCOVERY_COMMAND_TOKEN {
          while let Ok(command) = discovery.discovery_command_receiver.try_recv() {
            match command {
              DiscoveryCommand::STOP_DISCOVERY => {
                info!("Stopping Discovery");

                // disposing readers
                let db = discovery.discovery_db_read();
                let readers = db.get_all_local_topic_readers();
                for reader in readers {
                  dcps_subscription_writer
                    .dispose(reader.reader_proxy.remote_reader_guid.unwrap(), None)
                    .unwrap_or(());
                }

                let writers = db.get_all_local_topic_writers();
                for writer in writers {
                  dcps_publication_writer
                    .dispose(writer.writer_proxy.remote_writer_guid.unwrap(), None)
                    .unwrap_or(());
                }

                // finally disposing the participant we have
                let guid = discovery.domain_participant.get_guid();
                dcps_participant_writer.dispose(guid, None).unwrap_or(());

                return;
              }
              DiscoveryCommand::REMOVE_LOCAL_WRITER { guid } => {
                if guid == dcps_publication_writer.get_guid() {
                  continue;
                }

                dcps_publication_writer.dispose(guid, None).unwrap_or(());

                match discovery.discovery_db.write() {
                  Ok(mut db) => {
                    db.remove_local_topic_writer(guid);
                  }
                  Err(e) => panic!("DiscoveryDB is poisoned. {:?}", e),
                };
              }
              DiscoveryCommand::REMOVE_LOCAL_READER { guid } => {
                if guid == dcps_subscription_writer.get_guid() {
                  continue;
                }

                dcps_subscription_writer.dispose(guid, None).unwrap_or(());

                match discovery.discovery_db.write() {
                  Ok(mut db) => {
                    db.remove_local_topic_reader(guid);
                  }
                  Err(e) => panic!("DiscoveryDB is poisoned. {:?}", e),
                };
              }
              DiscoveryCommand::REFRESH_LAST_MANUAL_LIVELINESS => {
                liveliness_state.last_manual_participant_update = Timestamp::now();
              }
              DiscoveryCommand::ASSERT_TOPIC_LIVELINESS { writer_guid } => {
                discovery.send_discovery_notification(
                  DiscoveryNotificationType::AssertTopicLiveliness { writer_guid },
                );
              }
            };
          }
        } else if event.token() == DISCOVERY_PARTICIPANT_DATA_TOKEN {
          let data = discovery.handle_participant_reader(&mut dcps_participant_reader);
          match data {
            Some(dat) => {
              discovery.update_spdp_participant_writer(dat);
            }
            None => (),
          }
        } else if event.token() == DISCOVERY_PARTICIPANT_CLEANUP_TOKEN {
          discovery.participant_cleanup();
          // setting next cleanup timeout
          participant_cleanup_timer.set_timeout(Discovery::PARTICIPANT_CLEANUP_PERIOD, ());
        } else if event.token() == DISCOVERY_SEND_PARTICIPANT_INFO_TOKEN {
          // setting 3 times the duration so lease doesn't break if we fail once for some reason
          let lease_duration = Discovery::SEND_PARTICIPANT_INFO_PERIOD
            + Discovery::SEND_PARTICIPANT_INFO_PERIOD
            + Discovery::SEND_PARTICIPANT_INFO_PERIOD;
          let strong_dp = match discovery.domain_participant.clone().upgrade() {
            Some(dp) => dp,
            None => {
              error!("DomainParticipant doesn't exist anymore, exiting Discovery.");
              return;
            }
          };
          let data = SPDPDiscoveredParticipantData::from_participant(
            &strong_dp,
            Duration::from(lease_duration),
          );

          dcps_participant_writer.write(data, None).unwrap_or(());
          // reschedule timer
          participant_send_info_timer.set_timeout(Discovery::SEND_PARTICIPANT_INFO_PERIOD, ());
        } else if event.token() == DISCOVERY_READER_DATA_TOKEN {
          discovery.handle_subscription_reader(&mut dcps_subscription_reader);
        } else if event.token() == DISCOVERY_SEND_READERS_INFO_TOKEN {
          if discovery.read_readers_info() {
            discovery.write_readers_info(&mut dcps_subscription_writer);
          }

          readers_send_info_timer.set_timeout(Discovery::SEND_READERS_INFO_PERIOD, ());
        } else if event.token() == DISCOVERY_WRITER_DATA_TOKEN {
          discovery.handle_publication_reader(&mut dcps_publication_reader);
        } else if event.token() == DISCOVERY_SEND_WRITERS_INFO_TOKEN {
          if discovery.read_writers_info() {
            discovery.write_writers_info(&mut dcps_publication_writer);
          }

          writers_send_info_timer.set_timeout(Discovery::SEND_WRITERS_INFO_PERIOD, ());
        } else if event.token() == DISCOVERY_TOPIC_DATA_TOKEN {
          discovery.handle_topic_reader(&mut dcps_reader);
        } else if event.token() == DISCOVERY_TOPIC_CLEANUP_TOKEN {
          discovery.topic_cleanup();

          topic_cleanup_timer.set_timeout(Discovery::TOPIC_CLEANUP_PERIOD, ());
        } else if event.token() == DISCOVERY_SEND_TOPIC_INFO_TOKEN {
          discovery.write_topic_info(&mut dcps_writer);
          topic_info_send_timer.set_timeout(Discovery::SEND_TOPIC_INFO_PERIOD, ());
        } else if event.token() == DISCOVERY_PARTICIPANT_MESSAGE_TOKEN {
          discovery.handle_participant_message_reader(&mut dcps_participant_message_reader);
        } else if event.token() == DISCOVERY_PARTICIPANT_MESSAGE_TIMER_TOKEN {
          discovery
            .write_participant_message(&mut dcps_participant_message_writer, &mut liveliness_state);
          dcps_participant_message_timer.set_timeout(Discovery::CHECK_PARTICIPANT_MESSAGES, ());
        }
      }
    }
  }

  pub fn initialize_participant(&self, dp: &DomainParticipantWeak) {
    let mut db = self.discovery_db_write();
    let port = get_spdp_well_known_multicast_port(dp.domain_id());
    db.initialize_participant_reader_proxy(port);
    self.send_discovery_notification(DiscoveryNotificationType::WritersInfoUpdated {
      needs_new_cache_change: true,
    });
  }

  pub fn handle_participant_reader(
    &self,
    reader: &mut DataReader<
      SPDPDiscoveredParticipantData,
      PlCdrDeserializerAdapter<SPDPDiscoveredParticipantData>,
    >,
  ) -> Option<SPDPDiscoveredParticipantData> {
    let participant_data = match reader.take_next_sample() {
      Ok(d) => match d {
        Some(d) => match d.value() {
          Ok(aaaaa) => (aaaaa).clone(),
          Err(key) => {
            // we should dispose participant here
            self.discovery_db_write().remove_participant(*key);
            self.send_discovery_notification(DiscoveryNotificationType::WritersInfoUpdated {
              needs_new_cache_change: false,
            });
            self.send_discovery_notification(DiscoveryNotificationType::ReadersInfoUpdated);
            return None;
          }
        },
        None => return None,
      },
      _ => return None,
    };

    let mut db = self.discovery_db_write();
    let updated = db.update_participant(&participant_data);
    if updated {
      self.send_discovery_notification(DiscoveryNotificationType::WritersInfoUpdated {
        needs_new_cache_change: false,
      });
      self.send_discovery_notification(DiscoveryNotificationType::ReadersInfoUpdated);

      return Some(participant_data);
    }

    None
  }

  pub fn handle_subscription_reader(
    &self,
    reader: &mut DataReader<DiscoveredReaderData, PlCdrDeserializerAdapter<DiscoveredReaderData>>,
  ) {
    match reader.take(100, ReadCondition::not_read()) {
      Ok(d) => {
        let mut db = self.discovery_db_write();
        for data in d.into_iter() {
          match data.value() {
            Ok(val) => {
              db.update_subscription(&val);
              self.send_discovery_notification(DiscoveryNotificationType::WritersInfoUpdated {
                needs_new_cache_change: true,
              });
              db.update_topic_data_drd(&val);
            }
            Err(guid) => {
              db.remove_topic_reader(*guid);
              self.send_discovery_notification(DiscoveryNotificationType::WritersInfoUpdated {
                needs_new_cache_change: false,
              });
            }
          }
        }
      }
      _ => (),
    };
  }

  pub fn handle_publication_reader(
    &self,
    reader: &mut DataReader<DiscoveredWriterData, PlCdrDeserializerAdapter<DiscoveredWriterData>>,
  ) {
    match reader.take(100, ReadCondition::not_read()) {
      Ok(d) => {
        let mut db = self.discovery_db_write();
        for data in d.into_iter() {
          match data.value() {
            Ok(val) => {
              db.update_publication(&val);
              self.send_discovery_notification(DiscoveryNotificationType::ReadersInfoUpdated);
              db.update_topic_data_dwd(&val);
            }
            Err(guid) => {
              db.remove_topic_writer(*guid);
              self.send_discovery_notification(DiscoveryNotificationType::ReadersInfoUpdated);
            }
          }
        }
      }
      _ => (),
    };
  }

  pub fn handle_topic_reader(
    &self,
    reader: &mut DataReader<DiscoveredTopicData, PlCdrDeserializerAdapter<DiscoveredTopicData>>,
  ) {
    let topic_data_vec: Option<Vec<DiscoveredTopicData>> =
      match reader.take(100, ReadCondition::any()) {
        Ok(d) => Some(
          d.into_iter()
            .map(|p| p.value().clone())
            .filter_map(Result::ok)
            .collect(),
        ),
        _ => None,
      };

    let topic_data_vec = match topic_data_vec {
      Some(d) => d,
      None => return,
    };

    let mut db = self.discovery_db_write();
    topic_data_vec.iter().for_each(|data| {
      let updated = db.update_topic_data(data);
      if updated {
        self.send_discovery_notification(DiscoveryNotificationType::TopicsInfoUpdated);
      }
    });
  }

  pub fn handle_participant_message_reader(
    &self,
    reader: &mut DataReader<ParticipantMessageData, CDRDeserializerAdapter<ParticipantMessageData>>,
  ) {
    let participant_messages: Option<Vec<ParticipantMessageData>> =
      match reader.take(100, ReadCondition::any()) {
        Ok(msgs) => Some(
          msgs
            .into_iter()
            .map(|p| p.value().clone())
            .filter_map(Result::ok)
            .collect(),
        ),
        _ => None,
      };

    let msgs = match participant_messages {
      Some(d) => d,
      None => return,
    };

    let mut db = self.discovery_db_write();
    for msg in msgs.into_iter() {
      db.update_lease_duration(msg);
    }
  }

  pub fn write_participant_message(
    &self,
    writer: &mut DataWriter<
      ParticipantMessageData,
      CDRSerializerAdapter<ParticipantMessageData, LittleEndian>,
    >,
    liveliness_state: &mut LivelinessState,
  ) {
    let db = self.discovery_db_read();

    let writer_liveliness: Vec<Liveliness> = db
      .get_all_local_topic_writers()
      .filter_map(|p| {
        let liveliness = match p.publication_topic_data.liveliness {
          Some(lv) => lv,
          None => return None,
        };

        Some(liveliness)
      })
      .collect();

    let (automatic, manual): (Vec<&Liveliness>, Vec<&Liveliness>) =
      writer_liveliness.iter().partition(|p| match p {
        Liveliness::Automatic { lease_duration: _ } => true,
        Liveliness::ManualByParticipant { lease_duration: _ } => false,
        Liveliness::ManualByTopic { lease_duration: _ } => false,
      });

    let (manual_by_participant, _manual_by_topic): (Vec<&Liveliness>, Vec<&Liveliness>) =
      manual.iter().partition(|p| match p {
        Liveliness::Automatic { lease_duration: _ } => false,
        Liveliness::ManualByParticipant { lease_duration: _ } => true,
        Liveliness::ManualByTopic { lease_duration: _ } => false,
      });

    let inow = Timestamp::now();

    // Automatic
    {
      let current_duration =
        Duration::from(inow.duration_since(liveliness_state.last_auto_update) / 3);
      let min_automatic = automatic
        .iter()
        .map(|lv| match lv {
          Liveliness::Automatic { lease_duration }
          | Liveliness::ManualByParticipant { lease_duration }
          | Liveliness::ManualByTopic { lease_duration } => lease_duration,
        })
        .min();
      debug!(
        "Current auto duration {:?}. Min auto duration {:?}",
        current_duration, min_automatic
      );
      match min_automatic {
        Some(&mm) => {
          if current_duration > mm {
            let pp = ParticipantMessageData {
              guid: self.domain_participant.get_guid_prefix(),
              kind:
                ParticipantMessageDataKind::PARTICIPANT_MESSAGE_DATA_KIND_AUTOMATIC_LIVELINESS_UPDATE,
              length: 0,
              data: Vec::new(),
            };
            match writer.write(pp, None) {
              Ok(_) => (),
              Err(e) => {
                error!("Failed to write ParticipantMessageData auto. {:?}", e);
                return;
              }
            }
            liveliness_state.last_auto_update = inow;
          }
        }
        None => (),
      };
    }

    // Manual By Participant
    {
      let current_duration =
        Duration::from(inow.duration_since(liveliness_state.last_manual_participant_update) / 3);
      let min_manual_participant = manual_by_participant
        .iter()
        .map(|lv| match lv {
          Liveliness::Automatic { lease_duration }
          | Liveliness::ManualByParticipant { lease_duration }
          | Liveliness::ManualByTopic { lease_duration } => lease_duration,
        })
        .min();
      match min_manual_participant {
        Some(&dur) => {
          if current_duration > dur {
            let pp = ParticipantMessageData {
              guid: self.domain_participant.get_guid_prefix(),
              kind:
                ParticipantMessageDataKind::PARTICIPANT_MESSAGE_DATA_KIND_MANUAL_LIVELINESS_UPDATE,
              length: 0,
              data: Vec::new(),
            };
            match writer.write(pp, None) {
              Ok(_) => (),
              Err(e) => {
                error!("Failed to writer ParticipantMessageData manual. {:?}", e);
                return;
              }
            }
          }
        }
        None => (),
      };
    }
  }

  pub fn participant_cleanup(&self) {
    self.discovery_db_write().participant_cleanup();
  }

  pub fn topic_cleanup(&self) {
    self.discovery_db_write().topic_cleanup();
  }

  pub fn update_spdp_participant_writer(&self, data: SPDPDiscoveredParticipantData) -> bool {
    if !self.discovery_db_write().update_participant(&data) {
      return false;
    }

    self.send_discovery_notification(DiscoveryNotificationType::WritersInfoUpdated {
      needs_new_cache_change: true,
    });

    true
  }

  pub fn read_readers_info(&self) -> bool {
    let readers_info_updated = self.discovery_db_read().is_readers_updated();

    if readers_info_updated {
      self.discovery_db_write().readers_updated(false);
    }

    readers_info_updated
  }

  pub fn read_writers_info(&self) -> bool {
    let writers_info_updated = self.discovery_db_read().is_writers_updated();

    if writers_info_updated {
      self.discovery_db_write().writers_updated(false);
    }

    writers_info_updated
  }

  pub fn write_readers_info(
    &self,
    writer: &mut DataWriter<
      DiscoveredReaderData,
      CDRSerializerAdapter<DiscoveredReaderData, LittleEndian>,
    >,
  ) {
    let db = self.discovery_db_read();
    let datas = db.get_all_local_topic_readers();
    for data in datas
      // filtering out discoveries own readers
      .filter(|p| {
        let guid = match &p.reader_proxy.remote_reader_guid {
          Some(g) => g,
          None => return false,
        };
        let eid = &guid.entityId;
        *eid != EntityId::ENTITYID_SPDP_BUILTIN_PARTICIPANT_READER
          && *eid != EntityId::ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_READER
          && *eid != EntityId::ENTITYID_SEDP_BUILTIN_PUBLICATIONS_READER
          && *eid != EntityId::ENTITYID_SEDP_BUILTIN_TOPIC_READER
          && *eid != EntityId::ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_READER
      })
    {
      match writer.write(data.clone(), None) {
        Ok(_) => (),
        Err(e) => error!("Unable to write new readers info. {:?}", e),
      }
    }
  }

  pub fn write_writers_info(
    &self,
    writer: &mut DataWriter<
      DiscoveredWriterData,
      CDRSerializerAdapter<DiscoveredWriterData, LittleEndian>,
    >,
  ) {
    let db = self.discovery_db_read();
    let datas = db.get_all_local_topic_writers();
    for data in datas.filter(|p| {
      let guid = match &p.writer_proxy.remote_writer_guid {
        Some(g) => g,
        None => return false,
      };
      let eid = &guid.entityId;

      *eid != EntityId::ENTITYID_SPDP_BUILTIN_PARTICIPANT_WRITER
        && *eid != EntityId::ENTITYID_SEDP_BUILTIN_SUBSCRIPTIONS_WRITER
        && *eid != EntityId::ENTITYID_SEDP_BUILTIN_PUBLICATIONS_WRITER
        && *eid != EntityId::ENTITYID_SEDP_BUILTIN_TOPIC_WRITER
        && *eid != EntityId::ENTITYID_P2P_BUILTIN_PARTICIPANT_MESSAGE_WRITER
    }) {
      match writer.write(data.clone(), None) {
        Ok(_) => (),
        _ => error!("Unable to write new readers info."),
      }
    }
  }

  pub fn write_topic_info(
    &self,
    writer: &mut DataWriter<
      DiscoveredTopicData,
      CDRSerializerAdapter<DiscoveredTopicData, LittleEndian>,
    >,
  ) {
    let db = self.discovery_db_read();
    let datas = db.get_all_topics();
    for data in datas {
      match writer.write(data.clone(), None) {
        Ok(_) => (),
        _ => error!("Unable to write new topic info."),
      }
    }
  }

  pub fn subscriber_qos() -> QosPolicies {
    QosPolicyBuilder::new()
      .durability(Durability::TransientLocal)
      .presentation(Presentation {
        access_scope: PresentationAccessScope::Topic,
        coherent_access: false,
        ordered_access: false,
      })
      .deadline(Deadline(Duration::DURATION_INFINITE))
      .ownership(Ownership::Shared)
      .liveliness(Liveliness::Automatic {
        lease_duration: Duration::DURATION_INFINITE,
      })
      .time_based_filter(TimeBasedFilter {
        minimum_separation: Duration::DURATION_ZERO,
      })
      .reliability(Reliability::Reliable {
        max_blocking_time: Duration::from_std(StdDuration::from_millis(100)),
      })
      .destination_order(DestinationOrder::ByReceptionTimestamp)
      .history(History::KeepAll)
      .resource_limits(ResourceLimits {
        max_instances: std::i32::MAX,
        max_samples: std::i32::MAX,
        max_samples_per_instance: std::i32::MAX,
      })
      .build()
  }

  fn discovery_db_read(&self) -> RwLockReadGuard<DiscoveryDB> {
    match self.discovery_db.read() {
      Ok(db) => db,
      Err(e) => panic!("DiscoveryDB is poisoned {:?}.", e),
    }
  }

  fn discovery_db_write(&self) -> RwLockWriteGuard<DiscoveryDB> {
    match self.discovery_db.write() {
      Ok(db) => db,
      Err(e) => panic!("DiscoveryDB is poisoned {:?}.", e),
    }
  }

  fn send_discovery_notification(&self, dntype: DiscoveryNotificationType) {
    match self.discovery_updated_sender.send(dntype) {
      Ok(_) => (),
      Err(e) => error!("Failed to send DiscoveryNotification {:?}", e),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    test::{
      shape_type::ShapeType,
      test_data::{
        spdp_subscription_msg, spdp_publication_msg, spdp_participant_msg_mod,
        create_rtps_data_message,
      },
    },
    network::{udp_listener::UDPListener, udp_sender::UDPSender},
    structure::{entity::Entity, locator::Locator},
    serialization::{cdr_serializer::to_bytes, cdr_deserializer::CDRDeserializerAdapter},
    messages::submessages::submessages::{InterpreterSubmessage, EntitySubmessage},
    messages::{
      submessages::submessage_elements::serialized_payload::{RepresentationIdentifier},
    },
  };
  use crate::{
    discovery::data_types::topic_data::TopicBuiltinTopicData,
    dds::{participant::DomainParticipant, traits::serde_adapters::DeserializerAdapter},
  };
  use crate::serialization::submessage::*;

  use std::{net::SocketAddr};
  use mio::Token;
  use speedy::{Writable, Endianness};
  use byteorder::LittleEndian;

  #[test]
  fn discovery_participant_data_test() {
    let poll = Poll::new().unwrap();
    let mut udp_listener = UDPListener::new(Token(0), "127.0.0.1", 11000);
    poll
      .register(
        udp_listener.mio_socket(),
        Token(0),
        Ready::readable(),
        PollOpt::edge(),
      )
      .unwrap();

    // sending participant data to discovery
    let udp_sender = UDPSender::new_with_random_port();
    let addresses = vec![SocketAddr::new(
      "127.0.0.1".parse().unwrap(),
      get_spdp_well_known_unicast_port(0, 0),
    )];

    let tdata = spdp_participant_msg_mod(11000);
    let msg_data = tdata
      .write_to_vec_with_ctx(Endianness::LittleEndian)
      .expect("Failed to write msg data");

    udp_sender.send_to_all(&msg_data, &addresses);

    let mut events = Events::with_capacity(10);
    poll
      .poll(&mut events, Some(StdDuration::from_secs(1)))
      .unwrap();

    let _data2 = udp_listener.get_message();
    // TODO: we should have received our own participants info decoding the actual message might be good idea
  }

  #[test]
  fn discovery_reader_data_test() {
    let participant = DomainParticipant::new(0);

    let topic = participant
      .create_topic(
        "Square",
        "ShapeType",
        &QosPolicies::qos_none(),
        TopicKind::WithKey,
      )
      .unwrap();

    let publisher = participant
      .create_publisher(&QosPolicies::qos_none())
      .unwrap();
    let _writer = publisher
      .create_datawriter::<ShapeType, CDRSerializerAdapter<ShapeType, LittleEndian>>(
        None, &topic, None,
      )
      .unwrap();

    let subscriber = participant
      .create_subscriber(&QosPolicies::qos_none())
      .unwrap();
    let _reader = subscriber
      .create_datareader::<ShapeType, CDRDeserializerAdapter<ShapeType>>(&topic, None, None);

    let poll = Poll::new().unwrap();
    let mut udp_listener = UDPListener::new(Token(0), "127.0.0.1", 11001);
    poll
      .register(
        udp_listener.mio_socket(),
        Token(0),
        Ready::readable(),
        PollOpt::edge(),
      )
      .unwrap();

    let udp_sender = UDPSender::new_with_random_port();
    let addresses = vec![SocketAddr::new(
      "127.0.0.1".parse().unwrap(),
      get_spdp_well_known_unicast_port(14, 0),
    )];

    let mut tdata = spdp_subscription_msg();
    let mut data;
    for submsg in tdata.submessages.iter_mut() {
      match &mut submsg.body {
        SubmessageBody::Entity(v) => match v {
          EntitySubmessage::Data(d, _) => {
            let mut drd: DiscoveredReaderData = PlCdrDeserializerAdapter::from_bytes(
              &d.serialized_payload.as_ref().unwrap().value,
              RepresentationIdentifier::PL_CDR_LE,
            )
            .unwrap();
            drd.reader_proxy.unicast_locator_list.clear();
            drd
              .reader_proxy
              .unicast_locator_list
              .push(Locator::from(SocketAddr::new(
                "127.0.0.1".parse().unwrap(),
                11001,
              )));
            drd.reader_proxy.multicast_locator_list.clear();

            data = to_bytes::<DiscoveredReaderData, byteorder::LittleEndian>(&drd).unwrap();
            d.serialized_payload.as_mut().unwrap().value = data.clone();
          }
          _ => continue,
        },
        SubmessageBody::Interpreter(_) => (),
      }
    }

    let msg_data = tdata
      .write_to_vec_with_ctx(Endianness::LittleEndian)
      .expect("Failed to write msg dtaa");

    udp_sender.send_to_all(&msg_data, &addresses);

    let mut events = Events::with_capacity(10);
    poll
      .poll(&mut events, Some(StdDuration::from_secs(1)))
      .unwrap();

    let _data2 = udp_listener.get_message();
  }

  #[test]
  fn discovery_writer_data_test() {
    let participant = DomainParticipant::new(0);

    let topic = participant
      .create_topic(
        "Square",
        "ShapeType",
        &QosPolicies::qos_none(),
        TopicKind::WithKey,
      )
      .unwrap();

    let publisher = participant
      .create_publisher(&QosPolicies::qos_none())
      .unwrap();
    let _writer = publisher
      .create_datawriter::<ShapeType, CDRSerializerAdapter<ShapeType, LittleEndian>>(
        None, &topic, None,
      )
      .unwrap();

    let subscriber = participant
      .create_subscriber(&QosPolicies::qos_none())
      .unwrap();
    let _reader = subscriber
      .create_datareader::<ShapeType, CDRDeserializerAdapter<ShapeType>>(&topic, None, None);

    let poll = Poll::new().unwrap();
    let mut udp_listener = UDPListener::new(Token(0), "127.0.0.1", 0);
    poll
      .register(
        udp_listener.mio_socket(),
        Token(0),
        Ready::readable(),
        PollOpt::edge(),
      )
      .unwrap();

    let udp_sender = UDPSender::new_with_random_port();
    let addresses = vec![SocketAddr::new(
      "127.0.0.1".parse().unwrap(),
      get_spdp_well_known_unicast_port(15, 0),
    )];

    let mut tdata = spdp_publication_msg();
    for submsg in tdata.submessages.iter_mut() {
      match &mut submsg.body {
        SubmessageBody::Interpreter(v) => match v {
          InterpreterSubmessage::InfoDestination(dst, _flags) => {
            dst.guid_prefix = participant.get_guid_prefix().clone();
          }
          _ => continue,
        },
        SubmessageBody::Entity(_) => (),
      }
    }

    let par_msg_data = spdp_participant_msg_mod(udp_listener.port())
      .write_to_vec_with_ctx(Endianness::LittleEndian)
      .expect("Failed to write participant data.");

    let msg_data = tdata
      .write_to_vec_with_ctx(Endianness::LittleEndian)
      .expect("Failed to write msg data");

    udp_sender.send_to_all(&par_msg_data, &addresses);
    udp_sender.send_to_all(&msg_data, &addresses);

    let mut events = Events::with_capacity(10);
    poll
      .poll(&mut events, Some(StdDuration::from_secs(1)))
      .unwrap();

    for _ in udp_listener.get_messages() {
      info!("Message received");
    }
  }

  #[test]
  fn discovery_topic_data_test() {
    let _participant = DomainParticipant::new(0);

    let topic_data = DiscoveredTopicData::new(TopicBuiltinTopicData {
      key: None,
      name: Some(String::from("Square")),
      type_name: Some(String::from("ShapeType")),
      durability: None,
      deadline: None,
      latency_budget: None,
      liveliness: None,
      reliability: None,
      lifespan: None,
      destination_order: None,
      presentation: None,
      history: None,
      resource_limits: None,
      ownership: None,
    });

    let rtps_message = create_rtps_data_message(
      topic_data,
      EntityId::ENTITYID_SEDP_BUILTIN_TOPIC_READER,
      EntityId::ENTITYID_SEDP_BUILTIN_TOPIC_WRITER,
    );

    let udp_sender = UDPSender::new_with_random_port();
    let addresses = vec![SocketAddr::new(
      "127.0.0.1".parse().unwrap(),
      get_spdp_well_known_unicast_port(16, 0),
    )];

    let rr = rtps_message
      .write_to_vec_with_ctx(Endianness::LittleEndian)
      .unwrap();

    udp_sender.send_to_all(&rr, &addresses);
  }
}
