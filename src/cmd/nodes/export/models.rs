use std::collections::HashMap;

use dco3::nodes::Node;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::cmd::nodes::import::models::Room;

#[derive(Eq, Hash, PartialEq, Clone)]
pub struct NodeId(pub u64);

pub struct RoomsChannel {
    rx: mpsc::Receiver<Node>,
    tx: mpsc::Sender<Node>,
    pub collected_rooms: HashMap<NodeId, Room>,
}

static CAPACITY: usize = 50000;

impl RoomsChannel {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(CAPACITY);

        Self {
            rx,
            tx,
            collected_rooms: HashMap::new(),
        }
    }

    pub fn get_sender(&self) -> mpsc::Sender<Node> {
        self.tx.clone()
    }

    pub async fn collect(&mut self) {
        if self.tx.capacity() == CAPACITY {
            debug!("No rooms to collect.");
            return;
        }

        while let Some(room) = self.rx.recv().await {
            let capacity = self.tx.capacity();
            self.collected_rooms
                .insert(NodeId(room.id), Room::from(room));

            if capacity == CAPACITY {
                info!("All rooms collected. Closing channel.");
                break;
            }
        }
        self.shutdown();
    }

    pub fn get_rooms_map(&self) -> &HashMap<NodeId, Room> {
        &self.collected_rooms
    }

    pub fn shutdown(&mut self) {
        debug!("Shutting down reciever for rooms");
        self.rx.close();
    }
}
