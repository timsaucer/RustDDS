extern crate atosdds;
extern crate mio;
extern crate mio_extras;

use std::{io::Write};

use atosdds::{
  dds::{
    participant::DomainParticipant, readcondition::ReadCondition,
    no_key::datareader::DataReader as NoKeyDataReader,
  },
  serialization::cdrDeserializer::CDR_deserializer_adapter,
  structure::entity::Entity,
};
use log::{error};
use mio::{Events, Poll, PollOpt, Ready, Token};
use mio_extras::{channel as mio_channel};
use ros2::{node_control::NodeControl, turtle_control::TurtleControl, turtle_data::Twist};
use ros_data::{Gid, NodeInfo, ROSParticipantInfo};
use structures::{MainController, DataUpdate, RosCommand};
use termion::raw::IntoRawMode;
use log4rs;

// modules
mod ros2;
mod ros_data;
mod structures;

const ROS2_COMMAND_TOKEN: Token = Token(1000);
const ROS2_NODE_RECEIVED_TOKEN: Token = Token(1001);
const TURTLE_CMD_VEL_RECEIVER_TOKEN: Token = Token(1002);

fn main() {
  log4rs::init_file("log4rs.yaml", Default::default()).unwrap();

  let (command_sender, command_receiver) = mio_channel::sync_channel::<RosCommand>(10);

  let stdout_org = std::io::stdout();
  let mut stdout = stdout_org.lock().into_raw_mode().unwrap();
  // clearing screen
  write!(
    stdout,
    "{}{}",
    termion::clear::All,
    termion::cursor::Goto(1, 1)
  )
  .unwrap();
  stdout.flush().unwrap();

  let jhandle = std::thread::spawn(move || ros2_loop(command_receiver));

  let mut main_control = MainController::new(stdout, command_sender.clone());
  main_control.start();

  jhandle.join().unwrap();
}

fn ros2_loop(command_receiver: mio_channel::Receiver<RosCommand>) {
  let domain_participant = DomainParticipant::new(0, 15);
  let node_control = NodeControl::new(domain_participant.clone());

  let mut node_reader = node_control.get_node_reader();
  let mut node_writer = node_control.get_node_writer();
  // only to say that we have a parameter events writer
  let parameter_events_writer = node_control.get_parameter_events_writer();
  let _rosout_writer = node_control.get_rosout_writer();

  // turtle ops
  let turtle_control = TurtleControl::new(domain_participant.clone());
  let mut turtle_cmd_vel_reader = turtle_control.get_cmd_vel_reader();
  let mut turtle_cmd_vel_writer = turtle_control.get_cmd_vel_writer();

  let poll = Poll::new().unwrap();

  let mut nodes = Vec::new();
  let node_info = NodeInfo {
    node_namespace: String::from("/"),
    node_name: String::from("ros2_demo_turtle_node"),
    reader_guid: vec![
      Gid::from_guid(node_reader.get_guid()),
      Gid::from_guid(turtle_cmd_vel_reader.get_guid()),
    ],
    writer_guid: vec![
      Gid::from_guid(node_writer.get_guid()),
      Gid::from_guid(parameter_events_writer.get_guid()),
      Gid::from_guid(turtle_cmd_vel_writer.get_guid()),
    ],
  };
  nodes.push(node_info);
  let pinfo = ROSParticipantInfo::new(Gid::from_guid(domain_participant.get_guid()), nodes);

  poll
    .register(
      &command_receiver,
      ROS2_COMMAND_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    )
    .unwrap();

  poll
    .register(
      &node_reader,
      ROS2_NODE_RECEIVED_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    )
    .unwrap();

  poll
    .register(
      &turtle_cmd_vel_reader,
      TURTLE_CMD_VEL_RECEIVER_TOKEN,
      Ready::readable(),
      PollOpt::edge(),
    )
    .unwrap();

  // senders
  let mut nodes_updated_sender: Option<mio_channel::SyncSender<DataUpdate>> = None;

  loop {
    let mut events = Events::with_capacity(100);
    poll.poll(&mut events, None).unwrap();

    for event in events.iter() {
      if event.token() == ROS2_COMMAND_TOKEN {
        while let Ok(command) = command_receiver.try_recv() {
          match command {
            RosCommand::StopEventLoop => {
              return;
            }
            RosCommand::UpdateNode => {
              match node_writer.write(pinfo.clone(), None) {
                Ok(_) => (),
                Err(e) => error!("Failed to write into node_writer {:?}", e),
              };
            }
            RosCommand::AddNodeListSender { sender } => nodes_updated_sender = Some(sender),
            RosCommand::TurtleCmdVel { twist } => {
              turtle_cmd_vel_writer.write(twist, None).unwrap();
            }
          };
        }
      } else if event.token() == ROS2_NODE_RECEIVED_TOKEN {
        match &nodes_updated_sender {
          Some(s) => handle_node_reader(&mut node_reader, s),
          None => (),
        }
      } else if event.token() == TURTLE_CMD_VEL_RECEIVER_TOKEN {
        match &nodes_updated_sender {
          Some(s) => handle_turtle_cmd_vel_reader(&mut turtle_cmd_vel_reader, s),
          None => (),
        }
      }
    }
  }
}

fn handle_node_reader<'a>(
  node_reader: &mut NoKeyDataReader<
    'a,
    ROSParticipantInfo,
    CDR_deserializer_adapter<ROSParticipantInfo>,
  >,
  sender: &mio_channel::SyncSender<DataUpdate>,
) {
  if let Ok(data) = node_reader.read(100, ReadCondition::not_read()) {
    data.iter().for_each(|p| {
      match p.sample_info.instance_state {
        atosdds::dds::datasample::InstanceState::Alive => sender
          .send(DataUpdate::UpdateNode {
            info: p.value.clone(),
          })
          .unwrap(),
        atosdds::dds::datasample::InstanceState::NotAlive_Disposed
        | atosdds::dds::datasample::InstanceState::NotAlive_NoWriters => sender
          .send(DataUpdate::DeleteNode {
            guid: p.sample_info.publication_handle,
          })
          .unwrap(),
      };
    });
  }
}

fn handle_turtle_cmd_vel_reader<'a>(
  turle_cmd_vel_reader: &mut NoKeyDataReader<'a, Twist, CDR_deserializer_adapter<Twist>>,
  sender: &mio_channel::SyncSender<DataUpdate>,
) {
  if let Ok(data) = turle_cmd_vel_reader.read(100, ReadCondition::not_read()) {
    data.iter().for_each(|p| {
      sender
        .send(DataUpdate::TurtleCmdVel {
          twist: p.value.clone(),
        })
        .unwrap()
    });
  }
}