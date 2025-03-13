use std::{
  collections::HashMap,
  sync::{Arc, Mutex},
};

use simple_websockets::{Event, EventHub, Message, Responder};

use crate::{
  cmd::{ActivityCmd, ActivityPayload},
  log,
};

use super::process::ProcessDetectedEvent;

fn empty_activity(pid: u64, socket_id: String) -> String {
  format!(
    r#"
    {{
      "activity": null,
      "pid": {},
      "socketId": "{}"
    }}
  "#,
    pid, socket_id
  )
}

#[derive(Clone)]
pub struct ClientConnector {
  pub port: u16,
  server: Arc<Mutex<EventHub>>,
  pub clients: Arc<Mutex<HashMap<u64, Responder>>>,
  data_on_connect: String,

  pub last_pid: Arc<Mutex<Option<u64>>>,
  pub active_socket: Arc<Mutex<Option<String>>>,

  pub ipc_event_rec: Arc<Mutex<std::sync::mpsc::Receiver<ActivityCmd>>>,
  pub proc_event_rec: Arc<Mutex<std::sync::mpsc::Receiver<ProcessDetectedEvent>>>,
  pub ws_event_rec: Arc<Mutex<std::sync::mpsc::Receiver<ActivityCmd>>>,
}

impl ClientConnector {
  pub fn new(
    port: u16,
    data_on_connect: String,
    ipc_event_rec: std::sync::mpsc::Receiver<ActivityCmd>,
    proc_event_rec: std::sync::mpsc::Receiver<ProcessDetectedEvent>,
    ws_event_rec: std::sync::mpsc::Receiver<ActivityCmd>,
  ) -> ClientConnector {
    ClientConnector {
      server: Arc::new(Mutex::new(simple_websockets::launch(port).unwrap_or_else(
        |_| {
          log!("[Client Connector] Failed to launch websocket server, port may already be in use");
          std::process::exit(1);
        },
      ))),
      clients: Arc::new(Mutex::new(HashMap::new())),
      data_on_connect,
      port,

      last_pid: Arc::new(Mutex::new(None)),
      active_socket: Arc::new(Mutex::new(None)),

      ipc_event_rec: Arc::new(Mutex::new(ipc_event_rec)),
      proc_event_rec: Arc::new(Mutex::new(proc_event_rec)),
      ws_event_rec: Arc::new(Mutex::new(ws_event_rec)),
    }
  }

  pub fn start(&self) {
    let clone = self.clone();
    let clients_clone = self.clients.clone();

    std::thread::spawn(move || {
      loop {
        match clone.server.lock().unwrap().poll_event() {
          Event::Connect(client_id, responder) => {
            log!("[Client Connector] Client {} connected", client_id);
            // Send initial connection data
            responder.send(Message::Text(clone.data_on_connect.clone()));
            clients_clone.lock().unwrap().insert(client_id, responder);
          }
          Event::Disconnect(client_id) => {
            clients_clone.lock().unwrap().remove(&client_id);
          }
          Event::Message(client_id, message) => {
            log!(
              "[Client Connector] Received message from client {}: {:?}",
              client_id,
              message
            );
            let responder = clients_clone.lock().unwrap();
            let responder = responder.get(&client_id).unwrap();
            responder.send(message);
          }
        }
      }
    });

    // Create a thread for each reciever
    let mut ipc_clone = self.clone();
    let mut proc_clone = self.clone();
    let mut ws_clone = self.clone();

    std::thread::spawn(move || {
      loop {
        let mut ipc_activity = ipc_clone.ipc_event_rec.lock().unwrap().recv().unwrap();

        // if there are no client, skip
        if ipc_clone.clients.lock().unwrap().len() == 0 {
          log!("[Client Connector] No clients connected, skipping");
          continue;
        }

        ipc_activity.fix();

        let mut args = match ipc_activity.args {
          Some(args) => args,
          None => {
            log!("[Client Connector] Invalid activity command, skipping");
            continue;
          }
        };

        if args.activity.is_none() {
          let pid = args.pid.unwrap_or_default();
          // Send empty payload
          let payload = empty_activity(pid, pid.to_string());
          log!("[Client Connector] Sending empty payload");
          ipc_clone.send_data(payload);
          continue;
        }

        let activity = args.activity.as_mut();

        if let Some(activity) = activity {
          activity.application_id = ipc_activity.application_id;

          let payload = ActivityPayload {
            activity: Some(activity.clone()),
            pid: args.pid,
            socket_id: Some(args.pid.unwrap_or(0).to_string()),
          };

          match serde_json::to_string(&payload) {
            Ok(payload) => {
              log!(
                "[Client Connector] Sending payload for IPC activity: {:?}",
                payload
              );
              ipc_clone.send_data(payload)
            }
            Err(err) => log!("[Client Connector] Error serializing IPC activity: {}", err),
          };
        } else {
          log!("[Client Connector] Invalid activity command, skipping");
        }
      }
    });

    std::thread::spawn(move || {
      loop {
        let proc_event = proc_clone.proc_event_rec.lock().unwrap().recv().unwrap();
        let proc_activity = proc_event.activity;

        // if there are no clients, skip
        if proc_clone.clients.lock().unwrap().len() == 0 {
          log!("[Client Connector] No clients connected, skipping");
          continue;
        }

        if proc_activity.id == "null" {
          // If our last socket id is empty, skip
          if proc_clone.active_socket.lock().unwrap().is_none() {
            continue;
          }
          // Send an empty payload
          log!("[Client Connector] Sending empty payload");
          let payload = empty_activity(
            (*proc_clone.last_pid.lock().unwrap()).unwrap_or_default(),
            proc_clone.active_socket.lock().unwrap().clone().unwrap_or_default(),
          );
          proc_clone.send_data(payload);
          *proc_clone.active_socket.lock().unwrap() = None;
          continue;
        }

        // If the active socket is different from the current socket, send an empty payload for the old socket
        if *proc_clone.active_socket.lock().unwrap() != Some(proc_activity.id.clone()) {
          if proc_clone.active_socket.lock().unwrap().is_some() {
            // Send an empty payload
            log!("[Client Connector] Sending empty payload");
            let payload = empty_activity(
              (*proc_clone.last_pid.lock().unwrap()).unwrap_or_default(),
              proc_clone.active_socket.lock().unwrap().clone().unwrap_or_default(),
            );
            proc_clone.send_data(payload);
          }
        } else {
          log!(
            "[Client Connector] Already sent payload for activity: {}",
            proc_activity.name
          );
          continue;
        }

        let payload = format!(
          // I don't even know what half of these fields are for yet
          r#"
          {{
            "activity": {{
              "application_id": "{}",
              "name": "{}",
              "timestamps": {{
                "start": {}
              }},
              "type": 0,
              "metadata": {{}},
              "flags": 0
            }},
            "pid": {},
            "socketId": "{}"
          }}
          "#,
          proc_activity.id,
          proc_activity.name,
          proc_activity.timestamp.as_ref().unwrap_or(&"0".to_string()),
          proc_activity.pid.unwrap_or_default(),
          proc_activity.id
        );

        *proc_clone.last_pid.lock().unwrap() = proc_activity.pid;
        *proc_clone.active_socket.lock().unwrap() = Some(proc_activity.id.clone());

        log!(
          "[Client Connector] Sending payload for activity: {}",
          proc_activity.name
        );

        proc_clone.send_data(payload);
      }
    });

    std::thread::spawn(move || {
      loop {
        let mut ws_event = ws_clone.ws_event_rec.lock().unwrap().recv().unwrap();

        // if there are no clients, skip
        if ws_clone.clients.lock().unwrap().len() == 0 {
          log!("[Client Connector] No clients connected, skipping");
          continue;
        }

        if ws_event.cmd != "SET_ACTIVITY" {
          let payload = serde_json::to_string(&ws_event).unwrap_or("".to_string());
          log!("[Client Connector] Sending payload for WS event");
          ws_clone.send_data(payload);
          continue;
        }

        ws_event.fix();

        let mut args = match ws_event.args {
          Some(args) => args,
          None => {
            log!("[Client Connector] Invalid activity command, skipping");
            continue;
          }
        };

        if args.activity.is_none() {
          let pid = args.pid.unwrap_or_default();
          let payload = empty_activity(pid, pid.to_string());
          log!("[Client Connector] Sending empty payload");
          ws_clone.send_data(payload);
          continue;
        }

        let activity = args.activity.as_mut();

        if let Some(activity) = activity {
          activity.application_id = ws_event.application_id;

          let payload = ActivityPayload {
            activity: Some(activity.clone()),
            pid: args.pid,
            socket_id: Some(args.pid.unwrap_or(0).to_string()),
          };

          match serde_json::to_string(&payload) {
            Ok(payload) => {
              log!(
                "[Client Connector] Sending payload for IPC activity: {:?}",
                payload
              );
              ws_clone.send_data(payload)
            }
            Err(err) => log!("[Client Connector] Error serializing IPC activity: {}", err),
          };
        } else {
          log!("[Client Connector] Invalid activity command, skipping");
        }
      }
    });
  }

  pub fn send_data(&self, data: String) {
    // Send data to all clients
    for (_, responder) in self.clients.lock().unwrap().iter() {
      responder.send(Message::Text(data.clone()));
    }
  }
}

impl Drop for ClientConnector {
  fn drop(&mut self) {
    drop(self.server.lock().unwrap());
  }
}
