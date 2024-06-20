use std::{collections::HashMap, path};

use async_recursion::async_recursion;
use console::Term;
use dco3::{
    auth::Connected,
    nodes::{Node, NodeList, NodeType, NodesFilter},
    Dracoon, ListAllParams, Nodes, RangedItems, Rooms,
};
use models::{NodeId, RoomsChannel};
use tokio::{sync::mpsc, task::JoinSet};
use tracing_subscriber::fmt::format;

use crate::cmd::{
    init_dracoon,
    models::{DcCmdError, PasswordAuth},
    utils::strings::parse_path,
};

use super::import::models::{GroupRoomPermission, Room, RoomPolicies, UserRoomPermission};

pub mod models;

pub async fn export_room_structure(
    term: Term,
    source: String,
    target: String,
    auth: Option<PasswordAuth>,
) -> Result<(), DcCmdError> {
    let dracoon = init_dracoon(&source, auth, false).await?;
    let (parent_id, path) =
        validate_node_path_and_get_parent_id(&source, &dracoon, term.clone()).await?;
    let current_user = dracoon.get_user_info().await?;
    let mut rooms_channel = RoomsChannel::new();

    // todo implement in dco3
    let home_room_parent_name = "Benutzerdatenräume".to_string();

    get_all_rooms(
        term.clone(),
        dracoon.clone(),
        parent_id,
        path,
        Some(home_room_parent_name),
        rooms_channel.get_sender(),
    )
    .await?;

    rooms_channel.collect().await;

    let room_map = rooms_channel.get_rooms_map();

    // Channel could also handle updating room with user and group permissions
    let room_map =
        update_rooms_map_with_user_perms(term.clone(), dracoon.clone(), room_map, current_user.id)
            .await?;
    let room_map =
        update_rooms_map_with_group_perms(term.clone(), dracoon.clone(), &room_map).await?;

    let room_map = update_rooms_with_perms(term.clone(), dracoon.clone(), &room_map).await?;

    let rooms = build_room_hierarchy(room_map);

    write_to_json_file(&rooms, &target).unwrap();

    Ok(())
}

async fn validate_node_path_and_get_parent_id(
    source: &str,
    dracoon: &Dracoon<Connected>,
    _term: Term,
) -> Result<(Option<u64>, String), DcCmdError> {
    let (parent_path, node_name, _) = parse_path(source, dracoon.get_base_url().as_ref())?;
    let mut parent_id = None;
    let path = format!("{}{}", &parent_path, &node_name);

    // check if root
    if !node_name.is_empty() {
        let parent_node = dracoon
            .nodes
            .get_node_from_path(&path)
            .await?
            .ok_or(DcCmdError::InvalidPathOrNoPermission(source.to_owned()))?;

        if parent_node.node_type != NodeType::Room {
            return Err(DcCmdError::InvalidPath(source.to_owned()));
        }

        parent_id = Some(parent_node.id);
        Ok((parent_id, path))
    } else {
        Ok((parent_id, path))
    }
}

#[async_recursion]
async fn get_all_rooms(
    term: Term,
    dracoon: Dracoon<Connected>,
    parent_id: Option<u64>,
    _path: String,
    home_room_parent_name: Option<String>,
    rooms_channel_sender: mpsc::Sender<Node>,
) -> Result<(), DcCmdError> {
    let mut room_tasks: JoinSet<Result<(), DcCmdError>> = JoinSet::new();

    let params = ListAllParams::builder()
        .with_filter(NodesFilter::is_room())
        .build();

    if parent_id.is_some() {
        let room = dracoon
            .nodes
            .get_node(parent_id.expect("should have parent id"))
            .await?;
        let _ = rooms_channel_sender.send(room).await;
    }

    let rooms = dracoon
        .nodes
        .get_nodes(parent_id, None, Some(params))
        .await?;

    for room in rooms {
        if room.name == home_room_parent_name.clone().unwrap_or("".to_string()) {
            continue;
        }

        let dracoon = dracoon.clone();
        let term = term.clone();
        let path = format!(
            "{}{}",
            room.parent_path.unwrap_or("".to_string()),
            room.name
        );
        let rooms_channel_sender = rooms_channel_sender.clone();
        room_tasks.spawn(async move {
            get_all_rooms(
                term,
                dracoon,
                Some(room.id),
                path,
                None,
                rooms_channel_sender,
            )
            .await
        });
    }

    while let Some(res) = room_tasks.join_next().await {
        match res {
            Ok(task_res) => match task_res {
                Ok(_) => {}
                Err(_) => {}
            },
            Err(_) => {}
        }
    }

    Ok(())
}

async fn update_rooms_map_with_user_perms(
    _term: Term,
    dracoon: Dracoon<Connected>,
    rooms_map: &HashMap<NodeId, Room>,
    current_user_id: u64,
) -> Result<HashMap<NodeId, Room>, DcCmdError> {
    let mut new_rooms_map: HashMap<NodeId, Room> = HashMap::new();
    for (node_id, room) in rooms_map {
        let mut new_room = room.clone();
        let mut user_permissions: Vec<UserRoomPermission> = Vec::new();
        let mut admin_ids: Vec<u64> = Vec::new();
        let res = dracoon.nodes.get_room_users(node_id.0, None).await;

        match res {
            Ok(ranged_room_users) => {
                for room_user in ranged_room_users.items {
                    if room_user.permissions.is_some()
                        && room_user.clone().permissions.unwrap().manage
                    {
                        admin_ids.push(room_user.user_info.id as u64)
                    } else {
                        user_permissions.push(UserRoomPermission {
                            id: room_user.user_info.id as u64,
                            permissions: room_user.permissions.unwrap(),
                        })
                    }
                }
            }
            Err(_) => {
                // to save rooms with faulty permissions to change them manually
                // at least one admin is needed
                admin_ids.push(current_user_id);
            }
        }

        if user_permissions.len() > 0 {
            new_room.user_permissions = Some(user_permissions);
        }
        new_room.admin_ids = Some(admin_ids);
        new_rooms_map.insert(node_id.clone(), new_room);
    }
    Ok(new_rooms_map)
}

async fn update_rooms_map_with_group_perms(
    _term: Term,
    dracoon: Dracoon<Connected>,
    rooms_map: &HashMap<NodeId, Room>,
) -> Result<HashMap<NodeId, Room>, DcCmdError> {
    let mut new_rooms_map: HashMap<NodeId, Room> = HashMap::new();
    for (node_id, room) in rooms_map {
        let mut new_room = room.clone();
        let mut group_permissions: Vec<GroupRoomPermission> = Vec::new();
        let mut admin_group_ids: Vec<u64> = Vec::new();
        let res = dracoon.nodes.get_room_groups(node_id.0, None).await;

        match res {
            Ok(ranged_room_groups) => {
                for room_group in ranged_room_groups.items {
                    if room_group.permissions.is_some()
                        && room_group.clone().permissions.unwrap().manage
                    {
                        admin_group_ids.push(room_group.id)
                    } else {
                        group_permissions.push(GroupRoomPermission {
                            id: room_group.id as u64,
                            new_group_member_acceptance: room_group.new_group_member_acceptance,
                            permissions: room_group.permissions.unwrap(),
                        })
                    }
                }

                new_room.admin_group_ids = Some(admin_group_ids);
                if group_permissions.len() > 0 {
                    new_room.group_permissions = Some(group_permissions);
                }
            }
            Err(_) => {
                // save rooms with faulty permissions to change them manually
            }
        }
        new_rooms_map.insert(node_id.clone(), new_room);
    }
    Ok(new_rooms_map)
}

async fn update_rooms_with_perms(
    _term: Term,
    dracoon: Dracoon<Connected>,
    rooms_map: &HashMap<NodeId, Room>,
) -> Result<HashMap<NodeId, Room>, DcCmdError> {
    let mut new_rooms_map: HashMap<NodeId, Room> = HashMap::new();
    for (node_id, room) in rooms_map {
        let mut new_room = room.clone();
        let room_policies = dracoon.nodes.get_room_policies(node_id.0).await;

        match room_policies {
            Ok(room_policies) => {
                new_room.policies = Some(RoomPolicies {
                    default_expiration_period: Some(room_policies.default_expiration_period),
                    is_virus_protection_enabled: Some(room_policies.is_virus_protection_enabled),
                });
            }
            Err(_) => {
                // save rooms with faulty policies to change them manually
            }
        }
        new_rooms_map.insert(node_id.clone(), new_room);
    }
    Ok(new_rooms_map)
}

// Helper function to find a room by name within a list of rooms
fn find_room_by_name<'a>(rooms: &'a mut Vec<Room>, name: &'a str) -> Option<&'a mut Room> {
    rooms.iter_mut().find(|room| room.name == name)
}

// Helper function to insert a room into the hierarchy
fn insert_room<'a>(
    rooms: &mut Vec<Room>,
    path: &[&'a str],
    room: Room,
) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("Path cannot be empty");
    }

    if path.len() == 1 {
        rooms.push(room);
        Ok(())
    } else {
        let parent_name = path[0];
        if let Some(parent_room) = find_room_by_name(rooms, parent_name) {
            if let Some(sub_rooms) = parent_room.sub_rooms.as_mut() {
                insert_room(sub_rooms, &path[1..], room)
            } else {
                parent_room.sub_rooms = Some(vec![]);
                insert_room(parent_room.sub_rooms.as_mut().unwrap(), &path[1..], room)
            }
        } else {
            Err("Parent room not found: {}")
        }
    }
}

// Function to build the room hierarchy
fn build_room_hierarchy(rooms: HashMap<NodeId, Room>) -> Vec<Room> {
    let mut room_vec: Vec<Room> = rooms.into_values().collect();

    // Sort by the complete_path lexicographically
    room_vec.sort_by_key(|room| room.complete_path.clone());

    let mut top_level_rooms = Vec::new();

    for room in room_vec {
        if let Some(ref path) = room.complete_path {
            let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
            if let Err(err) = insert_room(&mut top_level_rooms, &path_parts, room.clone()) {
                eprintln!("Error inserting room: {}", err);
            }
        }
    }

    top_level_rooms
}

// Function to write Vec<Room> to a JSON file
fn write_to_json_file(rooms: &Vec<Room>, file_path: &str) -> std::io::Result<()> {
    let json_data = serde_json::to_string_pretty(rooms)?;
    std::fs::write(file_path, json_data)
}
