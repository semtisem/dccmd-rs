use std::io::Write;

use async_recursion::async_recursion;
use console::Term;
use dialoguer::Confirm;
use futures_util::future::join_all;
use tracing::{debug, info, error};

use crate::cmd::{
    init_dracoon,
    utils::strings::{build_node_path, parse_path, print_node},
};

use dco3::{
    auth::Connected,
    models::ListAllParams,
    nodes::{
        models::{CreateFolderRequest, NodeList, NodeType}, rooms::models::CreateRoomRequest, Folders, GroupMemberAcceptance, Node, Nodes, RoomGroupsAddBatchRequestItem, RoomUsersAddBatchRequest, RoomUsersAddBatchRequestItem, Rooms
    },
    Dracoon,
};

use self::models::Room;

use super::{
    models::{DcCmdError, PasswordAuth},
    utils::strings::{format_error_message, format_success_message},
};

pub mod download;
pub mod upload;
pub mod models;

#[allow(clippy::too_many_arguments, clippy::module_name_repetitions)]
pub async fn list_nodes(
    term: Term,
    source: String,
    long: Option<bool>,
    human_readable: Option<bool>,
    managed: Option<bool>,
    all: Option<bool>,
    offset: Option<u32>,
    limit: Option<u32>,
    auth: Option<PasswordAuth>
) -> Result<(), DcCmdError> {
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(500);

    let dracoon = init_dracoon(&source, auth, false).await?;

    let (parent_path, node_name, depth) = parse_path(&source, dracoon.get_base_url().as_ref())?;
    let node_path = build_node_path((parent_path.clone(), node_name.clone(), depth));

    let all = all.unwrap_or(false);

    // only provide a path if not the root node
    let node_path = if node_path == "//" {
        None
    } else {
        Some(node_path.as_str())
    };

    let node_list = if is_search_query(&node_name) {
        debug!("Searching for nodes with query {}", node_name);
        search_nodes(
            &dracoon,
            &node_name,
            Some(&parent_path),
            managed,
            all,
            offset,
            limit,
        )
        .await?
    } else {
        debug!("Fetching node list from path {}", node_path.unwrap_or("/"));
        get_nodes(&dracoon, node_path, managed, all, offset, limit).await?
    };

    node_list
        .items
        .iter()
        .for_each(|node| print_node(&term, node, long, human_readable));

    info!("Listed nodes in: {}", node_path.unwrap_or("/"));
    info!("Total nodes: {}", node_list.range.total);
    info!("Offset: {}", node_list.range.offset);
    info!("Limit: {}", node_list.range.limit);

    Ok(())
}

fn is_search_query(query: &str) -> bool {
    query.contains('*')
}

async fn get_nodes(
    dracoon: &Dracoon<Connected>,
    node_path: Option<&str>,
    managed: Option<bool>,
    all: bool,
    offset: u32,
    limit: u32,
) -> Result<NodeList, DcCmdError> {
    let parent_id = if let Some(node_path) = node_path {
        let node = dracoon.get_node_from_path(node_path).await?;

        let Some(node) = node else {
                return Err(DcCmdError::InvalidPath(node_path.to_string()))
            };

        Some(node.id)
    } else {
        None
    };

    let params = ListAllParams::builder()
        .with_offset(offset.into())
        .with_limit(limit.into())
        .build();

    let mut node_list = dracoon.get_nodes(parent_id, managed, Some(params)).await?;

    if all && node_list.range.total > 500 {
        let mut offset = 500;
        let limit = 500;

        while offset < node_list.range.total {
            let mut futures = vec![];

            while offset < node_list.range.total {
                let params = ListAllParams::builder()
                    .with_offset(offset)
                    .with_limit(limit)
                    .build();

                let next_node_list_req = dracoon.get_nodes(parent_id, managed, Some(params));
                futures.push(next_node_list_req);
                offset += limit;
            }

            let mut next_node_list_items = vec![];

            let results = join_all(futures).await;
            for result in results {
                let next_node_list = result?.items;
                next_node_list_items.extend(next_node_list);
            }
            node_list.items.append(&mut next_node_list_items);
        }
    }
    Ok(node_list)
}

async fn search_nodes(
    dracoon: &Dracoon<Connected>,
    search_string: &str,
    node_path: Option<&str>,
    managed: Option<bool>,
    all: bool,
    offset: u32,
    limit: u32,
) -> Result<NodeList, DcCmdError> {
    let parent_id = if let Some(node_path) = node_path {
        let node = dracoon.get_node_from_path(node_path).await?;

        let Some(node) = node else {
                    return Err(DcCmdError::InvalidPath(node_path.to_string()))
                };

        Some(node.id)
    } else {
        None
    };

    let params = ListAllParams::builder()
        .with_offset(offset.into())
        .with_limit(limit.into())
        .build();

    let mut node_list = dracoon
        .search_nodes(search_string, parent_id, Some(0), Some(params))
        .await?;

    if all && node_list.range.total > 500 {
        let mut offset = 500;
        let limit = 500;

        while offset < node_list.range.total {
            let mut futures = vec![];

            while offset < node_list.range.total {
                let params = ListAllParams::builder()
                    .with_offset(offset)
                    .with_limit(limit)
                    .build();

                let next_node_list_req =
                    dracoon.search_nodes(search_string, parent_id, Some(0), Some(params));
                futures.push(next_node_list_req);
                offset += limit;
            }

            let mut next_node_list_items = vec![];

            let results = join_all(futures).await;
            for result in results {
                let next_node_list = result?.items;
                next_node_list_items.extend(next_node_list);
            }
            node_list.items.append(&mut next_node_list_items);
        }
    }
    Ok(node_list)
}

pub async fn delete_node(
    term: Term,
    source: String,
    recursive: Option<bool>,
    auth: Option<PasswordAuth>
) -> Result<(), DcCmdError> {
    let dracoon = init_dracoon(&source, auth, false).await?;
    let (parent_path, node_name, depth) = parse_path(&source, dracoon.get_base_url().as_ref())?;
    let node_path = build_node_path((parent_path.clone(), node_name.clone(), depth));
    let node = dracoon
        .get_node_from_path(&node_path)
        .await?
        .ok_or(DcCmdError::InvalidPath(source.clone()))?;

    // check if recursive flag is set
    let recursive = recursive.unwrap_or(false);

    // if node type is folder or room and not recursive, abort
    if !recursive && (node.node_type == NodeType::Folder || node.node_type == NodeType::Room) {
        let msg = format_error_message("Deleting non-empty folder or room not allowed. Use --recursive flag to delete recursively.");
        error!("{}", msg);
        term.write_line(&msg)
            .expect("Error writing message to terminal.");
        return Ok(());
    }

    // define async block to delete node
    let delete_node = async {
        dracoon.delete_node(node.id).await?;
        let msg = format!("Node {node_name} deleted.");
        info!("{}", msg);
        let msg = format_success_message(&msg);
        term.write_line(&msg)
            .expect("Error writing message to terminal.");
        Ok(())
    };

    // check if node is a room
    match node.node_type {
        NodeType::Room => {
            // ask for confirmation if node is a room
            let confirmed = Confirm::new()
                .with_prompt(format!("Do you really want to delete room {node_name}?"))
                .interact()
                .expect("Error reading user input.");

            if confirmed {
                delete_node.await
            } else {
                let msg = format_error_message("Deleting room not confirmed.");
                error!("{}", msg);
                term.write_line(&msg)
                    .expect("Error writing message to terminal.");
                Ok(())
            }
        }
        _ => delete_node.await,
    }
}

pub async fn create_folder(
    term: Term,
    source: String,
    classification: Option<u8>,
    notes: Option<String>,
    auth: Option<PasswordAuth>
) -> Result<(), DcCmdError> {
    let dracoon = init_dracoon(&source, auth, false).await?;
    let (parent_path, node_name, _) = parse_path(&source, dracoon.get_base_url().as_ref())?;

    debug!("parent_path: {}", parent_path);
    debug!("base_url: {}", dracoon.get_base_url().as_ref());

    let parent_node = dracoon
        .get_node_from_path(&parent_path)
        .await?
        .ok_or(DcCmdError::InvalidPath(source.clone()))?;

    let req = CreateFolderRequest::builder(node_name.clone(), parent_node.id);

    let req = match classification {
        Some(classification) => req.with_classification(classification),
        None => req,
    };

    let req = match notes {
        Some(notes) => req.with_notes(notes),
        None => req,
    };

    let req = req.build();

    let folder = dracoon.create_folder(req).await?;

    let msg = format!("Folder {node_name} created.");
    info!("{}", msg);
    let msg = format_success_message(&msg);
    term.write_line(&msg)
        .expect("Error writing message to terminal.");

    Ok(())
}

pub async fn create_room(
    term: Term,
    source: String,
    classification: Option<u8>,
    auth: Option<PasswordAuth>
) -> Result<(), DcCmdError> {
    let dracoon = init_dracoon(&source, auth, false).await?;
    let (parent_path, node_name, _) = parse_path(&source, dracoon.get_base_url().as_ref())?;

    let parent_node = dracoon
        .get_node_from_path(&parent_path)
        .await?
        .ok_or(DcCmdError::InvalidPath(source.clone()))?;

    if parent_node.node_type != NodeType::Room {
        return Err(DcCmdError::InvalidPath(source.clone()));
    }

    let classification = classification.unwrap_or(2);

    let req = CreateRoomRequest::builder(&node_name.clone())
        .with_parent_id(parent_node.id)
        .with_classification(classification)
        .with_inherit_permissions(true)
        .build();

    let room = dracoon.create_room(req).await?;

    let msg = format!("Room {node_name} created.");
    info!("{}", msg);
    let msg = format_success_message(&msg);
    term.write_line(&msg)
        .expect("Error writing message to terminal.");

    Ok(())
}

pub async fn create_room_structure(
    term: Term,
    source: String,
    classification: Option<u8>,
    path: String,
    auth: Option<PasswordAuth>
) -> Result<(), DcCmdError> {
    let room_struct = Room::from_path(path)?;
    let dracoon = init_dracoon(&source, auth, false).await?;
    let (parent_path, node_name, _) = parse_path(&source, dracoon.get_base_url().as_ref())?;

    let path = parent_path.clone() + &node_name;

    let parent_node = dracoon
        .get_node_from_path(&path)
        .await?
        .ok_or(DcCmdError::InvalidPath(source.clone()))?;

    if parent_node.node_type != NodeType::Room {
        return Err(DcCmdError::InvalidPath(source.clone()));
    }

    info!("Creating room structure in room: {}", parent_node.id);
    term.write_line(&std::format!("Creating room structure at path: {}/", &path))
        .expect("Error writing message to terminal.");

    Ok(create_rooms_and_subrooms(&term, &dracoon, parent_node.id, room_struct, path).await?)

    

}

#[async_recursion]
async fn create_rooms_and_subrooms(
    term: &Term,
    dracoon: &Dracoon<Connected>,
    parent_id: u64,
    room_struct: Vec<Room>,
    path: String
) -> Result<(), DcCmdError> {
    for room in room_struct {
        let mut req = CreateRoomRequest::builder(&room.name)
            .with_parent_id(parent_id)
            .with_classification(room.classification.unwrap_or(2));

        if let Some(inherit_permissions) = room.inherit_permissions {
            req = req.with_inherit_permissions(inherit_permissions);
        };
        
        if let Some(quota) = room.quota {
            req = req.with_quota(quota);
        };

        if let Some(recycle_bin_retention_period) = room.recycle_bin_retention_period {
            req = req.with_recycle_bin_retention_period(recycle_bin_retention_period);
        };

        if let Some(admin_ids) = room.admin_ids {
            // todo if admin id is set and different from the current user, add the admin id
            // save the the room id where the script user needs to remove itself as admin
            req = req.with_admin_ids(admin_ids);
        };

        if let Some(admin_group_ids) = room.admin_group_ids {
            req = req.with_admin_group_ids(admin_group_ids);
        };

        if let Some(new_group_member_acceptance) = room.new_group_member_acceptance {
            req = req.with_new_group_member_acceptance(new_group_member_acceptance);
        };

        let req = req.build();



        let created_room: Node = dracoon.create_room(req).await?;

        info!("Creatied room: {} at path: {}", room.name, path);
        term.write_line(&std::format!("Created room: {} at path: {}/", room.name, path))
            .expect("Error writing message to terminal.");

        if let Some(user_permissions) = room.user_permissions {
            let mut user_updates: Vec<RoomUsersAddBatchRequestItem> = vec![];
            for user_permission in user_permissions {
                let user_update = RoomUsersAddBatchRequestItem::new(user_permission.id, user_permission.permissions);
                user_updates.push(user_update);
            } 
            dracoon.update_room_users(created_room.id, user_updates.into()).await?;
        }

        if let Some(group_permissions) = room.group_permissions {
            let mut group_updates: Vec<RoomGroupsAddBatchRequestItem> = vec![];
            for group_permission in group_permissions {
                let group_update = RoomGroupsAddBatchRequestItem::new(group_permission.id, group_permission.permissions, group_permission.new_group_member_acceptance);
                group_updates.push(group_update);
            }
            dracoon.update_room_groups(created_room.id, group_updates.into()).await?;
        }

        info!("Subrooms: {:?}", room.sub_rooms);
        term.write_line(&std::format!("Subrooms: {:?}", room.sub_rooms))
            .expect("Error writing message to terminal.");
      
    
        if let Some(sub_rooms) = room.sub_rooms {
            println!("Subrooms: {:?}", sub_rooms); 
            let new_parent_path = created_room.parent_path.unwrap() + &created_room.name;
            create_rooms_and_subrooms(term, dracoon, created_room.id, sub_rooms, new_parent_path ).await?
        }
    }
    Ok(())
}


